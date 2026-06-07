//! Box-container screen OCR: read a scrollable grid of item tiles and merge a
//! series of overlapping scroll captures into one item list by row uniqueness.
//!
//! A "box" container shows its contents on an in-game screen as a grid of tiles
//! (icon on top, name label below, ~4 columns) plus a total-weight readout
//! (`21.94 / 30`). The list scrolls, so a single screenshot rarely shows
//! everything; the user takes several while scrolling down and we merge them.
//!
//! This module has two clearly separated halves:
//!   - The **merge core** ([`merge_capture`], [`rows_match`], [`tally_rows`])
//!     operates on rows of [`Tile`] (`Option<ItemId>`, `None` = a tile we
//!     couldn't match). It is pure, platform-independent, and unit-tested on
//!     every target — it carries no OCR or image types.
//!   - The **OCR geometry** ([`process_box_image`], Windows-only) turns one
//!     screenshot into rows of tiles ([`BoxReadResult::tile_rows`]). It lives
//!     behind `#[cfg(windows)]` like the rest of [`crate::ocr`].
//!
//! ## Row-uniqueness merge, not position alignment
//!
//! A scan accumulates the *unique rows* of the grid. Each new capture's rows are
//! folded into the running set: a row already present (by the items composing
//! it) is a re-seen overlap and dropped; a genuinely new row is appended. A row
//! is identified by its **multiset of recognized items**, tolerant of one
//! drifted/missing tile so a marginal OCR pass still matches ([`rows_match`]).
//!
//! This replaced an earlier position-rigid sequence stitch that required the new
//! capture's prefix to align index-for-index with the running tail: one dropped
//! or clipped boundary tile shifted every later tile and collapsed the match, so
//! the 2nd+ capture almost always refused to merge. Row uniqueness is immune to
//! scroll distance and clipped boundary rows — overlap is handled by dropping
//! duplicate rows, not by finding a seam.
//!
//! The one cost: two *distinct* rows with the identical item composition (only
//! possible when many duplicate stackable items fill ≥2 full identical rows)
//! collapse to one and under-count. The desktop review step renders the captured
//! rows so the user can see and drop a bad one before applying.

use crate::data::{GameData, ItemId};
use crate::ocr::match_item::match_item;
use std::collections::HashMap;

/// One grid tile resolved to a known item, or `None` when OCR couldn't match
/// the label to any `Item.name` (below the matcher's threshold).
pub type Tile = Option<ItemId>;

/// One captured grid row: the tiles composing it, left→right as read, plus a
/// session-unique `id` so the desktop review step can address a row to drop.
/// Rows are the unit of cross-capture dedup ([`merge_capture`]); see module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRow {
    pub id: u64,
    pub tiles: Vec<Tile>,
}

/// What folding one new capture's rows into the running unique-row set did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureMerge {
    /// Rows genuinely new this capture (appended to the running set).
    pub rows_added: usize,
    /// Rows already present (overlap with an earlier capture) and skipped.
    pub rows_duplicate: usize,
}

/// The recognized (`Some`) item ids in a row, in order — `None` tiles dropped.
fn known_items(tiles: &[Tile]) -> Vec<&ItemId> {
    tiles.iter().filter_map(|t| t.as_ref()).collect()
}

/// Multiset-intersection size of two rows' recognized items: how many items they
/// share, counting multiplicity (two `nail`s on each side count as two).
fn shared_items(a: &[Tile], b: &[Tile]) -> usize {
    let mut remaining = known_items(b);
    let mut shared = 0;
    for id in known_items(a) {
        if let Some(pos) = remaining.iter().position(|x| *x == id) {
            remaining.swap_remove(pos);
            shared += 1;
        }
    }
    shared
}

/// Do two captured rows describe the *same* physical grid row?
///
/// Identity is the **multiset of recognized items** — order-independent (shear
/// can reorder a row) and position-independent (a dropped tile doesn't shift a
/// seam). "One drift, ≥2 must match": for a row of ≥3 recognized items we
/// tolerate a single drifted or missing tile (so ≥2 still agree); a 1–2 item row
/// carries too little signal, so it must match exactly. An all-`None` row never
/// matches — but [`read_tiles`] drops those as chrome before they reach here.
pub fn rows_match(a: &[Tile], b: &[Tile]) -> bool {
    let la = known_items(a).len();
    let lb = known_items(b).len();
    let maxlen = la.max(lb);
    if maxlen == 0 {
        return false;
    }
    let shared = shared_items(a, b);
    if maxlen >= 3 {
        // Tolerate one drift/missing; ≥2 agreements is implied (maxlen − 1 ≥ 2).
        shared + 1 >= maxlen
    } else {
        // 1–2 item rows: require an exact composition (same length, all shared).
        la == lb && shared == maxlen
    }
}

/// Fold one capture's rows into the running unique-row set `master`.
///
/// Each new row that [`rows_match`]es a row already in `master` is a re-seen
/// overlap and dropped; the rest are appended, each assigned a fresh id from
/// `next_id`. Returns how many rows were new vs. duplicate. Order-independent and
/// idempotent: re-capturing a view we already have adds nothing. Two *distinct*
/// rows with identical compositions collapse to one (the documented under-count
/// cost of row uniqueness — surfaced for manual fixup in the desktop review).
pub fn merge_capture(
    master: &mut Vec<ScanRow>,
    next_id: &mut u64,
    new_rows: &[Vec<Tile>],
) -> CaptureMerge {
    let mut out = CaptureMerge::default();
    for row in new_rows {
        if master.iter().any(|m| rows_match(&m.tiles, row)) {
            out.rows_duplicate += 1;
        } else {
            master.push(ScanRow {
                id: *next_id,
                tiles: row.clone(),
            });
            *next_id += 1;
            out.rows_added += 1;
        }
    }
    out
}

/// Count each recognized item across a flat tile sequence; report how many tiles
/// stayed unrecognized (surfaced to the user, never written to a container). Used
/// for the per-shot "this capture" tally; the running scan tally goes through
/// [`tally_rows`].
pub fn tally(master: &[Tile]) -> (HashMap<ItemId, u32>, usize) {
    let mut counts: HashMap<ItemId, u32> = HashMap::new();
    let mut unrecognized = 0usize;
    for tile in master {
        match tile {
            Some(id) => *counts.entry(id.clone()).or_insert(0) += 1,
            None => unrecognized += 1,
        }
    }
    (counts, unrecognized)
}

/// Count each recognized item across a scan's unique rows; report how many tiles
/// stayed unrecognized. This is the running tally the worker publishes and the
/// desktop review applies — it recomputes from whatever rows survive (so dropping
/// a row in review just drops its items).
pub fn tally_rows(rows: &[ScanRow]) -> (HashMap<ItemId, u32>, usize) {
    let mut counts: HashMap<ItemId, u32> = HashMap::new();
    let mut unrecognized = 0usize;
    for row in rows {
        for tile in &row.tiles {
            match tile {
                Some(id) => *counts.entry(id.clone()).or_insert(0) += 1,
                None => unrecognized += 1,
            }
        }
    }
    (counts, unrecognized)
}

// ===========================================================================
// OCR geometry: one screenshot → a reading-order `Vec<Tile>`.
//
// The clustering below works on [`LabelBox`] (plain text-box geometry), NOT the
// Windows-only `OcrWord`, so it's unit-tested on every target. Only the OCR
// call + `OcrWord` → `LabelBox` conversion ([`process_box_image`]) is
// Windows-gated.
//
// The thresholds below are tuned against the real captures in
// `screenshots/box/` and `screenshots/stash/` (regenerate the OCR with
// `ocr_debug`). They are
// expressed in multiples of the median text height so they scale with capture
// resolution rather than being pixel-absolute.
//
// Every item below is reached only by `process_box_image` (Windows) or the unit
// tests; the non-test, non-Windows build sees them as dead. We keep them
// compiled cross-target so the clustering tests run on Linux CI, so each
// carries `#[allow(dead_code)]` (not `#[cfg(windows)]`, which would drop the
// tests with the code).
// ===========================================================================

/// A single recognized text box — the platform-independent shape the grid
/// clustering consumes, decoupled from the Windows-only `OcrWord`.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct LabelBox {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[allow(dead_code)]
impl LabelBox {
    fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    fn right(&self) -> f32 {
        self.x + self.w
    }
}

