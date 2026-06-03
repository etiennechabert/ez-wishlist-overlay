//! Locate the Facility Upgrade panel from OCR words and derive the crop
//! rectangles the pipeline needs.
//!
//! Strategy (per `we-are-going-to-resilient-unicorn.md` plan):
//!   - "Need to submit items" anchor → panel cross-section (row_y, x extent).
//!   - "FROM RAID" pairs → cell column centres (one per requirement).
//!   - Header rect (top-left of panel): for `LV<digit>` current level only.
//!     Header NAME text is unreliable (e.g. shows "Kitchen" when module is
//!     "Kitchen Area") so we never match against it.
//!   - Row-label rect (just below header, first row's label area):
//!     canonical name, strictly equals `module.name` per Phase 0 walk.
//!
//! Helpers ported from `ocr_lab/src/{grid,pipeline}.rs` on the
//! `add_ocr_data` branch. The earlier code used integer-pixel BBoxes from
//! Tesseract; we convert from float-coord [`OcrWord`] at the boundary so
//! the porting can stay verbatim.

use crate::ocr::OcrWord;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug)]
pub struct PanelLayout {
    /// Header block crop — has `<short name>` + `LV<digit>`. Currently
    /// only consumed by the ignored diagnostic tests in `pipeline.rs`;
    /// runtime parses the `LV<digit>` token directly from the full-image
    /// OCR words.
    #[allow(dead_code)]
    pub header: BBox,
    /// First row's label area — has the canonical `module.name`. Same
    /// story as `header`: kept around for diagnostics; runtime now does
    /// a windowed match against the whole panel's OCR words instead.
    #[allow(dead_code)]
    pub row_label: BBox,
    /// One rect per cell, left-to-right. Each rect spans the cell's
    /// owned/needed counter region (`~55-88%` down the cell height).
    /// Empty when FROM RAID couldn't be located; the pipeline then
    /// derives cells positionally via [`positional_cells`] once it
    /// knows `n` from the resolved upgrade.
    pub cells: Vec<BBox>,
    /// "Need to submit items" anchor box (the source of every other
    /// rect on this layout). Kept around so [`positional_cells`] can
    /// derive a fallback cell row when FROM RAID isn't readable.
    pub anchor: BBox,
    pub img_w: u32,
    pub img_h: u32,
}

/// Detect the panel. Returns `None` if the "Need to submit items" anchor
/// isn't found (i.e. the screenshot isn't an upgrade panel).
///
/// `cells` is populated from `FROM RAID` anchors when those are readable;
/// otherwise it's left empty and the pipeline derives positional cells
/// later, once `requirements.len()` is known from the matched upgrade
/// ([`positional_cells`]). Low-res screenshots (the JPG fixtures, and
/// any future captures where the FROM RAID labels render too small) go
/// through the positional path; native-resolution captures use FROM RAID.
pub fn detect_panel(words: &[OcrWord], img_w: u32, img_h: u32) -> Option<PanelLayout> {
    let words: Vec<Word> = words.iter().map(Word::from).collect();

    let anchor = anchor_by_submit_phrase(img_w, img_h, &words)?;
    let cells_full = locate_cells_via_from_raid(&words, img_w, img_h);
    let count_strips: Vec<BBox> = cells_full.iter().map(count_strip_within_cell).collect();
    if cells_full.is_empty() {
        tracing::debug!(
            "anchor: FROM/RAID not detected; pipeline will derive positional cells once \
             the upgrade is identified",
        );
    }

    // Panel bounds. The "Need to submit items" phrase sits centred
    // horizontally inside the panel and takes up roughly 30% of the
    // panel width — so panel_w ≈ anchor.w * 10 / 3 — and the panel
    // extends ~18 anchor heights upward (title + 3-4 upgrade rows +
    // cost) and ~6 anchor heights downward (cost + cells + buttons).
    // Both ratios were measured against the native PNGs in
    // `screenshots/hideout/`.
    let anchor_cx = anchor.x + anchor.w / 2;
    let panel_w_est = anchor.w * 10 / 3;
    let panel_left = anchor_cx.saturating_sub(panel_w_est / 2).min(img_w);
    let panel_right = (anchor_cx + panel_w_est / 2).min(img_w);
    let panel_top = anchor.y.saturating_sub(anchor.h * 18);
    let panel_bottom = cells_full
        .iter()
        .map(|c| c.y + c.h)
        .max()
        // No FROM RAID: assume the cell strip extends down ~6 anchor
        // heights from the "Need to submit items" line.
        .unwrap_or(anchor.y + anchor.h * 6)
        .min(img_h);
    let panel_w = panel_right.saturating_sub(panel_left);
    let panel_h = panel_bottom.saturating_sub(panel_top);

    // Header rect: top-left corner of panel, two text rows tall (name +
    // LV<digit>). Width ~33% of panel — enough to fit the name + LV.
    let header_h = (panel_h * 18 / 100).max(anchor.h * 3);
    let header = BBox {
        x: panel_left + panel_w / 40,
        y: panel_top,
        w: panel_w * 35 / 100,
        h: header_h,
    };

    // Row-label rect: starts just below the header, two text-rows tall.
    // The first upgrade row's name text lives here — and (Phase 0) it
    // strictly equals `module.name`.
    let row_label = BBox {
        x: panel_left + panel_w / 25,
        y: panel_top + header.h,
        w: panel_w * 45 / 100,
        h: anchor.h * 2,
    };

    Some(PanelLayout {
        header,
        row_label,
        cells: count_strips,
        anchor,
        img_w,
        img_h,
    })
}

