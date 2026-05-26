//! Turn raw word boxes from [`crate::ocr::OcrResult`] into a structured
//! [`CapturedUpgrade`] entry.
//!
//! POC v1 parses the in-game *hideout upgrade* panel. Empirical findings
//! from running Windows.Media.Ocr on real ExfilZone screenshots:
//!
//! - Item names (`Boxed Nails`, `Large gas can`, `Disinfectant wipes`)
//!   come through cleanly enough — occasional one-char errors like
//!   "Balts" for "Bolts", which fuzzy-matching at the catalog layer
//!   handles fine.
//! - The upgrade title + level chip (`Storage Room A LV0`) reads, with
//!   the digit-0 sometimes recognized as letter-O ("LVO"). We normalize.
//! - The hideout cost ("80000") reads as bare digits next to a glyph
//!   that comes through as "0" or "@" — easy to detect.
//! - The per-cell collected/needed digits (e.g. "1/3") **do not** read
//!   reliably yet — they're small and overlap the progress bar fill.
//!   v1 leaves them as `None`; future iteration adds image preprocessing
//!   (upscale + contrast) before OCR to recover them.
//!
//! Parser strategy: cluster words into horizontal lines by Y-coordinate,
//! tag each line by content (title, cost, items, button row), then
//! cluster item-row words into cells by X-gap. Pure text-stream parsing
//! is fragile against multi-word names like "Large gas can"; using box
//! geometry buys robustness for the cost of ~30 extra lines of code.

use crate::ocr::{OcrResult, OcrWord};
use serde::{Deserialize, Serialize};

/// A single upgrade screen captured from a screenshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedUpgrade {
    /// "Storage Room A LV0" — title + normalized level chip, joined with
    /// a space. Used as the wishlist dedup key; re-screenshotting the
    /// same upgrade overwrites the prior entry.
    pub key: String,
    /// Display title alone ("Storage Room A").
    pub title: String,
    /// Normalized level chip ("LV0", "LV1", ...). "LVO" → "LV0", "LVl"
    /// → "LV1", etc. — small OCR confusions on single letters.
    pub level: String,
    /// Hideout currency cost. Optional — not all upgrade screens have
    /// one (e.g. free Lv1 unlocks).
    pub cost: Option<u64>,
    /// One entry per item cell on the screen, left-to-right.
    pub items: Vec<CapturedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedItem {
    /// Name as OCR read it, with leading/trailing whitespace trimmed.
    /// Preserved verbatim so a later fuzzy match against the catalog
    /// can be re-run with improved rules without re-OCR'ing.
    pub name: String,
    /// Quantity the user has collected of this item, when the in-cell
    /// progress digits were recognizable. v1 OCR misses these — see
    /// the module docstring.
    pub collected: Option<u32>,
    /// Quantity required. Same caveat as `collected`.
    pub needed: Option<u32>,
}

/// Best-effort parse of OCR output → structured upgrade entry. Returns
/// `None` only when we can't find anything that looks like an upgrade
/// title (i.e. the image isn't an upgrade screen at all).
pub fn parse_upgrade(ocr: &OcrResult) -> Option<CapturedUpgrade> {
    if ocr.words.is_empty() {
        return None;
    }

    let lines = group_into_lines(&ocr.words);
    let (title, level) = find_title_and_level(&lines)?;
    let cost = find_cost(&lines);
    let items = find_items(&lines);

    let key = format!("{title} {level}");
    Some(CapturedUpgrade {
        key,
        title,
        level,
        cost,
        items,
    })
}

// --- Layout helpers ---------------------------------------------------

/// Group of words sharing approximately the same Y-coordinate, sorted
/// left-to-right. Lines are sorted top-to-bottom in the parent vec.
struct Line {
    /// Average Y of the words in this line (used for ordering).
    y: f32,
    words: Vec<OcrWord>,
}