/// One screenshot's worth of box-screen reading: the reading-order tiles plus
/// the total-weight readout used as a post-merge sanity checksum. `slope` and
/// `rows` are recognition diagnostics, surfaced in the `ocr_debug` dump
/// ([`format_capture_dump`]) so a failed scan is debuggable in-app.
#[derive(Clone, Debug, Default)]
pub struct BoxReadResult {
    /// Every `Names`-row tile flattened in reading order — the per-shot "this
    /// capture" tally and the `ocr_debug` dump. The cross-capture merge uses
    /// [`tile_rows`](Self::tile_rows) instead.
    pub tiles: Vec<Tile>,
    /// One entry per `Names` sub-row: the tiles composing that grid row, in
    /// reading order. This is the unit fed to [`merge_capture`] — the merge
    /// dedups whole rows, never individual tiles.
    pub tile_rows: Vec<Vec<Tile>>,
    pub observed_weight: Option<f32>,
    /// Estimated perspective-shear slope used to de-tilt rows ([`shear_slope`]).
    pub slope: f32,
    /// Per-sub-row recognition trace, in reading order.
    pub rows: Vec<RowReport>,
    /// Per-tile ✓/✗ marks for the recognized (`Names`) rows, normalized to the
    /// crop rect — painted over the real tiles on the guide box (issue #137).
    /// Populated by [`process_box_image`] (which knows the image width); left
    /// empty by the platform-independent [`read_tiles`] and the non-Windows
    /// stub. Derived purely from `rows` + `slope` via [`tile_marks`].
    pub marks: Vec<TileMark>,
}

/// A box/stash tile's ✓/✗ mark for the guide overlay (issue #137): where the
/// tile sits (normalized to the crop rect) and whether OCR matched it to a
/// catalog item (`true` → green ✓, `false` → red ✗).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileMark {
    pub pos: crate::ocr::CropMark,
    pub matched: bool,
}

/// How [`read_tiles`] classified one sub-row (for the `ocr_debug` dump).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A tab strip or a row of per-item category subtitles — skipped.
    Category,
    /// Nothing resolved to an item (window title, weight readout, service
    /// panel, tooltip) — dropped before stitching.
    Chrome,
    /// A grid name row whose tiles are kept and fed to the stitch.
    Names,
}

/// One sub-row's recognition trace for the `ocr_debug` dump: where it sits
/// (de-sheared `ry`), how it was classified, and each tile's joined OCR text
/// with the item it resolved to (`None` = no match).
#[derive(Clone, Debug)]
pub struct RowReport {
    pub ry: f32,
    pub kind: RowKind,
    pub cells: Vec<(String, Tile)>,
    /// Each tile's center **x** in raw image pixels, **parallel to `cells`**.
    /// Combined with the de-sheared row `ry` (re-sheared per tile via the
    /// capture's `slope`) this places a guide-box mark over each tile (#137).
    pub cxs: Vec<f32>,
}

/// Category words shown on the box screen. The fixed top **tab strip** (All /
/// Medical Supplies / … / Tool) and the per-item **category subtitle** under each
/// tile both draw from this one small vocabulary. We classify a *tile* as chrome
/// — never an item — when every one of its words is a category word, so the same
/// rule drops both the tab strip and the subtitles. A multi-word item name that
/// merely starts with one of these (e.g. "Medical scissors", "Electric drill",
/// "Power Bank") keeps a non-category word and survives.
///
/// Hard-coded for the current English UI — revisit if the game relabels or
/// localizes these.
#[allow(dead_code)]
const CATEGORY_WORDS: &[&str] = &[
    "all",
    "medical",
    "supplies",
    "building",
    "combustible",
    "electric",
    "household",
    "intel",
    "tool",
    "power",
];

/// Lowercase a label word and keep only its alphanumerics, for comparison
/// against [`CATEGORY_WORDS`] (so trailing punctuation / case never matters).
#[allow(dead_code)]
fn category_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a tile is a category cell (a tab or a per-item subtitle): it has at
/// least one word and *every* word is a [`CATEGORY_WORDS`] entry. Requiring all
/// words keeps item names that merely begin with a category word (e.g. "Medical
/// scissors") classified as names.
#[allow(dead_code)]
fn is_category_tile(tile: &[&LabelBox]) -> bool {
    let mut saw_word = false;
    for b in tile {
        for word in b.text.split_whitespace() {
            let key = category_key(word);
            if key.is_empty() {
                continue;
            }
            saw_word = true;
            if !CATEGORY_WORDS.contains(&key.as_str()) {
                return false;
            }
        }
    }
    saw_word
}

#[allow(dead_code)]
fn median<I: Iterator<Item = f32>>(vals: I) -> f32 {
    let mut v: Vec<f32> = vals.collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The "used weight" readout (left number of `21.94 / 30`): the bottom-most
/// token in the lower part of the image that parses as a decimal *with* a
/// fractional part — the capacity (`30`) is an integer, so requiring the dot
/// distinguishes the two. Returns `(value, top_y)`; `top_y` also marks where
/// the scrolling grid ends.
#[allow(dead_code)]
fn extract_weight(boxes: &[LabelBox], img_h: f32) -> Option<(f32, f32)> {
    let mut best: Option<&LabelBox> = None;
    for b in boxes {
        if b.cy() < img_h * 0.6 {
            continue; // weight chrome lives in the lower part of the panel
        }
        if parse_weight_token(&b.text).is_none() {
            continue;
        }
        best = match best {
            Some(prev) if prev.cy() >= b.cy() => best,
            _ => Some(b),
        };
    }
    best.and_then(|b| parse_weight_token(&b.text).map(|v| (v, b.y)))
}

/// Parse a weight token, keeping only digits and a decimal separator and
/// requiring a fractional part (so a bare capacity integer doesn't match).
#[allow(dead_code)]
fn parse_weight_token(s: &str) -> Option<f32> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    let cleaned = cleaned.replace(',', ".");
    if !cleaned.contains('.') {
        return None;
    }
    cleaned.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// De-sheared vertical position of a box: `cy - slope*cx`. The tablet is viewed
/// at an angle in VR, so a visually horizontal row is *tilted* — its boxes' `cy`
/// rises or falls across the grid width. Subtracting the shear flattens rows into
/// horizontal bands so they cluster cleanly.
#[allow(dead_code)]
fn deshear(b: &LabelBox, slope: f32) -> f32 {
    b.cy() - slope * b.cx()
}

/// Estimate the box screen's perspective-shear slope `dy/dx`.
///
/// Left uncorrected the tilt wrecks row clustering: a single tilted row fragments
/// (its `cy` spread exceeds the row tolerance) or merges with its neighbour. We
/// seed with a Theil–Sen estimate — the median slope over pairs of boxes that are
/// plausibly in the same row (a moderate horizontal gap, a small vertical delta),
/// robust to the many cross-row pairs — then refine by clustering with the seed,
/// least-squares-fitting each wide row, and taking the median of those fits.
#[allow(dead_code)]
fn shear_slope(boxes: &[&LabelBox], med_h: f32) -> f32 {
    let (dx_min, dx_max, dy_max) = (med_h, med_h * 50.0, med_h * 3.0);
    let mut seeds: Vec<f32> = Vec::new();
    for a in boxes {
        for b in boxes {
            let dx = b.cx() - a.cx();
            let dy = b.cy() - a.cy();
            if dx > dx_min && dx < dx_max && dy.abs() < dy_max {
                seeds.push(dy / dx);
            }
        }
    }
    let mut slope = if seeds.is_empty() {
        0.0
    } else {
        median(seeds.into_iter())
    };

    for _ in 0..3 {
        let rows = cluster_rows(boxes, slope, med_h * 0.7);
        let mut fits: Vec<f32> = Vec::new();
        for row in &rows {
            if row.len() < 3 {
                continue;
            }
            let n = row.len() as f32;
            let mx = row.iter().map(|b| b.cx()).sum::<f32>() / n;
            let my = row.iter().map(|b| b.cy()).sum::<f32>() / n;
            let num: f32 = row.iter().map(|b| (b.cx() - mx) * (b.cy() - my)).sum();
            let den: f32 = row.iter().map(|b| (b.cx() - mx).powi(2)).sum();
            let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
            for b in row {
                lo = lo.min(b.cx());
                hi = hi.max(b.cx());
            }
            // Only trust rows wide enough that the fit reflects the global tilt.
            if den > 1e-3 && hi - lo > med_h * 10.0 {
                fits.push(num / den);
            }
        }
        if !fits.is_empty() {
            slope = median(fits.into_iter());
        }
    }
    slope
}

/// Group boxes into rows by de-sheared vertical position: boxes whose `cy -
/// slope*cx` falls within `tol` of the running row mean share a row. Returns rows
/// top→bottom.
#[allow(dead_code)]
fn cluster_rows<'a>(boxes: &[&'a LabelBox], slope: f32, tol: f32) -> Vec<Vec<&'a LabelBox>> {
    if boxes.is_empty() {
        return Vec::new();
    }
    let mut sorted = boxes.to_vec();
    sorted.sort_by(|a, b| deshear(a, slope).total_cmp(&deshear(b, slope)));

    let mut rows: Vec<Vec<&LabelBox>> = Vec::new();
    let mut cur: Vec<&LabelBox> = Vec::new();
    let mut cur_ry = 0.0f32;
    for b in sorted {
        if cur.is_empty() || (deshear(b, slope) - cur_ry).abs() <= tol {
            cur.push(b);
        } else {
            rows.push(std::mem::take(&mut cur));
            cur.push(b);
        }
        cur_ry = cur.iter().map(|x| deshear(x, slope)).sum::<f32>() / cur.len() as f32;
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Split one generous grid-row *block* into sub-rows at de-sheared vertical gaps
/// wider than ~one text height. A box tile stacks the item **name** over its
/// **category subtitle** (~2 text heights apart), so this separates the two; the
/// subtitle sub-row is then dropped while the name survives. A block with a single
/// sub-row (the names-only tablet layout, or a lone chrome line) returns unsplit.
#[allow(dead_code)]
fn split_subrows<'a>(block: &[&'a LabelBox], slope: f32, med_h: f32) -> Vec<Vec<&'a LabelBox>> {
    let mut sorted = block.to_vec();
    sorted.sort_by(|a, b| deshear(a, slope).total_cmp(&deshear(b, slope)));
    let gap = (med_h * 1.1).max(1.0);

    let mut subs: Vec<Vec<&LabelBox>> = Vec::new();
    let mut cur: Vec<&LabelBox> = Vec::new();
    let mut prev_ry = f32::NEG_INFINITY;
    for b in sorted {
        if !cur.is_empty() && deshear(b, slope) - prev_ry > gap {
            subs.push(std::mem::take(&mut cur));
        }
        prev_ry = deshear(b, slope);
        cur.push(b);
    }
    if !cur.is_empty() {
        subs.push(cur);
    }
    subs
}