/// Fallback when FROM RAID couldn't be located: lay out N equal-width
/// count-strip rects across the panel. The strip's Y is derived from
/// the OCR'd item-name row (more robust than fixed anchor.h multipliers
/// — anchor.h varies 30-50% across captures depending on head tilt /
/// panel zoom, while the item name → count gap is always one text-row
/// regardless of capture scale).
///
/// If the OCR caught no item-name words (rare, but possible if the
/// item names render too small at extreme HMD distances), the function
/// falls back to anchor-relative offsets.
pub fn positional_cells(layout: &PanelLayout, words: &[OcrWord], n: usize) -> Vec<BBox> {
    let anchor = layout.anchor;
    let img_w = layout.img_w;
    let img_h = layout.img_h;

    // Panel left/right: "Need to submit items" sits centred in the panel
    // and is ~30% of the panel's width, so panel_w ≈ anchor.w * 10/3.
    let anchor_cx = anchor.x + anchor.w / 2;
    let panel_w_est = anchor.w * 10 / 3;
    let panel_left = anchor_cx.saturating_sub(panel_w_est / 2).min(img_w);
    let panel_right = (anchor_cx + panel_w_est / 2).min(img_w);
    let panel_w = panel_right.saturating_sub(panel_left);

    // Y range for the digit row.
    //
    // The digit row sits one text-row above FROM RAID and one
    // text-row below the item-name row. We try three signals in
    // descending order of reliability:
    //
    //   1. **N/M tokens**. The OCR engine *sometimes* recognises one
    //      of the count cells as "4/3" / "0/3" / etc. — even one
    //      such token is gold: its Y *is* the digit row's Y, no
    //      heuristics needed. Drives all four cells off that.
    //   2. **FROM/RAID labels**. Detected reliably on native captures
    //      at typical panel scales. Digit row sits ~1 text-row above.
    //   3. **Item-name max-bottom + median text height**. Last resort
    //      when neither of the above land — historically what we used.
    //      We now also exclude any N/M-looking token from the
    //      item-name set, otherwise it poisons `max_bottom` and the
    //      strip lands a row below where it should.
    let in_panel_band = |w: &&OcrWord| {
        let wy = w.rect.y as u32;
        let wx = w.rect.x as u32;
        wy > anchor.y + anchor.h
            && wy < anchor.y + anchor.h * 12
            && wx >= panel_left
            && wx <= panel_right
    };
    let looks_like_count = |w: &&OcrWord| {
        // Match "N/M" shape (after stripping non-digit non-slash chars
        // to absorb OCR noise like "/5" being tagged as "I5" with a
        // bar mistaken for a slash, etc.). At least one digit either
        // side of the slash.
        let cleaned: String = w
            .text
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '/')
            .collect();
        if let Some((a, b)) = cleaned.split_once('/') {
            !a.is_empty() && !b.is_empty()
        } else {
            false
        }
    };

    let count_tokens: Vec<&OcrWord> = words
        .iter()
        .filter(in_panel_band)
        .filter(looks_like_count)
        .collect();

    let from_y_band: Vec<u32> = words
        .iter()
        .filter(in_panel_band)
        .filter(|w| {
            let up = w.text.to_ascii_uppercase();
            matches!(up.as_str(), "FROM" | "RAID")
        })
        .map(|w| w.rect.y as u32)
        .collect();

    let (count_top, count_bottom) = if !count_tokens.is_empty() {
        let top = count_tokens
            .iter()
            .map(|w| w.rect.y as u32)
            .min()
            .unwrap_or(0);
        let bot = count_tokens
            .iter()
            .map(|w| (w.rect.y + w.rect.height) as u32)
            .max()
            .unwrap_or(top);
        let max_h = count_tokens
            .iter()
            .map(|w| w.rect.height as u32)
            .max()
            .unwrap_or(20);
        // Small pad above/below so we don't clip the glyphs themselves.
        let pad = (max_h / 4).max(2);
        (top.saturating_sub(pad), bot.saturating_add(pad).min(img_h))
    } else if !from_y_band.is_empty() {
        let from_y_min = *from_y_band.iter().min().unwrap();
        // FROM RAID glyphs are ~the same height as the count digits in
        // every fixture. Use a fixed fraction of anchor.h as the per-
        // glyph reference and span ~1.5 of those above the FROM line.
        let glyph_h = anchor.h.max(8);
        let gap = glyph_h / 3;
        let bottom = from_y_min.saturating_sub(gap).min(img_h);
        let top = bottom.saturating_sub(glyph_h * 3 / 2).min(img_h);
        (top, bottom)
    } else {
        // Item-name fallback. Item-name words sit between the cost line
        // (~2 anchor heights below the anchor) and the buttons (~10
        // anchor heights below). Filter by Y range, by X inside the
        // panel, drop button + count tokens.
        let name_band_top = anchor.y + anchor.h * 2;
        let name_band_bottom = anchor.y + anchor.h * 12;
        let names: Vec<&OcrWord> = words
            .iter()
            .filter(|w| {
                let wy = w.rect.y as u32;
                let wx = w.rect.x as u32;
                wy > name_band_top && wy < name_band_bottom && wx >= panel_left && wx <= panel_right
            })
            .filter(|w| {
                let up = w.text.to_ascii_uppercase();
                !matches!(up.as_str(), "BACK" | "LEVEL" | "UP" | "FROM" | "RAID")
            })
            .filter(|w| !looks_like_count(w))
            .collect();

        if !names.is_empty() {
            let max_bottom = names
                .iter()
                .map(|w| (w.rect.y + w.rect.height) as u32)
                .max()
                .unwrap();
            let mut heights: Vec<u32> = names.iter().map(|w| w.rect.height as u32).collect();
            heights.sort_unstable();
            let median_h = heights[heights.len() / 2].max(8);
            let top = (max_bottom + median_h / 2).min(img_h);
            let bottom = (top + median_h * 3).min(img_h);
            (top, bottom)
        } else {
            let top = (anchor.y + anchor.h * 11 / 2).min(img_h);
            let bottom = (anchor.y + anchor.h * 15 / 2).min(img_h);
            (top, bottom)
        }
    };

    let mut out = Vec::with_capacity(n);
    if n == 0 || panel_w == 0 {
        return out;
    }
    let usable_left = panel_left + panel_w / 30;
    let usable_w = panel_w - panel_w * 2 / 30;
    let pitch = usable_w / n as u32;
    for i in 0..n {
        let left = usable_left + pitch * i as u32 + pitch / 10;
        let width = pitch.saturating_sub(pitch / 5);
        out.push(BBox {
            x: left,
            y: count_top,
            w: width,
            h: count_bottom.saturating_sub(count_top),
        });
    }
    out
}

