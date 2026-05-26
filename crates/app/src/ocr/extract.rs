//! Two-pass extraction orchestrator.
//!
//! Pass 1: OCR the full screenshot at native scale → reliably reads
//! large text (item names, title, cost). Parser identifies cell layout
//! (per-cell X-ranges) from the result.
//!
//! Pass 2: For each detected cell, crop a generous "progress strip"
//! around where the X/Y digits live, preprocess (upscale + binarize),
//! and OCR just that strip. The cell-progress digits read reliably at
//! this scale + contrast where the full-image pass misses them.
//!
//! The two passes share an in-memory copy of the source image — we only
//! decode from disk once.

use crate::ocr::{
    engine, parse,
    parse::{CapturedItem, CapturedUpgrade},
    preprocess, OcrResult, OcrWord,
};
use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView};
use std::path::Path;

/// Top-level entry: decode the screenshot, run both OCR passes, return
/// the structured upgrade entry with per-cell collected/needed
/// populated where the second pass succeeded.
///
/// Also returns the *raw* first-pass OCR text so the caller can
/// persist it in the wishlist entry for later re-parsing.
pub fn extract_upgrade(path: &Path) -> Result<(CapturedUpgrade, String)> {
    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    let first = engine::recognize_image(&img)?;
    let raw_text = first.text.clone();

    let mut upgrade = parse::parse_upgrade(&first)
        .context("first-pass OCR didn't look like an upgrade panel")?;

    // Locate the cells in the source image, then re-OCR each one.
    let cell_rects = detect_cell_rects(&first, &upgrade);
    for (idx, item) in upgrade.items.iter_mut().enumerate() {
        let Some(rect) = cell_rects.get(idx) else {
            continue;
        };
        match read_cell_progress(&img, rect, path, idx) {
            Ok(SecondPass { fraction, raw_text }) => {
                tracing::info!(
                    cell = idx,
                    name = %item.name,
                    rect = ?(rect.x, rect.y, rect.w, rect.h),
                    found = ?fraction,
                    raw = %raw_text.replace('\n', " | "),
                    "second-pass result",
                );
                if let Some((collected, needed)) = fraction {
                    item.collected = Some(collected);
                    item.needed = Some(needed);
                }
            }
            Err(e) => {
                tracing::warn!(cell = idx, error = %e, "second-pass OCR failed");
            }
        }
    }

    Ok((upgrade, raw_text))
}

/// Pixel rect to crop for one cell's progress strip. Derived from the
/// cell's text box found in the first-pass OCR — we widen the X range
/// to the natural cell width (gap to next cell) and put the Y window
/// at the bottom third of the cell (where the bar + digits live).
#[derive(Debug, Clone, Copy)]
struct CellRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn detect_cell_rects(ocr: &OcrResult, upgrade: &CapturedUpgrade) -> Vec<CellRect> {
    // The parser knows the *count* of cells and their names; we need
    // to recover their pixel positions. Walk the OCR words, find the
    // ones whose joined text matches each item name, take the union of
    // their bounding rects as the "name band" for that cell. The
    // progress strip lives below that band.
    let mut rects = Vec::with_capacity(upgrade.items.len());
    let mut cursor = 0usize; // search start in the word stream

    for item in &upgrade.items {
        if let Some((rect, advance)) = find_phrase_rect(&ocr.words, &item.name, cursor) {
            // Align the strip with the NAME's bounding box: same X
            // origin, same width, extending down for ~3× the name's
            // height. The progress digits in-game live directly below
            // the name (the bar's center column aligns with the name's
            // center column), so this catches them without any
            // estimate-the-cell-pitch math. Adds modest left/right
            // padding so a slight misalignment doesn't clip the
            // outermost digit.
            let pad = rect.w / 8;
            let strip_x = rect.x.saturating_sub(pad);
            let strip_w = rect.w + 2 * pad;
            let strip_y = rect.y + rect.h;
            let strip_h = rect.h * 3;

            rects.push(CellRect {
                x: strip_x,
                y: strip_y,
                w: strip_w,
                h: strip_h,
            });
            cursor = advance;
        } else {
            tracing::debug!(name = %item.name, "could not locate cell name in OCR words");
        }
    }
    rects
}


