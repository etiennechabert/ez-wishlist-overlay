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
    /// Panel rect — bounds the upgrade panel itself (excluding the world
    /// behind it). Used only for sanity bounds; downstream cropping reads
    /// the named rects below directly.
    pub panel: BBox,
    /// Header block crop — has `<short name>` + `LV<digit>`. The pipeline
    /// reads only the `LV<digit>` token from this rect.
    pub header: BBox,
    /// First row's label area — has the canonical `module.name`.
    pub row_label: BBox,
    /// One rect per cell, left-to-right. Each rect spans the cell's
    /// owned/needed counter region (`~55-88%` down the cell height).
    pub cells: Vec<BBox>,
}

/// Detect the panel. Returns `None` if the "Need to submit items" anchor
/// isn't found (i.e. the screenshot isn't an upgrade panel).
pub fn detect_panel(words: &[OcrWord], img_w: u32, img_h: u32) -> Option<PanelLayout> {
    let words: Vec<Word> = words.iter().map(Word::from).collect();

    let anchor = anchor_by_submit_phrase(img_w, img_h, &words)?;
    let cells = locate_cells_via_from_raid(&words, img_w, img_h);
    if cells.is_empty() {
        tracing::debug!("anchor: 'Need to submit items' found but no FROM/RAID cells located");
        return None;
    }

    // Cells span [icon + name + X/Y]; the owned-count strip is in the
    // bottom ~third. Trim each cell rect to just that strip — matches
    // `read_progress_via_templates` in the ocr_lab pipeline.
    let count_strips: Vec<BBox> = cells.iter().map(count_strip_within_cell).collect();

    // Panel: encloses everything we'll touch. Top of panel is the title
    // band (extrapolated above the anchor), bottom is the FROM RAID row.
    let panel_top = anchor.y.saturating_sub(anchor.h * 18);
    let panel_bottom = cells
        .iter()
        .map(|c| c.y + c.h)
        .max()
        .unwrap_or(img_h.saturating_sub(1))
        .min(img_h);
    let panel_left = anchor.x.saturating_sub(anchor.h * 10).min(img_w);
    let panel_right = (anchor.x + anchor.w + anchor.h * 10).min(img_w);
    let panel = BBox {
        x: panel_left,
        y: panel_top,
        w: panel_right.saturating_sub(panel_left),
        h: panel_bottom.saturating_sub(panel_top),
    };

    // Header rect: top-left corner of panel, two text rows tall (name +
    // LV<digit>). Width ~33% of panel — enough to fit the name + LV.
    // Constants calibrated empirically from `hideout_screenshots/`; tune
    // against native-PNG fixtures once those exist.
    let header_h = (panel.h * 18 / 100).max(anchor.h * 3);
    let header = BBox {
        x: panel.x + panel.w / 40,
        y: panel.y,
        w: panel.w * 35 / 100,
        h: header_h,
    };

    // Row-label rect: starts just below the header, one row tall. The
    // first upgrade row's name text lives here — and (Phase 0) it
    // strictly equals `module.name`.
    let row_label = BBox {
        x: panel.x + panel.w / 25,
        y: panel.y + header.h,
        w: panel.w * 45 / 100,
        h: anchor.h * 2,
    };

    Some(PanelLayout {
        panel,
        header,
        row_label,
        cells: count_strips,
    })
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

/// Narrow a cell rect to just the owned/needed counter strip (~55-88%
/// down the cell height). Matches `read_progress_via_templates` from the
/// ocr_lab pipeline.
fn count_strip_within_cell(cell: &BBox) -> BBox {
    let strip_top = cell.y + cell.h * 55 / 100;
    let strip_bottom = cell.y + cell.h * 88 / 100;
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