/// Word with integer-pixel coordinates — a more convenient view of
/// [`OcrWord`] for the ported helpers below.
#[derive(Clone, Debug)]
struct Word {
    text: String,
    bbox: BBox,
}

impl From<&OcrWord> for Word {
    fn from(w: &OcrWord) -> Self {
        Word {
            text: w.text.clone(),
            bbox: BBox {
                x: w.rect.x.round().max(0.0) as u32,
                y: w.rect.y.round().max(0.0) as u32,
                w: w.rect.width.round().max(0.0) as u32,
                h: w.rect.height.round().max(0.0) as u32,
            },
        }
    }
}

/// Find "Need to submit items" and return a bounding box covering the
/// whole panel. Ported from `pipeline::anchor_by_submit_phrase`.
fn anchor_by_submit_phrase(_img_w: u32, _img_h: u32, words: &[Word]) -> Option<BBox> {
    let row: Vec<&Word> = words
        .iter()
        .filter(|w| {
            matches!(
                w.text.to_ascii_lowercase().as_str(),
                "need" | "to" | "submit" | "items"
            )
        })
        .collect();
    if row.len() < 2 {
        return None;
    }
    let median_y = median_of(row.iter().map(|w| w.bbox.y));
    let median_h = median_of(row.iter().map(|w| w.bbox.h)).max(10);
    let on_row: Vec<&Word> = row
        .into_iter()
        .filter(|w| (w.bbox.y as i32 - median_y as i32).abs() < median_h as i32)
        .collect();
    if on_row.len() < 2 {
        return None;
    }
    let left = on_row.iter().map(|w| w.bbox.x).min().unwrap();
    let right = on_row.iter().map(|w| w.bbox.x + w.bbox.w).max().unwrap();
    let top = on_row.iter().map(|w| w.bbox.y).min().unwrap();
    let bottom = on_row.iter().map(|w| w.bbox.y + w.bbox.h).max().unwrap();
    Some(BBox {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top).max(median_h),
    })
}