/// Split one row's boxes into tiles by horizontal gaps: words within a tile's
/// label sit close together, while the gap between tiles is much wider. Returns
/// tiles left→right, each a list of its label words (left→right).
#[allow(dead_code)]
fn split_tiles<'a>(row: &[&'a LabelBox]) -> Vec<Vec<&'a LabelBox>> {
    let mut sorted = row.to_vec();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
    let gap_thresh = (median(sorted.iter().map(|b| b.h)) * 1.5).max(1.0);

    let mut tiles: Vec<Vec<&LabelBox>> = Vec::new();
    let mut cur: Vec<&LabelBox> = Vec::new();
    let mut prev_right = f32::NEG_INFINITY;
    for b in sorted {
        if !cur.is_empty() && b.x - prev_right > gap_thresh {
            tiles.push(std::mem::take(&mut cur));
        }
        prev_right = b.right();
        cur.push(b);
    }
    if !cur.is_empty() {
        tiles.push(cur);
    }
    tiles
}

/// Turn one screenshot's recognized text boxes into reading-order tiles.
///
/// Layout-aware and tilt-robust ([issue #109]):
///   1. Estimate the perspective shear ([`shear_slope`]) and cluster boxes into
///      generous grid-row *blocks* by de-sheared `cy` ([`cluster_rows`]). A block
///      holds one grid row — an item name with its category subtitle — while
///      neighbouring rows stay apart even when the slope estimate is imperfect.
///   2. Split each block into sub-rows ([`split_subrows`]) and split each sub-row
///      into tiles by horizontal gaps ([`split_tiles`]).
///   3. Drop **category** sub-rows (mostly [`is_category_tile`]s) — this skips a
///      real top tab strip *and* the per-item subtitles with one rule. Resolve the
///      remaining tiles via [`match_item`] and drop a sub-row that resolves to
///      nothing at all (the title, weight readout, service panel, tooltips — none
///      of it is an item). The fixed chrome must go because it doesn't scroll;
///      leaving it in would wreck the cross-capture alignment.
///
/// Reading order is block top→bottom, sub-row top→bottom, tile left→right — stable
/// across shots (the de-shear removes the tilt that otherwise reorders a row),
/// which is what lets [`stitch`] align overlapping captures.
///
/// [issue #109]: https://github.com/etiennechabert/ez-wishlist-overlay/issues/109
#[allow(dead_code)]
pub fn read_tiles(boxes: &[LabelBox], img_h: f32, data: &GameData) -> BoxReadResult {
    let observed_weight = extract_weight(boxes, img_h).map(|(v, _)| v);

    let refs: Vec<&LabelBox> = boxes.iter().collect();
    let med_h = median(refs.iter().map(|b| b.h)).max(1.0);
    let slope = shear_slope(&refs, med_h);

    // Generous blocks: a name and its category subtitle (~2 text heights apart)
    // land together, while neighbouring grid rows (~10 text heights apart) stay
    // separate even if the slope estimate is a little off.
    let blocks = cluster_rows(&refs, slope, med_h * 4.0);

    let mut tiles: Vec<Tile> = Vec::new();
    let mut tile_rows: Vec<Vec<Tile>> = Vec::new();
    let mut rows: Vec<RowReport> = Vec::new();
    for block in &blocks {
        for sub in split_subrows(block, slope, med_h) {
            let ry = median(sub.iter().map(|b| deshear(b, slope)));
            let cells = split_tiles(&sub);
            // Per-tile center x (raw px), parallel to `cells` — feeds the
            // guide-box ✓/✗ marks (#137).
            let cxs: Vec<f32> = cells.iter().map(|t| tile_center_x(t)).collect();

            // Skip a category sub-row (tab strip or per-item subtitles): one whose
            // tiles are mostly category words.
            let cat = cells.iter().filter(|t| is_category_tile(t)).count();
            if cat > 0 && cat * 2 >= cells.len() {
                rows.push(RowReport {
                    ry,
                    kind: RowKind::Category,
                    cells: cells.iter().map(|t| (join_text(t), None)).collect(),
                    cxs,
                });
                continue;
            }

            // Resolve each tile; drop an all-unrecognized sub-row as chrome.
            let resolved: Vec<(String, Tile)> = cells
                .iter()
                .map(|t| {
                    let tokens: Vec<&str> = t.iter().map(|b| b.text.as_str()).collect();
                    (join_text(t), match_item(data, &tokens))
                })
                .collect();
            let kind = if resolved.iter().all(|(_, m)| m.is_none()) {
                RowKind::Chrome
            } else {
                RowKind::Names
            };
            rows.push(RowReport {
                ry,
                kind,
                cells: resolved.clone(),
                cxs,
            });
            if kind == RowKind::Names {
                let row_tiles: Vec<Tile> = resolved.into_iter().map(|(_, m)| m).collect();
                tiles.extend(row_tiles.iter().cloned());
                tile_rows.push(row_tiles);
            }
        }
    }

    BoxReadResult {
        tiles,
        tile_rows,
        observed_weight,
        slope,
        rows,
        // Marks need the image *width* to normalize x; `read_tiles` only takes
        // `img_h`. `process_box_image` fills these via [`tile_marks`] once it has
        // the full dimensions. Left empty here (and in the cross-platform tests).
        marks: Vec::new(),
    }
}

/// Center **x** (raw image px) of a tile's horizontal extent — the midpoint
/// between its leftmost word's left edge and its rightmost word's right edge.
/// Returns 0 for an empty tile (never produced by [`split_tiles`], but safe).
#[allow(dead_code)]
fn tile_center_x(tile: &[&LabelBox]) -> f32 {
    let lo = tile.iter().map(|b| b.x).fold(f32::INFINITY, f32::min);
    let hi = tile
        .iter()
        .map(|b| b.right())
        .fold(f32::NEG_INFINITY, f32::max);
    if lo.is_finite() && hi.is_finite() {
        (lo + hi) / 2.0
    } else {
        0.0
    }
}