/// Search `words` starting at index `from` for the first run of words
/// whose joined text matches `phrase` (case-insensitive, whitespace-
/// collapsed). Returns the union rect + the index after the matched
/// run for the next search.
fn find_phrase_rect(words: &[OcrWord], phrase: &str, from: usize) -> Option<(CellRect, usize)> {
    let target: Vec<String> = phrase
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if target.is_empty() {
        return None;
    }
    let n = target.len();
    let mut i = from;
    while i + n <= words.len() {
        let matches = (0..n).all(|j| words[i + j].text.eq_ignore_ascii_case(&target[j]));
        if matches {
            let rect = union_rect(&words[i..i + n]);
            return Some((rect, i + n));
        }
        i += 1;
    }
    None
}

fn union_rect(words: &[OcrWord]) -> CellRect {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for w in words {
        min_x = min_x.min(w.rect.x);
        min_y = min_y.min(w.rect.y);
        max_x = max_x.max(w.rect.right());
        max_y = max_y.max(w.rect.bottom());
    }
    CellRect {
        x: min_x.max(0.0) as u32,
        y: min_y.max(0.0) as u32,
        w: (max_x - min_x).max(1.0) as u32,
        h: (max_y - min_y).max(1.0) as u32,
    }
}

/// Output of a single per-cell second-pass attempt. `raw_text` is the
/// recognized text from the cropped + preprocessed strip — always
/// captured for diagnostics, regardless of whether we managed to
/// extract a fraction from it.
struct SecondPass {
    fraction: Option<(u32, u32)>,
    raw_text: String,
}

/// Crop, preprocess, second-pass OCR. Saves the preprocessed crop next
/// to the source screenshot in debug builds (`<stem>.cell-N.png`) so we
/// can inspect what the engine actually sees.
fn read_cell_progress(
    img: &DynamicImage,
    rect: &CellRect,
    source_path: &Path,
    cell_idx: usize,
) -> Result<SecondPass> {
    let (img_w, img_h) = img.dimensions();
    if rect.x >= img_w || rect.y >= img_h {
        return Ok(SecondPass {
            fraction: None,
            raw_text: String::new(),
        });
    }

    let preprocessed = preprocess::crop_upscale(img, rect.x, rect.y, rect.w, rect.h, 4)
        .context("preprocess cell crop")?;

    // Dump each preprocessed crop next to the source screenshot so we
    // can eyeball what the engine is reading. Cheap (the file is tiny
    // after binarization) and only happens during a re-OCR, not on
    // every live capture.
    save_debug_crop(&preprocessed, source_path, cell_idx);

    let second = engine::recognize_image(&preprocessed)?;
    let fraction = parse_fraction(&second.text);
    Ok(SecondPass {
        fraction,
        raw_text: second.text,
    })
}

fn save_debug_crop(img: &DynamicImage, source: &Path, cell_idx: usize) {
    let Some(parent) = source.parent() else { return };
    let Some(stem) = source.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let debug_dir = parent.join("ocr-debug");
    if let Err(e) = std::fs::create_dir_all(&debug_dir) {
        tracing::debug!(error = %e, "could not create ocr-debug/ next to screenshot");
        return;
    }
    let out = debug_dir.join(format!("{stem}.cell-{cell_idx}.png"));
    if let Err(e) = img.save(&out) {
        tracing::debug!(error = %e, "debug crop save failed");
    }
}