/// Pair up "FROM" / "RAID" tokens per cell and derive cell rects.
/// Ported from `pipeline::locate_cells_via_from_raid`.
fn locate_cells_via_from_raid(words: &[Word], pw: u32, ph: u32) -> Vec<BBox> {
    let froms: Vec<&Word> = words.iter().filter(|w| looks_like_from(&w.text)).collect();
    let raids: Vec<&Word> = words.iter().filter(|w| looks_like_raid(&w.text)).collect();

    let mut pairs: Vec<(&Word, &Word)> = Vec::new();
    for f in &froms {
        let mid_y = f.bbox.y as i32 + f.bbox.h as i32 / 2;
        let f_right = (f.bbox.x + f.bbox.w) as i32;
        let cand = raids
            .iter()
            .filter(|r| {
                let r_mid_y = r.bbox.y as i32 + r.bbox.h as i32 / 2;
                (r_mid_y - mid_y).abs() < f.bbox.h as i32
            })
            .filter(|r| {
                let dx = (r.bbox.x as i32) - f_right;
                dx > -((f.bbox.w as i32) / 2) && dx < (f.bbox.w as i32) * 3
            })
            .min_by_key(|r| (r.bbox.x as i32 - f_right).abs());
        if let Some(r) = cand {
            pairs.push((f, r));
        }
    }
    if pairs.is_empty() {
        tracing::debug!("locate_cells: no FROM/RAID pairs in OCR words");
        return Vec::new();
    }
    pairs.sort_by_key(|(f, _)| f.bbox.x);
    pairs.dedup_by_key(|(f, _)| f.bbox.x);

    let cell_top = pairs.iter().map(|(f, _)| f.bbox.y).min().unwrap();
    let cell_h = pairs
        .iter()
        .map(|(f, r)| (f.bbox.y + f.bbox.h).max(r.bbox.y + r.bbox.h) - f.bbox.y)
        .max()
        .unwrap();
    // Each cell extends UP from the FROM RAID strip by ~5x text-height to
    // capture icon + name + X/Y progress row.
    let cell_full_top = cell_top.saturating_sub(cell_h * 5);
    let cell_full_bottom = cell_top + cell_h;

    let mut centres: Vec<u32> = pairs
        .iter()
        .map(|(f, r)| (f.bbox.x + (r.bbox.x + r.bbox.w)) / 2)
        .collect();
    centres.sort_unstable();
    let pitch = if centres.len() >= 2 {
        let gaps: Vec<u32> = centres.windows(2).map(|w| w[1] - w[0]).collect();
        median_of(gaps.into_iter())
    } else {
        (cell_h * 7).max(40)
    };

    let mut cells = Vec::new();
    for (i, &c) in centres.iter().enumerate() {
        let left = if i == 0 {
            c.saturating_sub(pitch / 2)
        } else {
            (centres[i - 1] + c) / 2
        };
        let right = if i + 1 < centres.len() {
            (c + centres[i + 1]) / 2
        } else {
            c + pitch / 2
        };
        cells.push(clamp_bbox(
            pw,
            ph,
            left as i32,
            cell_full_top as i32,
            right as i32,
            cell_full_bottom as i32,
        ));
    }
    cells
}