/// Build the per-tile ✓/✗ guide marks (issue #137) for one shot's recognized
/// rows. Only [`RowKind::Names`] rows contribute. Each tile's mark sits at its
/// own center x (`cxs[i]`) and the row's de-sheared `ry` **re-sheared back** to
/// that column (`ry + slope*cx`, the inverse of [`deshear`], so the mark lands
/// on the tile's true screen y rather than its flattened cluster y), then
/// normalized to the `img_w × img_h` crop. `matched` is whether OCR resolved
/// the tile to a catalog item. Pure + cross-platform so the position→normalized
/// mapping is CI-tested.
#[allow(dead_code)]
pub fn tile_marks(rows: &[RowReport], slope: f32, img_w: f32, img_h: f32) -> Vec<TileMark> {
    let mut marks = Vec::new();
    for row in rows {
        if row.kind != RowKind::Names {
            continue;
        }
        for (&cx, (_, tile)) in row.cxs.iter().zip(row.cells.iter()) {
            let cy = row.ry + slope * cx;
            marks.push(TileMark {
                pos: crate::ocr::CropMark::from_px(cx, cy, img_w, img_h),
                matched: tile.is_some(),
            });
        }
    }
    marks
}

/// Whitespace-join a tile's label words left→right (for diagnostics / matching
/// previews).
#[allow(dead_code)]
fn join_text(tile: &[&LabelBox]) -> String {
    tile.iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render a human-readable recognition dump for one box-scan capture, for the
/// `ocr_debug` sidecar. Shows the de-shear slope, every sub-row's classification
/// and per-tile match, this shot's reading-order tiles, the stitch verdict, and
/// the running session tally — enough to see *why* a scan mis-read without the
/// game running. Pure (no I/O); the caller writes it next to the source PNG.
#[allow(dead_code)]
pub fn format_capture_dump(
    read: &BoxReadResult,
    outcome: CaptureMerge,
    tally: &HashMap<ItemId, u32>,
    unrecognized: usize,
    captures: u32,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "=== BOX-SCAN CAPTURE #{captures} ===");
    let _ = writeln!(s, "shear slope     : {:+.4}", read.slope);
    match read.observed_weight {
        Some(w) => {
            let _ = writeln!(s, "observed weight : {w:.2}");
        }
        None => {
            let _ = writeln!(s, "observed weight : —");
        }
    }
    let _ = writeln!(
        s,
        "merge           : +{} new row(s), {} duplicate row(s)",
        outcome.rows_added, outcome.rows_duplicate
    );
    let _ = writeln!(
        s,
        "this shot       : {} row(s), {} tile(s)",
        read.tile_rows.len(),
        read.tiles.len()
    );
    let _ = writeln!(s);

    // The captured contents this shot, reconstructed row by row: one line per
    // grid row that resolved to an item, each tile shown as its item id (`_` for
    // a tile that matched nothing), left→right. This concise view sits above the
    // verbose per-row trace below so a scan can be read — and a ground-truth
    // label rebuilt — row by row without wading through the chrome/category lines.
    let _ = writeln!(s, "=== CAPTURED ITEMS (this shot, per row) ===");
    if read.tile_rows.is_empty() {
        let _ = writeln!(s, "  (no item rows this capture)");
    } else {
        for (i, row) in read.tile_rows.iter().enumerate() {
            let cells: Vec<String> = row
                .iter()
                .map(|t| t.clone().unwrap_or_else(|| "_".to_string()))
                .collect();
            let _ = writeln!(s, "  row {:>2}: {}", i + 1, cells.join("  "));
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== ROWS (reading order, de-sheared) ===");
    for r in &read.rows {
        let (tag, note) = match r.kind {
            RowKind::Names => ("name  ", ""),
            RowKind::Category => ("CAT   ", "  (skipped: tab strip / subtitles)"),
            RowKind::Chrome => ("chrome", "  (dropped: no item matched)"),
        };
        let _ = writeln!(s, "  [{tag}] ry={:>7.1}{note}", r.ry);
        for (text, resolved) in &r.cells {
            match resolved {
                Some(id) => {
                    let _ = writeln!(s, "      {text:<28} -> {id}");
                }
                None => {
                    let _ = writeln!(s, "      {text:<28} -> —");
                }
            }
        }
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "=== RUNNING TALLY (after this capture) ===");
    let mut items: Vec<(&ItemId, &u32)> = tally.iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    for (id, n) in items {
        let _ = writeln!(s, "  {id:<28} {n}");
    }
    let _ = writeln!(s, "  (unrecognized tiles: {unrecognized})");
    s
}

/// OCR one box-screen screenshot into a [`BoxReadResult`]. Windows-only — runs
/// Windows.Media.Ocr, then hands the word boxes to the platform-independent
/// [`read_tiles`].
#[cfg(target_os = "windows")]
pub fn process_box_image(
    img: &image::DynamicImage,
    data: &GameData,
) -> anyhow::Result<BoxReadResult> {
    use crate::ocr::engine;
    use anyhow::Context;
    use image::GenericImageView;

    let (img_w, img_h) = img.dimensions();
    let words = engine::recognize_image(img).context("box-screen OCR")?;
    let boxes: Vec<LabelBox> = words
        .iter()
        .map(|w| LabelBox {
            text: w.text.clone(),
            x: w.rect.x,
            y: w.rect.y,
            w: w.rect.width,
            h: w.rect.height,
        })
        .collect();
    let mut read = read_tiles(&boxes, img_h as f32, data);
    // Now that we have the full dimensions, normalize each recognized tile's
    // position into the crop rect for the guide-box ✓/✗ marks (issue #137).
    read.marks = tile_marks(&read.rows, read.slope, img_w as f32, img_h as f32);
    Ok(read)
}

/// Non-Windows stub: Windows.Media.Ocr is unavailable, so there's nothing to
/// read. Mirrors [`crate::ocr::pipeline::process_image`]'s stub.
#[cfg(not(target_os = "windows"))]
pub fn process_box_image(
    _img: &image::DynamicImage,
    _data: &GameData,
) -> anyhow::Result<BoxReadResult> {
    Ok(BoxReadResult::default())
}

// `pub(crate)` so the Windows-only `eval_report_json` diagnostic in
// `pipeline.rs` can reuse `run_box_scan` / `load_box_label` / `score_scan`
// to score the box + stash assets in the same combined report.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build one row's tiles from a string spec: each whitespace-separated token
    /// is an item id, or `_` for an unrecognized (`None`) tile. Doubles as a flat
    /// tile sequence for the `tally` test.
    fn seq(spec: &str) -> Vec<Tile> {
        spec.split_whitespace()
            .map(|t| if t == "_" { None } else { Some(t.to_string()) })
            .collect()
    }

    // ===================================================================
    // Native-capture regression fixtures (`screenshots/box/`, `screenshots/stash/`).
    //
    // Real box-screen captures keep getting flushed from the debug dir, so we
    // freeze each one's Windows.Media.Ocr output (the word boxes) to JSON next
    // to its PNG. `read_tiles` + `merge_capture` are pure and platform-independent,
    // so these fixtures let us regression-test the whole post-OCR pipeline on every
    // target (incl. Linux CI) without re-running the Windows-only, slightly
    // nondeterministic engine. The PNGs are the ground truth; the `.boxes.json`
    // are regenerated from them by `regen_box_fixtures` (Windows, --ignored).
    //
    // Expected results live in `<scan>.label.txt` (`<item_id>  <count>` lines).
    // The `box` scan passes. The `stash` scan may stay `#[ignore]`d: row-uniqueness
    // is immune to the dropped-tile desync that blocked the old position stitch,
    // but its 10 shots have real *scroll gaps* (rows that appear in no shot at all)
    // past shot 04 — those rows are simply missing data and under-count, which no
    // merge can recover. See `stash_scan_matches_label` and `screenshots/CLAUDE.md`.
    // ===================================================================

    #[derive(serde::Serialize, serde::Deserialize)]
    struct BoxFixture {
        img_h: f32,
        boxes: Vec<FxWord>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct FxWord {
        text: String,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    }

    /// Fixture dir for a box-scan `category` — `"box"` (the world
    /// container tablet) or `"stash"` (the Johnny's-Service submit
    /// terminal) — each a subfolder of `screenshots/`. The per-category
    /// env override `<CATEGORY>_FIXTURE_DIR` (e.g. `BOX_FIXTURE_DIR`,
    /// `STASH_FIXTURE_DIR`) points `regen_box_fixtures` at a scratch
    /// copy (e.g. to compare OCR across image formats) without touching
    /// the committed set. Unset → the committed fixtures under
    /// `screenshots/<category>/`.
    fn fixture_dir(category: &str) -> std::path::PathBuf {
        let env_key = format!("{}_FIXTURE_DIR", category.to_uppercase());
        std::env::var_os(&env_key)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../screenshots")
                    .join(category)
            })
    }

    impl BoxFixture {
        fn load(category: &str, name: &str) -> Self {
            let p = fixture_dir(category).join(name);
            let s = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()));
            serde_json::from_str(&s)
                .unwrap_or_else(|e| panic!("parse fixture {}: {e}", p.display()))
        }
        fn label_boxes(&self) -> Vec<LabelBox> {
            self.boxes
                .iter()
                .map(|w| LabelBox {
                    text: w.text.clone(),
                    x: w.x,
                    y: w.y,
                    w: w.w,
                    h: w.h,
                })
                .collect()
        }
    }

    /// Run every shot of a scan (frozen-OCR fixtures, in scroll order) through
    /// `read_tiles` + `merge_capture` and return the final tally — what would be
    /// written into the container.
    pub(crate) fn run_box_scan(category: &str, shots: &[String]) -> HashMap<ItemId, u32> {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let mut master: Vec<ScanRow> = Vec::new();
        let mut next_id = 0u64;
        for shot in shots {
            let fx = BoxFixture::load(category, shot);
            let res = read_tiles(&fx.label_boxes(), fx.img_h, &data);
            merge_capture(&mut master, &mut next_id, &res.tile_rows);
        }
        tally_rows(&master).0
    }

    /// Parse a `<scan>.label.txt` ground-truth tally (`<item_id>  <count>`).
    pub(crate) fn load_box_label(category: &str, name: &str) -> HashMap<ItemId, u32> {
        let p = fixture_dir(category).join(name);
        std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("read label {}: {e}", p.display()))
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let mut it = l.split_whitespace();
                let id = it.next().expect("item id").to_string();
                let n: u32 = it.next().expect("count").parse().expect("count int");
                (id, n)
            })
            .collect()
    }

    /// One item whose read count diverges from the ground-truth label, for
    /// triaging a partial scan score.
    #[derive(serde::Serialize, Clone)]
    #[allow(dead_code)] // fields read via serde only in the Windows eval
    pub(crate) struct ScanDiff {
        pub item_id: String,
        pub read: u32,
        pub label: u32,
    }

    /// Graded tile-accuracy score for one box-scan, independent of the strict
    /// pass/fail gate. `tiles_correct = Σ min(read, label)` over item ids and
    /// `tiles_total = Σ label`, so a partial scan (e.g. the un-stitchable
    /// `stash`) gets a meaningful fraction instead of a bare `false`.
    #[derive(serde::Serialize, Clone)]
    #[allow(dead_code)] // fields read via serde only in the Windows eval
    pub(crate) struct ScanScore {
        /// The scan category (`"box"` / `"stash"`).
        pub scan: String,
        pub tiles_correct: u32,
        pub tiles_total: u32,
        /// Strict equality — the same thing the gate test asserts.
        pub exact_match: bool,
        /// Σ max(0, label − read): ground-truth tiles the scan failed to read.
        pub missing: u32,
        /// Σ max(0, read − label): tiles the scan over-counted / hallucinated.
        pub extra: u32,
        /// Per-item mismatches (only items where read ≠ label).
        pub diffs: Vec<ScanDiff>,
    }

    /// Score a scan's stitched tally against its `<scan>.label.txt`. Reuses the
    /// same pure `read_tiles` / `stitch` / `tally` path as `run_box_scan`, so it
    /// runs on every target from the frozen `.boxes.json` fixtures. Used by the
    /// combined `eval_report_json` (Windows) to score box + stash independently.
    #[allow(dead_code)] // called only from the Windows-gated eval diagnostic
    pub(crate) fn score_scan(category: &str, shots: &[String], label_file: &str) -> ScanScore {
        let read = run_box_scan(category, shots);
        let label = load_box_label(category, label_file);

        let mut keys: std::collections::BTreeSet<&ItemId> = read.keys().collect();
        keys.extend(label.keys());

        let (mut tiles_correct, mut tiles_total, mut missing, mut extra) = (0u32, 0u32, 0u32, 0u32);
        let mut diffs = Vec::new();
        for k in keys {
            let r = *read.get(k).unwrap_or(&0);
            let l = *label.get(k).unwrap_or(&0);
            tiles_total += l;
            tiles_correct += r.min(l);
            missing += l.saturating_sub(r);
            extra += r.saturating_sub(l);
            if r != l {
                diffs.push(ScanDiff {
                    item_id: k.clone(),
                    read: r,
                    label: l,
                });
            }
        }
        diffs.sort_by(|a, b| a.item_id.cmp(&b.item_id));

        ScanScore {
            scan: category.to_string(),
            tiles_correct,
            tiles_total,
            exact_match: read == label,
            missing,
            extra,
            diffs,
        }
    }

    // -----------------------------------------------------------------------
    // Committed, human-readable per-image OCR result sidecars (box / stash).
    //
    // Each scroll-shot's frozen `.boxes.json` is the *raw* OCR (word boxes) —
    // unreadable at a glance. These render the same frozen input through the
    // real pipeline into a `<shot>.ocr-result.txt` that says, in plain terms,
    // what the OCR made of that one image: a high-level summary first (how many
    // item tiles it recognized, how many texts it threw away), then the row-by-
    // row detail (every text it read and the catalog item it resolved to, or
    // "no match"). Deterministic and pure (`read_tiles` only), so they regen
    // identically on any platform and stay in lockstep with the `.boxes.json`.
    // Written by the ignored `write_box_scan_results` test; the merged,
    // label-scored result of the whole scan stays in `<scan>.ocr-result.txt`.
    // -----------------------------------------------------------------------

    /// Tally one frame's recognized tiles → `(item_id → count, unrecognized)`.
    fn shot_tally(read: &BoxReadResult) -> (Vec<(ItemId, u32)>, usize) {
        let mut counts: HashMap<ItemId, u32> = HashMap::new();
        let mut unrecognized = 0usize;
        for row in &read.tile_rows {
            for tile in row {
                match tile {
                    Some(id) => *counts.entry(id.clone()).or_insert(0) += 1,
                    None => unrecognized += 1,
                }
            }
        }
        let mut items: Vec<(ItemId, u32)> = counts.into_iter().collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        (items, unrecognized)
    }

    /// Render the per-image result text for one scroll shot: a high-level
    /// summary, the items it found, then the row-by-row OCR trace. Pure.
    #[allow(dead_code)] // used by the (ignored) committed-sidecar generator
    fn shot_result_text(stem: &str, scan: &str, read: &BoxReadResult) -> String {
        use std::fmt::Write as _;
        let (items, unrec) = shot_tally(read);
        let recognized: u32 = items.iter().map(|(_, n)| n).sum();
        let mut s = String::new();
        let _ = writeln!(s, "# {stem} — what the OCR read in this one screenshot");
        let _ = writeln!(
            s,
            "# (auto-generated; do not edit by hand — see screenshots/CLAUDE.md)"
        );
        let _ = writeln!(s, "#");
        let _ = writeln!(
            s,
            "# This is ONE scroll shot of the {scan} grid. The grid is taller than the"
        );
        let _ = writeln!(
            s,
            "# screen, so the full contents come from merging every shot — that merged"
        );
        let _ = writeln!(
            s,
            "# result, scored against the label, lives in {scan}.ocr-result.txt. This file"
        );
        let _ = writeln!(
            s,
            "# just shows what the OCR pulled out of this single frame, regenerated from"
        );
        let _ = writeln!(
            s,
            "# the frozen {stem}.boxes.json (so it never runs the game)."
        );
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "## SUMMARY");
        let _ = writeln!(
            s,
            "  recognized {recognized} item tile(s) across {} grid row(s)",
            read.tile_rows.len()
        );
        let _ = writeln!(
            s,
            "  dropped {unrec} text(s) that matched no catalog item (window title, weight"
        );
        let _ = writeln!(
            s,
            "    readout, category tabs/subtitles — i.e. screen chrome, not items)"
        );
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "  items found in this frame:");
        if items.is_empty() {
            let _ = writeln!(s, "    (none — no item tiles recognized in this shot)");
        } else {
            for (id, n) in &items {
                let _ = writeln!(s, "    {n} x {id}");
            }
        }
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "## DETAIL — every text the OCR produced, row by row");
        let _ = writeln!(
            s,
            "#   \"-> id\" resolved to that catalog item;  \"-> (no match)\" was dropped."
        );
        let _ = writeln!(
            s,
            "#   Rows tagged [items] are kept; [tabs]/[chrome] rows are dropped as UI."
        );
        for r in &read.rows {
            let tag = match r.kind {
                RowKind::Names => "items ",
                RowKind::Category => "tabs  ",
                RowKind::Chrome => "chrome",
            };
            let _ = writeln!(s, "  [{tag}]");
            for (text, resolved) in &r.cells {
                match resolved {
                    Some(id) => {
                        let _ = writeln!(s, "    {text:<30} -> {id}");
                    }
                    None => {
                        let _ = writeln!(s, "    {text:<30} -> (no match)");
                    }
                }
            }
        }
        s
    }

    /// Render the merged-scan result text: what the whole scan captured vs the
    /// ground-truth label, item by item, so a glance shows what was and wasn't
    /// found. Pure. `read` is the merged tally; `label` the per-scan ground truth.
    #[allow(dead_code)] // used by the (ignored) committed-sidecar generator
    fn scan_result_text(
        scan: &str,
        read: &HashMap<ItemId, u32>,
        label: &HashMap<ItemId, u32>,
        note: Option<&str>,
    ) -> String {
        use std::fmt::Write as _;
        let mut keys: std::collections::BTreeSet<&ItemId> = read.keys().collect();
        keys.extend(label.keys());
        let (mut captured, mut total, mut missing, mut extra) = (0u32, 0u32, 0u32, 0u32);
        let mut rows: Vec<(String, u32, u32)> = Vec::new();
        for k in keys {
            let r = *read.get(k).unwrap_or(&0);
            let l = *label.get(k).unwrap_or(&0);
            total += l;
            captured += r.min(l);
            missing += l.saturating_sub(r);
            extra += r.saturating_sub(l);
            rows.push((k.clone(), r, l));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let exact = *read == *label;

        let mut s = String::new();
        let _ = writeln!(s, "# {scan} — merged scan result vs the ground-truth label");
        let _ = writeln!(
            s,
            "# (auto-generated; do not edit by hand — see screenshots/CLAUDE.md)"
        );
        let _ = writeln!(s, "#");
        let _ = writeln!(
            s,
            "# The {scan} grid spans several scroll shots; this is the merged tally of all"
        );
        let _ = writeln!(
            s,
            "# of them (regenerated from the frozen .boxes.json — no game), compared to"
        );
        let _ = writeln!(
            s,
            "# {scan}.label.txt. Per-shot reads are in the sibling {scan}.shotN.ocr-result.txt."
        );
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "## SUMMARY");
        let _ = writeln!(
            s,
            "  captured {captured}/{total} label tile(s)  ({missing} missing, {extra} extra)"
        );
        let _ = writeln!(
            s,
            "  exact match: {}",
            if exact {
                "YES — the scan reads the label exactly"
            } else {
                "NO — see the per-item breakdown below"
            }
        );
        if let Some(n) = note {
            let _ = writeln!(s, "  note: {n}");
        }
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "## PER ITEM   (read = scan count, label = ground truth)");
        let _ = writeln!(s, "# status  read/label  item_id");
        for (id, r, l) in &rows {
            let status = if r == l {
                "OK     "
            } else if r < l {
                "MISSING"
            } else {
                "EXTRA  "
            };
            let _ = writeln!(s, "  {status}  {r:>3}/{l:<3}    {id}");
        }
        s
    }

    /// Shot fixtures per scan, in scroll order: `(category, [boxes.json names])`.
    /// Single source of truth shared by the eval scorer and the sidecar writer.
    #[allow(dead_code)]
    fn scan_shots(category: &str) -> Vec<String> {
        match category {
            "box" => (0..3).map(|i| format!("box.shot{i}.boxes.json")).collect(),
            "stash" => (0..10)
                .map(|i| format!("stash.shot{i:02}.boxes.json"))
                .collect(),
            other => panic!("unknown scan category {other:?}"),
        }
    }

    /// (Ignored) Regenerate the committed, human-readable per-image OCR result
    /// sidecars for both box-scan categories — one `<shot>.ocr-result.txt` per
    /// scroll-shot image (what that frame's OCR read) plus the merged
    /// `<scan>.ocr-result.txt` (captured vs label). Pure and deterministic (reads
    /// only the frozen `.boxes.json`), so it runs on every platform and the files
    /// stay in lockstep with the fixtures:
    ///   cargo test -p ez-wishlist-overlay write_box_scan_results -- --ignored
    #[test]
    #[ignore = "regenerates committed box/stash .ocr-result.txt sidecars"]
    fn write_box_scan_results() {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let notes: std::collections::HashMap<&str, &str> = [(
            "stash",
            "captures have real scroll gaps (some grid rows appear in no shot), so a \
             partial capture is expected here — informational, not gated",
        )]
        .into_iter()
        .collect();

        let mut written = 0usize;
        for category in ["box", "stash"] {
            let shots = scan_shots(category);
            // Per-shot: one sidecar per scroll-shot image.
            for shot in &shots {
                let fx = BoxFixture::load(category, shot);
                let read = read_tiles(&fx.label_boxes(), fx.img_h, &data);
                let stem = shot.trim_end_matches(".boxes.json");
                let text = shot_result_text(stem, category, &read);
                let out = fixture_dir(category).join(format!("{stem}.ocr-result.txt"));
                std::fs::write(&out, text)
                    .unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
                written += 1;
            }
            // Merged scan: captured vs label.
            let read = run_box_scan(category, &shots);
            let label = load_box_label(category, &format!("{category}.label.txt"));
            let text = scan_result_text(category, &read, &label, notes.get(category).copied());
            let out = fixture_dir(category).join(format!("{category}.ocr-result.txt"));
            std::fs::write(&out, text).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
            written += 1;
        }
        eprintln!("wrote {written} box/stash .ocr-result.txt sidecars");
    }

    /// (Windows-only, ignored) Regenerate every `<stem>.boxes.json` from the
    /// capture images (PNG / JPEG / WebP) across **both** box-scan categories
    /// (`screenshots/box/` and `screenshots/stash/`). Run after adding/replacing
    /// a capture:
    ///   cargo test -p ez-wishlist-overlay regen_box_fixtures -- --ignored
    #[test]
    #[ignore]
    #[cfg(target_os = "windows")]
    fn regen_box_fixtures() {
        use image::GenericImageView;
        for category in ["box", "stash"] {
            for entry in std::fs::read_dir(fixture_dir(category)).expect("fixture dir") {
                let path = entry.unwrap().path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !matches!(ext, "png" | "jpg" | "jpeg" | "webp") {
                    continue;
                }
                let img = image::open(&path).expect("open image");
                let words = crate::ocr::engine::recognize_image(&img).expect("ocr");
                let fx = BoxFixture {
                    img_h: img.dimensions().1 as f32,
                    boxes: words
                        .iter()
                        .map(|w| FxWord {
                            text: w.text.clone(),
                            x: w.rect.x,
                            y: w.rect.y,
                            w: w.rect.width,
                            h: w.rect.height,
                        })
                        .collect(),
                };
                let out = path.with_extension("boxes.json");
                std::fs::write(&out, serde_json::to_string_pretty(&fx).unwrap()).expect("write");
                eprintln!("wrote {} ({} boxes)", out.display(), fx.boxes.len());
            }
        }
    }

    /// (Ignored authoring aid) Dump `(shot, item_id, name-bbox)` for every
    /// recognized tile across all 10 stash shots, mirroring `read_tiles`'
    /// clustering. Used to generate the per-item `screenshots/stash/units/`
    /// crops without hand-identifying each tile: the cropper expands each
    /// name-bbox up for the icon and labels by `item_id`. Cross-platform (runs
    /// on the frozen `.boxes.json`, no live engine). Run:
    ///   cargo test -p ez-wishlist-overlay dump_stash_unit_tiles -- --ignored --nocapture
    #[test]
    #[ignore = "authoring aid — print (shot, item_id, name bbox) per stash tile"]
    fn dump_stash_unit_tiles() {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        println!("<<<STASH_TILES>>>");
        for i in 0..10 {
            let fx = BoxFixture::load("stash", &format!("stash.shot{i:02}.boxes.json"));
            let boxes = fx.label_boxes();
            let refs: Vec<&LabelBox> = boxes.iter().collect();
            let med_h = median(refs.iter().map(|b| b.h)).max(1.0);
            let slope = shear_slope(&refs, med_h);
            for block in &cluster_rows(&refs, slope, med_h * 4.0) {
                for sub in split_subrows(block, slope, med_h) {
                    let cells = split_tiles(&sub);
                    let cat = cells.iter().filter(|t| is_category_tile(t)).count();
                    if cat > 0 && cat * 2 >= cells.len() {
                        continue; // category sub-row (tab strip / subtitles)
                    }
                    for t in &cells {
                        let tokens: Vec<&str> = t.iter().map(|b| b.text.as_str()).collect();
                        if let Some(id) = match_item(&data, &tokens) {
                            let x0 = t.iter().map(|b| b.x).fold(f32::INFINITY, f32::min);
                            let y0 = t.iter().map(|b| b.y).fold(f32::INFINITY, f32::min);
                            let x1 = t
                                .iter()
                                .map(|b| b.right())
                                .fold(f32::NEG_INFINITY, f32::max);
                            let y1 = t
                                .iter()
                                .map(|b| b.y + b.h)
                                .fold(f32::NEG_INFINITY, f32::max);
                            println!(
                                "{i:02}\t{id}\t{x0:.0}\t{y0:.0}\t{x1:.0}\t{y1:.0}\t{}",
                                tokens.join(" ")
                            );
                        }
                    }
                }
            }
        }
        println!("<<<END_STASH_TILES>>>");
    }

    /// `box` scan — the world container tablet (3 overlapping scroll shots) →
    /// its full 22-tile contents. The de-sheared, layout-aware `read_tiles`
    /// produces stable rows across the three shots, so `merge_capture` dedups the
    /// overlapping rows and the tally matches `box.label.txt` exactly (issue #109).
    #[test]
    fn box_scan_matches_label() {
        let shots: Vec<String> = (0..3).map(|i| format!("box.shot{i}.boxes.json")).collect();
        assert_eq!(
            run_box_scan("box", &shots),
            load_box_label("box", "box.label.txt")
        );
    }

    /// `stash` scan — the Johnny's-Service submit terminal (10 scroll shots) →
    /// its full contents.
    ///
    /// Row-uniqueness fixed one of the two old blockers: the OCR dropping whole
    /// labels (shot02 missing "Tape"; "Wire Cutter"→"Cutter") used to shift later
    /// columns and break the rigid position stitch — but the row merge tolerates
    /// one drifted/missing tile, so those rows now dedup correctly.
    ///
    /// What remains is genuine **scroll gaps**: shots 00–04 share a full row each
    /// (clean scroll), but 04→05→06→07→08→09 share *no* row — some grid rows fall
    /// between shots and appear in no capture at all. Those rows are missing data;
    /// no merge can invent them, so the tally under-counts and this stays
    /// `#[ignore]`d. Un-ignoring needs better captures (no row skipped between
    /// shots). `stash.label.txt` is kept as a verified reference of the contents;
    /// see `screenshots/CLAUDE.md`. The `eval_report_json` diagnostic still scores
    /// this scan's partial tile accuracy (a graded signal, not a gate).
    #[test]
    #[ignore]
    fn stash_scan_matches_label() {
        let shots: Vec<String> = (0..10)
            .map(|i| format!("stash.shot{i:02}.boxes.json"))
            .collect();
        assert_eq!(
            run_box_scan("stash", &shots),
            load_box_label("stash", "stash.label.txt")
        );
    }

    /// Merge a series of captures (each a list of rows) and return the final
    /// unique-row set. `seq` builds one row's tiles from a token spec.
    fn merge_all(captures: &[Vec<Vec<Tile>>]) -> Vec<ScanRow> {
        let mut master = Vec::new();
        let mut next = 0u64;
        for cap in captures {
            merge_capture(&mut master, &mut next, cap);
        }
        master
    }

    #[test]
    fn rows_match_identical() {
        assert!(rows_match(&seq("a b c"), &seq("a b c")));
    }

    #[test]
    fn rows_match_tolerates_one_drift_on_big_rows() {
        // ≥3-item row: one drifted tile (c→x) or one missing tile (→_) still
        // identifies the same physical row.
        assert!(rows_match(&seq("a b c"), &seq("a b x")));
        assert!(rows_match(&seq("a b c"), &seq("a b _")));
    }

    #[test]
    fn rows_match_rejects_two_drifts() {
        // Two tiles differ → too far apart to be the same row.
        assert!(!rows_match(&seq("a b c"), &seq("a x y")));
    }

    #[test]
    fn rows_match_short_rows_need_exact() {
        // 1–2 item rows carry too little signal to tolerate a drift: exact only.
        assert!(rows_match(&seq("a b"), &seq("a b")));
        assert!(!rows_match(&seq("a b"), &seq("a x")));
        assert!(rows_match(&seq("a"), &seq("a")));
        assert!(!rows_match(&seq("a"), &seq("b")));
        // Different known-lengths are different rows.
        assert!(!rows_match(&seq("a b"), &seq("a")));
    }

    #[test]
    fn rows_match_ignores_order_and_unknowns() {
        // Shear can reorder a row; an unrecognized tile neither confirms nor
        // breaks a match (it's dropped from the comparison).
        assert!(rows_match(&seq("a b c"), &seq("c b a")));
        assert!(rows_match(&seq("a b c _"), &seq("a b c")));
    }

    #[test]
    fn rows_match_all_unknown_never_matches() {
        assert!(!rows_match(&seq("_ _"), &seq("_ _")));
    }

    #[test]
    fn seeds_then_dedups_overlap() {
        // Capture 1: [a b c],[d e f]. Capture 2 overlaps on [d e f], reveals [g h i].
        let master = merge_all(&[
            vec![seq("a b c"), seq("d e f")],
            vec![seq("d e f"), seq("g h i")],
        ]);
        assert_eq!(master.len(), 3); // [d e f] deduped, not appended twice
        let (counts, unrecognized) = tally_rows(&master);
        assert_eq!(unrecognized, 0);
        assert_eq!(counts.len(), 9);
        assert!(counts.values().all(|&c| c == 1));
    }

    #[test]
    fn merge_reports_added_and_duplicate() {
        let mut master = Vec::new();
        let mut next = 0u64;
        let m1 = merge_capture(&mut master, &mut next, &[seq("a b c"), seq("d e f")]);
        assert_eq!(
            m1,
            CaptureMerge {
                rows_added: 2,
                rows_duplicate: 0
            }
        );
        let m2 = merge_capture(&mut master, &mut next, &[seq("d e f"), seq("g h i")]);
        assert_eq!(
            m2,
            CaptureMerge {
                rows_added: 1,
                rows_duplicate: 1
            }
        );
    }

    #[test]
    fn dedups_overlap_despite_one_drift() {
        // The shared row re-reads with one tile drifted (f→x) — still recognized
        // as the same row, so it isn't double-counted.
        let master = merge_all(&[
            vec![seq("a b c"), seq("d e f")],
            vec![seq("d e x"), seq("g h i")],
        ]);
        assert_eq!(master.len(), 3);
    }

    #[test]
    fn keeps_distinct_rows_sharing_one_item() {
        // Two rows that each hold a piezometer but are otherwise different are
        // distinct rows — the duplicate item is NOT collapsed (it shares only one
        // of three items, below the one-drift threshold).
        let master = merge_all(&[vec![seq("a piezometer b"), seq("c piezometer d")]]);
        assert_eq!(master.len(), 2);
        let (counts, _) = tally_rows(&master);
        assert_eq!(counts.get("piezometer"), Some(&2));
    }

    #[test]
    fn collapses_identical_rows_documented_undercount() {
        // Two DISTINCT physical rows with the same composition collapse to one —
        // the known under-count cost of row uniqueness, surfaced for manual fixup
        // in the desktop review step.
        let master = merge_all(&[vec![seq("nail nail nail"), seq("nail nail nail")]]);
        assert_eq!(master.len(), 1);
    }

    #[test]
    fn ids_are_unique_and_increasing() {
        let master = merge_all(&[vec![seq("a b c"), seq("d e f")], vec![seq("g h i")]]);
        let ids: Vec<u64> = master.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn tally_counts_duplicates_and_unknowns() {
        let master = seq("nail nail _ screw nail _");
        let (counts, unrecognized) = tally(&master);
        assert_eq!(counts.get("nail"), Some(&3));
        assert_eq!(counts.get("screw"), Some(&1));
        assert_eq!(unrecognized, 2);
    }

    // --- OCR geometry (platform-independent clustering) ----------------------

    use crate::data::Item;

    fn item(id: &str, name: &str) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            icon_path: String::new(),
            category: None,
            subcategory: None,
            weight: None,
            price: None,
            rarity: None,
        }
    }

    fn box_data() -> GameData {
        GameData {
            data_version: "test".into(),
            scraped_at: "test".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: Vec::new(),
            items: vec![
                item("uvlight", "UV lamp"),
                item("copperwire", "Copper wire"),
                item("piezometer", "Piezometer"),
                item("oliveoil", "Olive oil"),
                item("gunoil", "Gun oil"),
                item("gunpowder", "Gunpowder"),
            ],
        }
    }

    fn lb(text: &str, x: f32, y: f32) -> LabelBox {
        LabelBox {
            text: text.into(),
            x,
            y,
            w: 30.0,
            h: 18.0,
        }
    }

    #[test]
    fn extract_weight_prefers_decimal_over_capacity() {
        // "21.94" (used) and "30" (capacity) both sit in the bottom strip; only
        // the fractional one is the weight.
        let boxes = vec![
            lb("21.94", 600.0, 430.0),
            lb("/", 660.0, 430.0),
            lb("30", 690.0, 430.0),
        ];
        let w = extract_weight(&boxes, 500.0);
        assert!(matches!(w, Some((v, _)) if (v - 21.94).abs() < 0.01));
    }

    #[test]
    fn read_tiles_clusters_grid_excludes_tabs_and_weight() {
        let data = box_data();
        let boxes = vec![
            // Category-tab strip (fixed chrome — must be excluded).
            lb("ALL", 50.0, 40.0),
            lb("Building", 120.0, 40.0),
            lb("Electric", 230.0, 40.0),
            // Row 1: two multi-word labels + one single-word.
            lb("UV", 50.0, 120.0),
            lb("lamp", 85.0, 120.0),
            lb("Copper", 200.0, 120.0),
            lb("wire", 255.0, 120.0),
            lb("Piezometer", 360.0, 120.0),
            // Row 2.
            lb("Olive", 50.0, 200.0),
            lb("oil", 85.0, 200.0),
            lb("Gun", 200.0, 200.0),
            lb("oil", 235.0, 200.0),
            lb("Gunpowder", 360.0, 200.0),
            // Weight readout (fixed chrome).
            lb("21.94", 600.0, 430.0),
        ];
        let res = read_tiles(&boxes, 500.0, &data);
        assert_eq!(
            res.tiles,
            vec![
                Some("uvlight".to_string()),
                Some("copperwire".to_string()),
                Some("piezometer".to_string()),
                Some("oliveoil".to_string()),
                Some("gunoil".to_string()),
                Some("gunpowder".to_string()),
            ]
        );
        assert!(matches!(res.observed_weight, Some(v) if (v - 21.94).abs() < 0.01));
    }

    #[test]
    fn read_tiles_marks_unrecognized_tiles_as_none() {
        let data = box_data();
        let boxes = vec![
            lb("Piezometer", 50.0, 120.0),
            lb("Kalashnikov", 200.0, 120.0), // not in the vocab
            lb("Gunpowder", 360.0, 120.0),
        ];
        let res = read_tiles(&boxes, 500.0, &data);
        assert_eq!(
            res.tiles,
            vec![
                Some("piezometer".to_string()),
                None,
                Some("gunpowder".to_string())
            ]
        );
    }

    #[test]
    fn tile_marks_filters_to_names_reshears_and_normalizes() {
        use crate::ocr::CropMark;
        // Two Names tiles (one matched, one not) on a sheared row, plus a
        // Category row that must contribute no marks.
        let rows = vec![
            RowReport {
                ry: 100.0,
                kind: RowKind::Names,
                cells: vec![
                    ("Piezometer".into(), Some("piezometer".to_string())),
                    ("Kalashnikov".into(), None),
                ],
                cxs: vec![100.0, 300.0],
            },
            RowReport {
                ry: 40.0,
                kind: RowKind::Category,
                cells: vec![("ALL".into(), None)],
                cxs: vec![50.0],
            },
        ];
        let slope = 0.1;
        let (img_w, img_h) = (400.0, 500.0);
        let marks = tile_marks(&rows, slope, img_w, img_h);

        // Only the two Names tiles produce marks; the Category row is skipped.
        assert_eq!(marks.len(), 2);
        assert!(marks[0].matched, "recognized tile → ✓");
        assert!(!marks[1].matched, "unrecognized tile → ✗");

        // Each mark's y is the de-sheared `ry` re-sheared back to that tile's
        // own column (`ry + slope*cx`), then normalized by the image dims; x is
        // the raw center normalized straight.
        let expect = |cx: f32| CropMark::from_px(cx, 100.0 + slope * cx, img_w, img_h);
        assert_eq!(marks[0].pos, expect(100.0));
        assert_eq!(marks[1].pos, expect(300.0));
        assert!((marks[0].pos.x - 0.25).abs() < 1e-6);
        assert!((marks[1].pos.x - 0.75).abs() < 1e-6);
    }

    #[test]
    fn read_tiles_records_row_classification() {
        let data = box_data();
        let boxes = vec![
            // Tab strip → Category, the weight row → Chrome, a real row → Names.
            lb("ALL", 50.0, 40.0),
            lb("Building", 120.0, 40.0),
            lb("Electric", 230.0, 40.0),
            lb("UV", 50.0, 120.0),
            lb("lamp", 85.0, 120.0),
            lb("Piezometer", 360.0, 120.0),
            lb("21.94", 600.0, 430.0),
        ];
        let res = read_tiles(&boxes, 500.0, &data);
        let kinds: Vec<RowKind> = res.rows.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&RowKind::Category), "tab strip → Category");
        assert!(kinds.contains(&RowKind::Names), "item row → Names");
        assert!(kinds.contains(&RowKind::Chrome), "weight row → Chrome");
        // Only the Names row contributes tiles.
        assert_eq!(
            res.tiles,
            vec![Some("uvlight".into()), Some("piezometer".into())]
        );
    }

    #[test]
    fn format_capture_dump_renders_verdict_rows_and_tally() {
        let data = box_data();
        let boxes = vec![
            lb("ALL", 50.0, 40.0),
            lb("Building", 120.0, 40.0),
            lb("Electric", 230.0, 40.0),
            lb("UV", 50.0, 120.0),
            lb("lamp", 85.0, 120.0),
            lb("Piezometer", 360.0, 120.0),
            lb("21.94", 600.0, 430.0),
        ];
        let res = read_tiles(&boxes, 500.0, &data);
        let mut master = Vec::new();
        let mut next_id = 0u64;
        let merge = merge_capture(&mut master, &mut next_id, &res.tile_rows);
        let (counts, unrecognized) = tally_rows(&master);
        let dump = format_capture_dump(&res, merge, &counts, unrecognized, 1);
        assert!(dump.contains("BOX-SCAN CAPTURE #1"));
        assert!(dump.contains("shear slope"));
        assert!(dump.contains("new row")); // merge summary: "+N new row(s)"
        assert!(dump.contains("uvlight")); // resolved tile shown in the tally
        assert!(dump.contains("(skipped: tab strip / subtitles)"));
        // Concise per-row reconstruction sits above the verbose trace.
        assert!(dump.contains("CAPTURED ITEMS (this shot, per row)"));
        assert!(dump.contains("row  1: uvlight"));
    }
}