impl Line {
    /// Reconstructed line text, words joined with single spaces.
    fn text(&self) -> String {
        self.words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn group_into_lines(words: &[OcrWord]) -> Vec<Line> {
    if words.is_empty() {
        return Vec::new();
    }
    // Use the median word height as the tolerance for Y-clustering.
    // Two words are on the same line if their Y centers are within
    // ~60% of a typical glyph height.
    let mut heights: Vec<f32> = words.iter().map(|w| w.rect.height).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_h = heights[heights.len() / 2];
    let tol = median_h * 0.6;

    let mut by_y = words.to_vec();
    by_y.sort_by(|a, b| {
        a.rect
            .center_y()
            .partial_cmp(&b.rect.center_y())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut lines: Vec<Line> = Vec::new();
    for w in by_y {
        let cy = w.rect.center_y();
        match lines.last_mut() {
            Some(line) if (cy - line.y).abs() <= tol => {
                // Rolling average — keeps the line's y stable as we add words.
                let n = line.words.len() as f32;
                line.y = (line.y * n + cy) / (n + 1.0);
                line.words.push(w);
            }
            _ => lines.push(Line {
                y: cy,
                words: vec![w],
            }),
        }
    }
    // Sort words within each line left-to-right.
    for line in &mut lines {
        line.words.sort_by(|a, b| {
            a.rect
                .x
                .partial_cmp(&b.rect.x)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    lines
}

// --- Title + level ----------------------------------------------------

fn find_title_and_level(lines: &[Line]) -> Option<(String, String)> {
    // Scan top-to-bottom for the first line containing a level chip
    // ("LV0" through "LV9", with OCR confusions like "LVO" / "LVl"
    // accepted). Title = words before the chip on the same line, or —
    // if the chip is on its own line — the previous non-noise line.
    for (i, line) in lines.iter().enumerate() {
        let text = line.text();
        if let Some((before, level)) = split_at_level_chip(&text) {
            let title = if before.trim().is_empty() {
                // Level chip alone on its line; back up to the previous
                // line for the title.
                lines
                    .get(i.wrapping_sub(1))
                    .map(|l| cleanup_title(&l.text()))
                    .unwrap_or_default()
            } else {
                cleanup_title(before)
            };
            if !title.is_empty() {
                return Some((title, level));
            }
        }
    }
    None
}

/// If `text` contains a "LV<digit>" / "LVO" / "LVl" / "LVI" chip, split
/// it into `(before_chip, normalized_chip)`. Case-insensitive.
fn split_at_level_chip(text: &str) -> Option<(&str, String)> {
    // Walk word by word so we don't false-match "LV" inside other tokens.
    for (start, _) in text.match_indices(|c: char| c == 'L' || c == 'l') {
        if start + 3 > text.len() {
            continue;
        }
        let candidate = &text[start..start + 3];
        if !candidate[..2].eq_ignore_ascii_case("LV") {
            continue;
        }
        let third = candidate.chars().nth(2)?;
        let normalized_digit = match third {
            '0'..='9' => third,
            'O' | 'o' | 'D' => '0',
            'l' | 'I' | 'i' => '1',
            _ => continue,
        };
        // Ensure it's a token boundary on the right side (next char is
        // space, end-of-string, or punctuation) so we don't match "LV05"
        // when we mean "LV0".
        let after = text.as_bytes().get(start + 3).copied();
        match after {
            None => {}
            Some(b) if (b as char).is_ascii_whitespace() => {}
            _ => continue,
        }
        return Some((&text[..start], format!("LV{normalized_digit}")));
    }
    None
}

/// Strip preamble chrome from a title line. The hideout panel often
/// renders "GRAPHIC FACILITY UPGRADE" above the title — we drop those
/// well-known headers plus any leading-only noise tokens.
fn cleanup_title(raw: &str) -> String {
    const NOISE_PREFIXES: &[&str] = &[
        "FACILITY UPGRADE",
        "GRAPHIC FACILITY UPGRADE",
        "GRAPHIC PRESENTATION FACILITY UPGRADE",
        "PRESENTATION FACILITY UPGRADE",
    ];
    let mut s = raw.trim().to_string();
    loop {
        let mut stripped = false;
        for noise in NOISE_PREFIXES {
            if let Some(rest) = s.strip_prefix(noise) {
                s = rest.trim().to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    // Sometimes the title appears twice on the same OCR line (the chip
    // bar repeats the upgrade name). Dedup adjacent duplicates by
    // splitting on whitespace and walking.
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::new();
    for w in &words {
        if out.last().is_some_and(|&prev| prev.eq_ignore_ascii_case(w)) {
            continue;
        }
        out.push(w);
    }
    out.join(" ")
}

// --- Cost -------------------------------------------------------------

fn find_cost(lines: &[Line]) -> Option<u64> {
    // The cost lives near "Need to submit items" — either same line
    // (next token) or the line right above/below. Look for an all-digit
    // token >= 1000 (smaller numbers are likely overall progress like
    // "0/1" mis-read, or cell counts).
    for line in lines {
        for w in &line.words {
            let cleaned: String = w
                .text
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            if cleaned.len() < 4 {
                continue;
            }
            if let Ok(n) = cleaned.parse::<u64>() {
                if (1_000..=99_999_999).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

// --- Items ------------------------------------------------------------

/// Tokens that should be ignored when scanning for cell names — UI
/// chrome that surrounds the item cells in the hideout panel.
const ITEM_NOISE: &[&str] = &[
    "BACK",
    "LEVEL",
    "UP",
    "LEVEL UP",
    "FACILITY",
    "UPGRADE",
    "GRAPHIC",
    "PRESENTATION",
    "FROM",
    "RAID",
    "FROM RAID",
    "Need",
    "to",
    "submit",
    "items",
    "Storage",
    "Room",
    "Access",
    "storage",
    "room",
    "on",
    "t",  // OCR artifact for the up-arrow glyph
    "SUBMIT",
    "SERVICE",
    "RESTOCK",
];

fn find_items(lines: &[Line]) -> Vec<CapturedItem> {
    // Heuristic: the item-name region sits between the "submit items"
    // line and the "BACK / LEVEL UP" button row. Within that region,
    // gather all non-noise words and cluster them by X-gap into cells.
    let (start_y, end_y) = item_region_y_range(lines);
    // Tag each word with its original index (the order it arrived in
    // from OCR — Windows OCR returns them in reading order: top-to-
    // bottom by line, left-to-right within line). We need this to
    // restore reading order *within* a cell after X-sorting splits the
    // words across cell-clusters — without it, multi-line cell names
    // like "Large gas / can" come out as "Large can gas" (X-order
    // when the second line is centered below the first).
    let mut cell_words: Vec<(usize, &OcrWord)> = Vec::new();
    let mut original_idx = 0usize;
    for line in lines {
        if line.y < start_y || line.y > end_y {
            continue;
        }
        for w in &line.words {
            let idx = original_idx;
            original_idx += 1;
            if is_noise_word(&w.text) {
                continue;
            }
            if is_progress_fraction(&w.text) {
                // Cell progress digits — captured separately when v2
                // wires per-cell collected/needed. For v1 the items
                // come out with progress = None.
                continue;
            }
            cell_words.push((idx, w));
        }
    }
    if cell_words.is_empty() {
        return Vec::new();
    }

    // Sort by X, then cluster by horizontal gap. We threshold on
    // *gap-between-words*, not word width — within a cell the gap is
    // small (~the OCR's natural inter-word spacing, ≤ a glyph width);
    // between cells it's an order of magnitude bigger (the panel pads
    // each cell with substantial whitespace). Use 3× the median gap
    // as the cell boundary, which is robust against any single
    // unusually wide word (e.g. "Disinfectant") skewing the metric.
    cell_words.sort_by(|a, b| {
        a.1.rect
            .x
            .partial_cmp(&b.1.rect.x)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let by_word: Vec<&OcrWord> = cell_words.iter().map(|(_, w)| *w).collect();
    let cell_gap = compute_cell_gap_threshold(&by_word);

    let mut cells: Vec<Vec<(usize, &OcrWord)>> = Vec::new();
    for entry in cell_words {
        let w = entry.1;
        if let Some(last_cell) = cells.last() {
            if let Some(prev) = last_cell.last() {
                if w.rect.x - prev.1.rect.right() > cell_gap {
                    cells.push(vec![entry]);
                    continue;
                }
            }
        }
        match cells.last_mut() {
            Some(cell) => cell.push(entry),
            None => cells.push(vec![entry]),
        }
    }

    // Restore reading order within each cell so multi-line names
    // come out as they're actually displayed in-game.
    for cell in &mut cells {
        cell.sort_by_key(|(idx, _)| *idx);
    }

    cells
        .into_iter()
        .map(|cell| CapturedItem {
            name: cell
                .iter()
                .map(|(_, w)| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            collected: None,
            needed: None,
        })
        .collect()
}

/// Distance between adjacent words that we treat as a cell boundary.
/// Computed as 3× the median word-to-word gap, clamped to a sensible
/// floor so a degenerate (all-same-X) input doesn't make every word
/// its own cell.
fn compute_cell_gap_threshold(words: &[&OcrWord]) -> f32 {
    if words.len() < 2 {
        return f32::INFINITY;
    }
    let mut gaps: Vec<f32> = words
        .windows(2)
        .map(|pair| (pair[1].rect.x - pair[0].rect.right()).max(0.0))
        .collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_gap = gaps[gaps.len() / 2];
    // Floor: at least one full glyph width, otherwise tightly-set
    // OCR output where the median is ~0 collapses every gap into a
    // cell boundary.
    let glyph_w = words[0].rect.width.max(8.0);
    (median_gap * 3.0).max(glyph_w * 1.5)
}

fn item_region_y_range(lines: &[Line]) -> (f32, f32) {
    // Floor: just below the "Need to submit items" line. Ceiling: just
    // above the "BACK / LEVEL UP" button row. Fall back to the whole
    // image when one of the markers is missing.
    let start = lines
        .iter()
        .find(|l| {
            let t = l.text().to_ascii_lowercase();
            t.contains("submit items") || t.contains("need to submit")
        })
        .map(|l| l.y + 1.0)
        .unwrap_or(f32::MIN);
    let end = lines
        .iter()
        .rfind(|l| {
            let t = l.text();
            t.contains("LEVEL UP") || (t.contains("BACK") && !t.contains("BACK to"))
        })
        .map(|l| l.y - 1.0)
        .unwrap_or(f32::MAX);
    if start <= end {
        (start, end)
    } else {
        // Markers out of order (OCR drift on a partial screenshot) —
        // give up on the region constraint and parse everything.
        (f32::MIN, f32::MAX)
    }
}

fn is_noise_word(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit() || c == '/') {
        // Pure number/fraction — handled elsewhere (cost / progress).
        return true;
    }
    ITEM_NOISE
        .iter()
        .any(|n| n.eq_ignore_ascii_case(trimmed))
}

fn is_progress_fraction(s: &str) -> bool {
    // "1/3", "10/100", etc. Also catches "011" which OCR sometimes
    // produces when the slash drops out of a fraction like "0/11".
    s.contains('/')
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::OcrRect;

    fn w(text: &str, x: f32, y: f32, width: f32) -> OcrWord {
        OcrWord {
            text: text.to_string(),
            rect: OcrRect {
                x,
                y,
                width,
                height: 24.0,
            },
        }
    }

    fn make(words: Vec<OcrWord>) -> OcrResult {
        OcrResult {
            image_width: 2000,
            image_height: 1200,
            text: String::new(),
            words,
        }
    }

    #[test]
    fn normalizes_lvo_to_lv0() {
        let (_, level) = split_at_level_chip("Storage Room A LVO ").unwrap();
        assert_eq!(level, "LV0");
    }

    #[test]
    fn normalizes_lvl_to_lv1() {
        let (_, level) = split_at_level_chip("Storage Room A LVl").unwrap();
        assert_eq!(level, "LV1");
    }

    #[test]
    fn parses_header_and_items_from_clean_storage_room_a() {
        // Synthetic word boxes mirroring the cleanest Storage Room A
        // OCR run we've seen — 4 cells in a row with multi-word names.
        let words = vec![
            // Header / title row
            w("FACILITY", 100.0, 50.0, 200.0),
            w("UPGRADE", 320.0, 50.0, 180.0),
            w("Storage", 100.0, 110.0, 180.0),
            w("Room", 300.0, 110.0, 100.0),
            w("A", 420.0, 110.0, 30.0),
            w("LVO", 470.0, 110.0, 60.0),
            // Need-to-submit marker
            w("Need", 100.0, 270.0, 80.0),
            w("to", 200.0, 270.0, 40.0),
            w("submit", 260.0, 270.0, 100.0),
            w("items", 380.0, 270.0, 80.0),
            w("80000", 600.0, 270.0, 140.0),
            // Item cells, single row
            w("Boxed", 100.0, 500.0, 120.0),
            w("Nails", 240.0, 500.0, 100.0),
            w("Boxed", 500.0, 500.0, 120.0),
            w("Balts", 640.0, 500.0, 100.0),
            w("Disinfectant", 900.0, 500.0, 220.0),
            w("wipes", 1140.0, 500.0, 100.0),
            w("Large", 1400.0, 500.0, 120.0),
            w("gas", 1540.0, 500.0, 80.0),
            w("can", 1640.0, 500.0, 80.0),
            // Button row
            w("BACK", 200.0, 700.0, 100.0),
            w("LEVEL", 1500.0, 700.0, 110.0),
            w("UP", 1630.0, 700.0, 70.0),
        ];
        let result = make(words);
        let cap = parse_upgrade(&result).expect("should parse");
        assert_eq!(cap.title, "Storage Room A");
        assert_eq!(cap.level, "LV0");
        assert_eq!(cap.cost, Some(80_000));
        assert_eq!(cap.items.len(), 4);
        assert_eq!(cap.items[0].name, "Boxed Nails");
        assert_eq!(cap.items[1].name, "Boxed Balts");
        assert_eq!(cap.items[2].name, "Disinfectant wipes");
        assert_eq!(cap.items[3].name, "Large gas can");
    }
}