/// Narrow a cell rect to just the owned/needed counter strip.
///
/// The cell rect from [`locate_cells_via_from_raid`] is built as
/// `cell.y = from.y - 5*from.h` and `cell.h = 6*from.h`, so the
/// bottom of the cell aligns with the bottom of the FROM RAID label.
/// The digit row sits exactly one text-row above FROM RAID,
/// regardless of how compact the panel is in a given capture — the
/// game UI layout itself is fixed; only the panel's pixel scale
/// varies with head distance.
///
/// Anchoring the strip to FROM RAID (via `from_h`) instead of
/// percentages of cell.h means the strip lands on the digit row
/// across every fixture. The earlier 55-88% range extended past
/// `cell.y + cell.h - from_h` into the FROM RAID glyphs, which fed
/// "F R O M  R A I D" letters into the template matcher and made
/// many fixtures silently read every count as 0.
fn count_strip_within_cell(cell: &BBox) -> BBox {
    let from_h = (cell.h / 6).max(8);
    let gap = from_h / 4;
    let from_top = (cell.y + cell.h).saturating_sub(from_h);
    let strip_bottom = from_top.saturating_sub(gap);
    let strip_top = strip_bottom.saturating_sub(from_h * 14 / 10);
    BBox {
        x: cell.x,
        y: strip_top,
        w: cell.w,
        h: strip_bottom.saturating_sub(strip_top),
    }
}

fn looks_like_from(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    up == "FROM" || up == "FROM." || up == "FRDM" || up == "FROW"
}

fn looks_like_raid(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    up == "RAID" || up == "RAID." || up == "RAIO" || up == "RAID:"
}

fn clamp_bbox(img_w: u32, img_h: u32, x0: i32, y0: i32, x1: i32, y1: i32) -> BBox {
    let x0 = x0.max(0) as u32;
    let y0 = y0.max(0) as u32;
    let x1 = (x1.max(0) as u32).min(img_w);
    let y1 = (y1.max(0) as u32).min(img_h);
    BBox {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

fn median_of<I: Iterator<Item = u32>>(it: I) -> u32 {
    let mut v: Vec<u32> = it.collect();
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Parse a level token (`"LV0"`, `"Lv1"`, `"LVO"`, …) into the integer.
/// Tolerant of common OCR letter-vs-digit confusions: `O` → `0`, `I` → `1`.
/// Returns `None` if the token doesn't match the shape.
pub fn parse_level_token(text: &str) -> Option<u32> {
    let upper = text
        .to_ascii_uppercase()
        .replace('O', "0")
        .replace('I', "1");
    if !upper.starts_with("LV") || upper.len() < 3 {
        return None;
    }
    let digits = &upper[2..];
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{OcrRect, OcrWord};

    fn word(text: &str, x: f32, y: f32, w: f32, h: f32) -> OcrWord {
        OcrWord {
            text: text.into(),
            rect: OcrRect {
                x,
                y,
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn parse_level_token_handles_oi_substitutions() {
        assert_eq!(parse_level_token("LV0"), Some(0));
        assert_eq!(parse_level_token("Lv1"), Some(1));
        assert_eq!(parse_level_token("LVO"), Some(0)); // O misread for 0
        assert_eq!(parse_level_token("LVI"), Some(1)); // I misread for 1
        assert_eq!(parse_level_token("LV10"), Some(10));
        assert_eq!(parse_level_token("Level"), None);
        assert_eq!(parse_level_token("LV"), None);
        assert_eq!(parse_level_token("LVAB"), None);
    }

    #[test]
    fn no_panel_when_anchor_missing() {
        let words = vec![
            word("Hello", 0.0, 0.0, 100.0, 30.0),
            word("World", 100.0, 0.0, 100.0, 30.0),
        ];
        assert!(detect_panel(&words, 1000, 1000).is_none());
    }

    #[test]
    fn anchor_found_with_two_phrase_words() {
        // "Need" + "submit" on the same Y row, plus FROM/RAID pairs.
        let words = vec![
            word("Need", 400.0, 500.0, 60.0, 20.0),
            word("submit", 500.0, 500.0, 80.0, 20.0),
            word("FROM", 200.0, 600.0, 40.0, 15.0),
            word("RAID", 250.0, 600.0, 40.0, 15.0),
            word("FROM", 400.0, 600.0, 40.0, 15.0),
            word("RAID", 450.0, 600.0, 40.0, 15.0),
        ];
        let layout = detect_panel(&words, 1000, 1000).expect("panel detected");
        assert_eq!(layout.cells.len(), 2);
        assert!(layout.cells[0].x < layout.cells[1].x, "cells sorted L→R");
    }
}