/// Pull the first `<digits>/<digits>` token out of an OCR text dump.
/// Tolerates two common failure modes seen on the in-game progress
/// font:
///
/// 1. **Slash misread as a vertical bar / l / I / `|`** — normalize to
///    `/` before scanning.
/// 2. **Slash completely dropped, leaving an all-digits run.** The
///    diagonal of the `/` is so thin at this rendering size that
///    Windows OCR sometimes reads it as a `1` (e.g. `"1/3"` → `"113"`)
///    or omits it entirely (`"13"`). For 2- or 3-digit all-digit
///    tokens we recover by assuming the slash lives at the conventional
///    middle position and the surviving digits are the X and Y.
fn parse_fraction(text: &str) -> Option<(u32, u32)> {
    // Pass 1: explicit slash, with the obvious mis-reads normalized to '/'.
    let normalized: String = text
        .chars()
        .map(|c| match c {
            'I' | 'l' | '|' => '/',
            _ => c,
        })
        .collect();
    for token in normalized.split_whitespace() {
        if let Some((a, b)) = token.split_once('/') {
            let a = a.trim_matches(|c: char| !c.is_ascii_digit());
            let b = b.trim_matches(|c: char| !c.is_ascii_digit());
            if let (Ok(collected), Ok(needed)) = (a.parse::<u32>(), b.parse::<u32>()) {
                if needed > 0 {
                    return Some((collected, needed));
                }
            }
        }
    }

    // Pass 2: fallback for the "slash got dropped/absorbed" case.
    // Empirical: Windows OCR returns "113" for "1/3" and "13" for the
    // same when the slash collapsed entirely. The crop is tight on the
    // progress region so any digits we see SHOULD belong to a fraction.
    for token in text.split_whitespace() {
        let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
        let bytes = digits.as_bytes();
        let (x, y) = match bytes.len() {
            // "13" → 1/3 — both digits, no surviving slash.
            2 => ((bytes[0] - b'0') as u32, (bytes[1] - b'0') as u32),
            // "113" → 1/3 — middle byte was the misread slash.
            3 => ((bytes[0] - b'0') as u32, (bytes[2] - b'0') as u32),
            _ => continue,
        };
        // Hideout progress is always X ≤ Y and Y is a small natural
        // number in practice — bail on anything that violates this so
        // we don't return false positives on stray numbers.
        if y > 0 && x <= y && y <= 100 {
            return Some((x, y));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_parses_clean_token() {
        assert_eq!(parse_fraction("1/3"), Some((1, 3)));
        assert_eq!(parse_fraction("foo 3/3 bar"), Some((3, 3)));
    }

    #[test]
    fn fraction_normalizes_slash_misreads() {
        // OCR reading "1/3" as "1l3" because the slash looked like an l.
        assert_eq!(parse_fraction("1l3"), Some((1, 3)));
        // Or as a vertical bar.
        assert_eq!(parse_fraction("0|5"), Some((0, 5)));
    }

    #[test]
    fn fraction_rejects_zero_denominator() {
        assert_eq!(parse_fraction("0/0"), None);
    }

    #[test]
    fn fraction_none_when_no_token() {
        assert_eq!(parse_fraction("Boxed Nails FROM RAID"), None);
    }

    #[test]
    fn fraction_recovers_dropped_slash() {
        // Common Windows OCR failure: "1/3" becomes "113" because the
        // slash gets misread as a vertical-bar-shaped 1.
        assert_eq!(parse_fraction("113"), Some((1, 3)));
        assert_eq!(parse_fraction("Boxed 113 RAID"), Some((1, 3)));
        // And entirely collapsed: "1/3" → "13".
        assert_eq!(parse_fraction("13"), Some((1, 3)));
        assert_eq!(parse_fraction("38"), Some((3, 8)));
    }

    #[test]
    fn fraction_rejects_x_greater_than_y() {
        // "531" interpreted as 5/31 is X<Y → valid... actually 5/31 IS
        // legal in our heuristic. To get a true X>Y rejection use a
        // 3-digit token like "521" → 5/1 → rejected.
        assert_eq!(parse_fraction("521"), None);
        // 2-digit "53" → 5/3 → X>Y → rejected.
        assert_eq!(parse_fraction("53"), None);
    }

    #[test]
    fn fraction_accepts_completed_cells() {
        assert_eq!(parse_fraction("99"), Some((9, 9)));
        assert_eq!(parse_fraction("88"), Some((8, 8)));
    }
}
