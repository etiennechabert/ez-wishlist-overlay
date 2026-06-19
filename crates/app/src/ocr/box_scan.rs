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
use crate::ocr::{GridCell, GridRow};
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
    /// One entry per `Names` sub-row: the tiles composing that grid row, in
    /// reading order. This is the unit fed to [`merge_capture`] — the merge
    /// dedups whole rows, never individual tiles (on a multi-round burst the
    /// rounds' rows pass through [`union_rounds`] first).
    pub tile_rows: Vec<Vec<Tile>>,
    pub observed_weight: Option<f32>,
    /// Estimated perspective-shear slope used to de-tilt rows ([`shear_slope`]).
    pub slope: f32,
    /// Per-sub-row recognition trace, in reading order.
    pub rows: Vec<RowReport>,
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
    /// Pixel x-center of each cell, aligned 1:1 with [`cells`](Self::cells).
    /// (The de-shear only adjusts `y`, so a cell's horizontal midpoint is its
    /// raw center.) Consumed by [`grid_rows`] to place the per-shot feedback
    /// marks at the right column.
    pub cxs: Vec<f32>,
    /// Pixel text height of each cell (median of its boxes' heights), aligned
    /// 1:1 with [`cells`](Self::cells). Used by [`dedup_icon_labels`] to tell an
    /// icon-printed name (rendered larger on the box art) from the real tile
    /// name below it.
    pub chs: Vec<f32>,
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

/// De-sheared vertical gap (as a fraction of median box height) above which
/// [`split_subrows`] starts a new sub-row. See that function for why `0.6`.
const SUBROW_GAP_FRAC: f32 = 0.6;

/// Split one generous grid-row *block* into sub-rows at de-sheared vertical gaps
/// wider than [`SUBROW_GAP_FRAC`]·med_h. A box tile stacks the item **name** over
/// its **category subtitle** (~1.3 text heights apart), so this separates the two;
/// the subtitle sub-row is then dropped while the name survives. A block with a
/// single sub-row (the names-only tablet layout, or a lone chrome line) returns
/// unsplit.
///
/// The threshold sits between the **within-row** consecutive de-sheared gap
/// (adjacent tiles across one row — a few px, even with an imperfect slope) and
/// the **name↔category** gap (~30 px). The original `1.1·med_h` landed right on
/// the name↔category gap once the PP-OCR engine's unclip inflated `med_h` (#181),
/// so under a steeper VR viewing angle the name and its subtitle merged into one
/// unmatchable "Name Category Name" row that got dropped — losing whole grid rows
/// from a scan (#182). `0.6` clears the within-row gap with margin while still
/// splitting the name from its subtitle.
#[allow(dead_code)]
fn split_subrows<'a>(block: &[&'a LabelBox], slope: f32, med_h: f32) -> Vec<Vec<&'a LabelBox>> {
    let mut sorted = block.to_vec();
    sorted.sort_by(|a, b| deshear(a, slope).total_cmp(&deshear(b, slope)));
    let gap = (med_h * SUBROW_GAP_FRAC).max(1.0);

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
/// label sit close together, while the gap between tiles is wider. Returns tiles
/// left→right, each a list of its label words (left→right).
///
/// The break threshold is **adaptive** ([`split_threshold`]) rather than a flat
/// `1.5·med_h`. A flat threshold fuses dense grids whose long, cell-filling names
/// leave only a narrow inter-column gap: the "Magazine & Attachments" box (#197)
/// packs names like `AK74 P-Mag 5.45x39mm 30rnd magazine` so the inter-column gap
/// is ~0.8–1.4·med_h while intra-name word gaps are ~0.3–0.5·med_h — both *below*
/// `1.5·med_h`, so the whole row collapsed into one unmatchable blob. The Otsu
/// valley between the two gap clusters, floored at `0.6·med_h` (above any
/// intra-name gap) and capped at the old `1.5·med_h` (so we only ever split
/// *more*), separates the columns; on the well-spaced misc box / stash / gunsmith
/// fixtures it reproduces the old splits exactly (their intra gaps clear the
/// floor by a wide margin), so those gates are unaffected.
#[allow(dead_code)]
fn split_tiles<'a>(row: &[&'a LabelBox]) -> Vec<Vec<&'a LabelBox>> {
    let mut sorted = row.to_vec();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
    let med_h = median(sorted.iter().map(|b| b.h)).max(1.0);
    let gaps: Vec<f32> = sorted
        .windows(2)
        .map(|w| (w[1].x - w[0].right()).max(0.0))
        .collect();
    let thresh = split_threshold(&gaps, med_h);

    let mut tiles: Vec<Vec<&LabelBox>> = Vec::new();
    let mut cur: Vec<&LabelBox> = Vec::new();
    let mut prev_right = f32::NEG_INFINITY;
    for b in sorted {
        if !cur.is_empty() && b.x - prev_right >= thresh {
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

/// True when a word-box's text ends in the boilerplate word "magazine" (after
/// trimming trailing punctuation) — the per-tile anchor every magazine name
/// shares, even when the engine glued it to the preceding token
/// (`"10rndmagazine"`).
#[allow(dead_code)]
fn ends_magazine(text: &str) -> bool {
    text.to_lowercase()
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .ends_with("magazine")
}

/// Re-split a gun-part row's tiles at their "magazine"-word anchors. A cell
/// carrying **≥2** boxes that end in "magazine" is a fused multi-magazine blob
/// (each single magazine name has exactly one "magazine" word), so it is cut
/// after each anchor — recovering the columns that [`split_tiles`] left fused
/// because the dense grid's inter-column gap was below the per-row split
/// threshold. The trailing run after the last anchor is kept (it holds the last
/// magazine, whose suffix may be OCR-truncated to "magazin"). Cells with <2
/// anchors pass through untouched, so a single part name (one anchor or none) is
/// never fragmented. Called only on `Some("gunsmith")` reads, so the misc
/// box/stash path and the box-exact gate are unaffected; gun-part **storage**
/// tiles show short aliases (no "magazine" word), so they never trigger it.
#[allow(dead_code)]
fn resplit_magazine_runs(cells: Vec<Vec<&LabelBox>>) -> Vec<Vec<&LabelBox>> {
    let mut out: Vec<Vec<&LabelBox>> = Vec::with_capacity(cells.len());
    for cell in cells {
        if cell.iter().filter(|b| ends_magazine(&b.text)).count() < 2 {
            out.push(cell);
            continue;
        }
        let mut cur: Vec<&LabelBox> = Vec::new();
        for b in cell {
            let anchor = ends_magazine(&b.text);
            cur.push(b);
            if anchor {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// The horizontal gap (px) at or above which a break ends one tile and begins
/// the next, for one row's consecutive word-box gaps. Splits the gaps into an
/// intra-name cluster and an inter-column cluster at their Otsu valley, then
/// clamps that to `[0.6·med_h, 1.5·med_h]`: the floor sits above the *typical*
/// intra-name word gap, and the cap is the legacy flat threshold.
///
/// Two boundary caveats (verified by the #197 review, both absorbed by the
/// floor-gated magbox — the exact box/stash/gunsmith fixtures don't hit them):
/// - **Over-split:** the floor protects a name only when *all* its internal gaps
///   are below `0.6·med_h`. A name with one internal gap in `[0.6, 1.5]·med_h`
///   (the lone "high" value) is the Otsu valley and fragments — see the
///   `split_threshold_finds_the_column_valley` boundary case.
/// - **Under-split:** a single dominant gap (an empty grid cell ≫ the real
///   inter-column gap) maximizes Otsu's variance on its own, pulling the valley
///   up so the genuine narrower inter-column gaps in that row stay fused. The
///   gunsmith-scoped [`resplit_magazine_runs`] recovers those magazine rows
///   downstream.
#[allow(dead_code)]
fn split_threshold(gaps: &[f32], med_h: f32) -> f32 {
    let cap = med_h * 1.5;
    let floor = med_h * 0.6;
    otsu_break(gaps).unwrap_or(cap).clamp(floor, cap)
}

/// Otsu's method on a small set of 1-D gap values: the gap value that, used as a
/// `< t` / `>= t` split, maximizes between-class variance — i.e. the valley of a
/// bimodal gap distribution. `None` when there's nothing to split (< 2 gaps).
#[allow(dead_code)]
fn otsu_break(gaps: &[f32]) -> Option<f32> {
    if gaps.len() < 2 {
        return None;
    }
    let mut cands: Vec<f32> = gaps.to_vec();
    cands.sort_by(f32::total_cmp);
    cands.dedup();
    let n = gaps.len() as f32;
    let mut best_var = -1.0f32;
    let mut best_t = None;
    for &c in cands.iter().skip(1) {
        let lo: Vec<f32> = gaps.iter().copied().filter(|&g| g < c).collect();
        let hi: Vec<f32> = gaps.iter().copied().filter(|&g| g >= c).collect();
        if lo.is_empty() || hi.is_empty() {
            continue;
        }
        let (w0, w1) = (lo.len() as f32 / n, hi.len() as f32 / n);
        let m0 = lo.iter().sum::<f32>() / lo.len() as f32;
        let m1 = hi.iter().sum::<f32>() / hi.len() as f32;
        let var = w0 * w1 * (m0 - m1).powi(2);
        if var > best_var {
            best_var = var;
            best_t = Some(c);
        }
    }
    best_t
}

/// De-sheared vertical gap (×med_h) under which an upper `Names` row is close
/// enough to a lower one to be holding the lower tile's icon-printed name.
/// ≈0.6 of a grid-row pitch (~11.8·med_h on stash): above the icon-label→name
/// gap (~5·med_h) but below a genuine adjacent grid row (~11·med_h). A per-shot
/// pitch estimate was rejected — it collapses on icon-print-heavy frames where
/// every row gap is sub-pitch — so the threshold is med_h-relative; the
/// mandatory same-column test (below) is what keeps the box gate safe under it.
#[allow(dead_code)]
const ICON_LABEL_VGAP_FRAC: f32 = 7.0;

/// How much taller (×) an upper tile's glyphs must be than the lower's to count
/// as the icon-printed name rather than a real tile name. The box art renders
/// the printed product name ~1.3× the tile-name size (e.g. "ASPIRIN" h≈38 over
/// "Aspirin" h≈28); 1.15 keeps margin while still rejecting a same-height real
/// name that happens to sit above the next row's icon print.
#[allow(dead_code)]
const ICON_LABEL_MIN_TALLER: f32 = 1.15;

/// Median adjacent-tile x-gap within a row's cell centers (its column pitch),
/// falling back to 4·med_h when the row has fewer than two cells.
#[allow(dead_code)]
fn col_pitch(cxs: &[f32], med_h: f32) -> f32 {
    if cxs.len() < 2 {
        return 4.0 * med_h;
    }
    let mut sorted = cxs.to_vec();
    sorted.sort_by(f32::total_cmp);
    median(sorted.windows(2).map(|w| w[1] - w[0])).max(med_h)
}

/// Drop the icon-printed product name some box tiles carry on their art (e.g.
/// "ASPIRIN" on the Aspirin box, "Box Nails" on Boxed Nails). It OCRs as a
/// second text box ~one icon-height above the real tile name in the SAME grid
/// cell and resolves to the SAME item, so left in it survives as a phantom tile
/// that double-counts the item (the live box/stash `EXTRA` aspire/nail).
///
/// For each pair of `Names` rows where the upper sits within
/// [`ICON_LABEL_VGAP_FRAC`]·med_h above the lower, demote an upper tile that
/// duplicates a lower tile to `None` when ALL of: (1) **same item** (same
/// resolved id); (2) **same column** (x-centers within half the lower row's
/// column pitch); (3) **upper is the icon print** — its glyphs are taller
/// ([`ICON_LABEL_MIN_TALLER`]×) than the lower's, since the box art renders the
/// printed name larger than the tile's real name.
///
/// Keyed this way, legitimately repeated items are all untouched: side by side
/// in one row (same ry, fails the vertical test); a full grid-row apart (fails
/// the gap); in a different column (fails the column test); and — critically —
/// a *real* name row sitting above the *next* row's icon print is spared by the
/// height test (the real name is not taller than an icon print). A row emptied
/// to all-`None` becomes `Chrome`. The box gate's only same-item vertical
/// neighbours wrap to a different column, so it is doubly safe.
#[allow(dead_code)]
fn dedup_icon_labels(rows: &mut [RowReport], med_h: f32) {
    let vgap = med_h * ICON_LABEL_VGAP_FRAC;
    let names: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::Names)
        .map(|(i, _)| i)
        .collect();

    // Immutable pass collects (row, cell) demotions, then apply (avoids
    // aliasing two &mut into `rows`).
    let mut demote: Vec<(usize, usize)> = Vec::new();
    for &iu in &names {
        for &il in &names {
            if iu == il {
                continue;
            }
            let dv = rows[il].ry - rows[iu].ry; // upper `iu` above lower `il`
            if dv <= 0.0 || dv >= vgap {
                continue;
            }
            let xtol = 0.5 * col_pitch(&rows[il].cxs, med_h);
            for (cu, (_, tu)) in rows[iu].cells.iter().enumerate() {
                let Some(idu) = tu.as_ref() else { continue };
                let cxu = rows[iu].cxs[cu];
                let hu = rows[iu].chs.get(cu).copied().unwrap_or(med_h);
                let is_icon_dup = rows[il]
                    .cells
                    .iter()
                    .zip(&rows[il].cxs)
                    .zip(&rows[il].chs)
                    .any(|(((_, tl), &cxl), &hl)| {
                        tl.as_ref() == Some(idu)
                            && (cxu - cxl).abs() < xtol
                            && hu > hl * ICON_LABEL_MIN_TALLER
                    });
                if is_icon_dup {
                    demote.push((iu, cu));
                }
            }
        }
    }
    for (ri, ci) in demote {
        rows[ri].cells[ci].1 = None;
    }
    for r in rows.iter_mut() {
        if r.kind == RowKind::Names && r.cells.iter().all(|(_, t)| t.is_none()) {
            r.kind = RowKind::Chrome;
        }
    }
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
/// `scope` is the catalog category the box is locked to in-game (`misc` /
/// `medical` / `gunsmith`, or `None` for a generic/legacy case): tiles only
/// resolve within it, and `Some("gunsmith")` additionally enables the gun-part
/// short-name matcher (issue #183). Forwarded straight to [`match_item`].
#[allow(dead_code)]
pub fn read_tiles(
    boxes: &[LabelBox],
    img_h: f32,
    data: &GameData,
    scope: Option<&str>,
) -> BoxReadResult {
    let observed_weight = extract_weight(boxes, img_h).map(|(v, _)| v);

    let refs: Vec<&LabelBox> = boxes.iter().collect();
    let med_h = median(refs.iter().map(|b| b.h)).max(1.0);
    let slope = shear_slope(&refs, med_h);

    // Generous blocks: a name and its category subtitle (~2 text heights apart)
    // land together, while neighbouring grid rows (~10 text heights apart) stay
    // separate even if the slope estimate is a little off.
    let blocks = cluster_rows(&refs, slope, med_h * 4.0);

    let mut rows: Vec<RowReport> = Vec::new();
    for block in &blocks {
        for sub in split_subrows(block, slope, med_h) {
            let ry = median(sub.iter().map(|b| deshear(b, slope)));
            let cells = split_tiles(&sub);
            // Gun-part boxes (the Magazine & Attachments box) can pack a whole
            // row of long magazine names into one OCR run when the inter-column
            // gap is too narrow to split geometrically (the misc box's larger
            // intra-name gaps would over-split if `split_tiles` chased it — so
            // that fix lives here, scoped). Re-split such a fused blob at its
            // "magazine"-word anchors so each magazine resolves on its own.
            // Gunsmith-scoped + guarded on >=2 anchors, so the misc box/stash and
            // single-name gun parts are untouched. See [`resplit_magazine_runs`].
            let cells = if scope == Some("gunsmith") {
                resplit_magazine_runs(cells)
            } else {
                cells
            };
            // x-center of each tile, aligned 1:1 with the cells below — carried
            // on every RowReport so the feedback overlays can place per-cell
            // marks at the right column (#137 / #138).
            let cxs: Vec<f32> = cells.iter().map(|t| tile_cx(t)).collect();
            let chs: Vec<f32> = cells
                .iter()
                .map(|t| median(t.iter().map(|b| b.h)))
                .collect();

            // Skip a category sub-row (tab strip or per-item subtitles): one whose
            // tiles are mostly category words.
            let cat = cells.iter().filter(|t| is_category_tile(t)).count();
            if cat > 0 && cat * 2 >= cells.len() {
                rows.push(RowReport {
                    ry,
                    kind: RowKind::Category,
                    cells: cells.iter().map(|t| (join_text(t), None)).collect(),
                    cxs,
                    chs,
                });
                continue;
            }

            // Resolve each tile; drop an all-unrecognized sub-row as chrome.
            let resolved: Vec<(String, Tile)> = cells
                .iter()
                .map(|t| {
                    let tokens: Vec<&str> = t.iter().map(|b| b.text.as_str()).collect();
                    (join_text(t), match_item(data, &tokens, scope))
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
                cells: resolved,
                cxs,
                chs,
            });
        }
    }

    // Drop icon-printed-name duplicates before building the tile grid, so both
    // the single-shot `tile_rows` and the burst `round_rows` (which read
    // `rows`) see the deduped result.
    dedup_icon_labels(&mut rows, med_h);

    let tile_rows: Vec<Vec<Tile>> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Names)
        .map(|r| r.cells.iter().map(|(_, t)| t.clone()).collect())
        .collect();

    BoxReadResult {
        tile_rows,
        observed_weight,
        slope,
        rows,
    }
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

/// Horizontal pixel center of a tile — midpoint of its words' combined span.
#[allow(dead_code)]
fn tile_cx(tile: &[&LabelBox]) -> f32 {
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

/// Build the normalized per-shot tile grid for the feedback overlays (the
/// mini-grid card #138 and the on-the-items markers #137).
///
/// `rows` are a read's [`round_rows`] — already just the [`RowKind::Names`]
/// rows (the real item grid; tab strips, the weight readout, and other chrome
/// were dropped) — or the round-fused rows of a burst shot ([`union_rounds`]),
/// so the marks reflect what actually fed the merge, fills included. Maps each
/// cell to a [`GridCell`] at its normalized x-center (✓ when it matched a
/// catalog item, ✗ when it was detected but unreadable), and each row to its
/// normalized y-center. `img_w` / `img_h` are the cropped frame's pixel
/// dimensions the OCR ran on, so the output is in the same crop space as the
/// guide box. Pure and cross-platform — unit-tested on every target.
pub fn grid_rows(rows: &[RoundRow], img_w: f32, img_h: f32) -> Vec<GridRow> {
    let iw = img_w.max(1.0);
    let ih = img_h.max(1.0);
    rows.iter()
        .map(|r| GridRow {
            y: (r.ry / ih).clamp(0.0, 1.0),
            cells: r
                .cells
                .iter()
                .map(|(cx, tile)| GridCell {
                    x: (cx / iw).clamp(0.0, 1.0),
                    matched: tile.is_some(),
                })
                .collect(),
        })
        .collect()
}

// ===========================================================================
// Round union (issue #165): one trigger pull can burst-capture several mirror
// frames ~100 ms apart and OCR each independently. Re-OCRing one frame is
// deterministic — two engine passes over the same pixels return identical
// words — but in VR the head is never perfectly still, so consecutive frames
// are genuinely different pixels, and a busy-icon tile (RAM above all) that
// OCRs to nothing in one frame often resolves in another. The union below
// fuses those per-round reads into one capture *before* [`merge_capture`].
// Pure and platform-independent, like the merge core.
// ===========================================================================

/// One grid row of a single round's read, carrying the geometry the
/// cross-round union needs: each tile's pixel x-center (to align the *same
/// physical tile* across rounds) and the de-sheared row position (kept for
/// the feedback overlays). Extracted from a [`BoxReadResult`] by
/// [`round_rows`], aligned 1:1 with [`BoxReadResult::tile_rows`].
#[derive(Clone, Debug, PartialEq)]
pub struct RoundRow {
    /// De-sheared vertical row position in the cropped frame. Display only —
    /// the union identifies rows by composition, never by position.
    pub ry: f32,
    /// `(x_center, tile)` per detected tile, left→right.
    pub cells: Vec<(f32, Tile)>,
}

impl RoundRow {
    /// The row's tiles without geometry — the shape [`merge_capture`] eats.
    pub fn tiles(&self) -> Vec<Tile> {
        self.cells.iter().map(|(_, t)| t.clone()).collect()
    }
}

/// A read's `Names` rows with their geometry, aligned 1:1 with
/// [`BoxReadResult::tile_rows`].
pub fn round_rows(read: &BoxReadResult) -> Vec<RoundRow> {
    read.rows
        .iter()
        .filter(|r| r.kind == RowKind::Names)
        .map(|r| RoundRow {
            ry: r.ry,
            cells: r
                .cxs
                .iter()
                .zip(&r.cells)
                .map(|(&cx, (_, tile))| (cx, tile.clone()))
                .collect(),
        })
        .collect()
}

/// One round's sighting of a physical row, tiles pre-extracted so the
/// grouping loop doesn't re-collect them per comparison.
struct Sighting<'a> {
    round: usize,
    row: &'a RoundRow,
    tiles: Vec<Tile>,
}

/// Fuse several rounds' reads of the same view into one capture's rows.
///
/// Rows are identified across rounds with [`rows_match`] — two rows of the
/// *same* round are never grouped (two rows in one frame are distinct
/// physical rows). Each group keeps the union of its sightings' recognized
/// tiles: the sightings' cells are clustered into columns by x-center
/// (cross-round head jitter is far smaller than the grid's column pitch),
/// and each column resolves to one tile — a tile read in any round counts,
/// disagreeing reads resolve by majority, ties go to the earliest round.
/// Data-safe by construction: it only fills unknowns / votes within a row
/// already identified as the same physical row, so it can never invent a
/// row, and a row seen in a single round passes through unchanged. With one
/// round the whole call is a pass-through.
pub fn union_rounds(rounds: &[Vec<RoundRow>]) -> Vec<RoundRow> {
    // Group sightings of the same physical row, in first-sighting order.
    let mut groups: Vec<Vec<Sighting>> = Vec::new();
    for (round, rows) in rounds.iter().enumerate() {
        for row in rows {
            let tiles = row.tiles();
            // Best group: none of its sightings is from this round, and the
            // strongest item overlap among those `rows_match` accepts — so an
            // exact re-read prefers its own row over a one-drift neighbour.
            let mut best: Option<(usize, usize)> = None; // (shared, group idx)
            for (gi, g) in groups.iter().enumerate() {
                if g.iter().any(|s| s.round == round) {
                    continue;
                }
                let shared = g
                    .iter()
                    .filter(|s| rows_match(&s.tiles, &tiles))
                    .map(|s| shared_items(&s.tiles, &tiles))
                    .max();
                if let Some(shared) = shared {
                    if best.is_none_or(|(b, _)| shared > b) {
                        best = Some((shared, gi));
                    }
                }
            }
            let sighting = Sighting { round, row, tiles };
            match best {
                Some((_, gi)) => groups[gi].push(sighting),
                None => groups.push(vec![sighting]),
            }
        }
    }
    groups.iter().map(|g| fuse_row(g)).collect()
}

/// Fuse one group of sightings (the same physical row seen in ≥1 rounds)
/// into a single row. See [`union_rounds`] for the column/vote rules.
fn fuse_row(sightings: &[Sighting]) -> RoundRow {
    if sightings.len() == 1 {
        return sightings[0].row.clone();
    }
    // Column pitch: the tightest adjacent-tile gap any single sighting
    // exhibits. Half of it separates neighbouring columns while absorbing the
    // cross-round x jitter (head drift over ~100 ms is a small fraction of a
    // grid column). Infinite when every sighting is a single tile — then
    // `rows_match` already said they're the same one-item row, one column.
    let mut pitch = f32::INFINITY;
    for s in sightings {
        let mut xs: Vec<f32> = s.row.cells.iter().map(|&(cx, _)| cx).collect();
        xs.sort_by(f32::total_cmp);
        for w in xs.windows(2) {
            pitch = pitch.min(w[1] - w[0]);
        }
    }
    let tol = pitch * 0.5;

    // Cluster every sighting's cells into columns by x-center against the
    // running column mean (the same greedy shape as `cluster_rows`).
    let mut cells: Vec<(f32, usize, &Tile)> = sightings
        .iter()
        .flat_map(|s| s.row.cells.iter().map(|(cx, t)| (*cx, s.round, t)))
        .collect();
    cells.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut columns: Vec<Vec<(f32, usize, &Tile)>> = Vec::new();
    let mut mean = f32::NEG_INFINITY;
    for c in cells {
        match columns.last_mut() {
            Some(col) if (c.0 - mean).abs() <= tol => col.push(c),
            _ => columns.push(vec![c]),
        }
        let col = columns.last().expect("just pushed");
        mean = col.iter().map(|c| c.0).sum::<f32>() / col.len() as f32;
    }

    // One tile per column: a read in any round counts (fills a tile that
    // OCR'd to nothing or matched nothing elsewhere); disagreeing reads
    // resolve by majority, ties to the earliest round.
    struct FusedCell {
        cx: f32,
        tile: Tile,
        votes: usize,
        round: usize,
    }
    let mut fused: Vec<FusedCell> = columns
        .iter()
        .map(|col| {
            let cx = col.iter().map(|c| c.0).sum::<f32>() / col.len() as f32;
            let mut votes: Vec<(&ItemId, usize, usize)> = Vec::new(); // (id, count, first round)
            for &(_, round, tile) in col {
                if let Some(id) = tile {
                    match votes.iter_mut().find(|(v, _, _)| *v == id) {
                        Some(v) => {
                            v.1 += 1;
                            v.2 = v.2.min(round);
                        }
                        None => votes.push((id, 1, round)),
                    }
                }
            }
            let mut winner: Option<(&ItemId, usize, usize)> = None;
            for v in votes {
                let better = match winner {
                    None => true,
                    Some((_, count, round)) => v.1 > count || (v.1 == count && v.2 < round),
                };
                if better {
                    winner = Some(v);
                }
            }
            match winner {
                Some((id, votes, round)) => FusedCell {
                    cx,
                    tile: Some(id.clone()),
                    votes,
                    round,
                },
                None => FusedCell {
                    cx,
                    tile: None,
                    votes: 0,
                    round: usize::MAX,
                },
            }
        })
        .collect();

    // Data-safety cap: the union FILLS tiles, it must never MULTIPLY them.
    // Two columns claiming the same item when no single sighting saw that
    // many means the column clustering split one physical tile (e.g. the
    // stash's lone ALL-CAPS icon-print rows, identical in composition but
    // sighted at different x): the surplus column demotes to an
    // unrecognized tile instead of minting a duplicate. A row genuinely
    // holding N copies keeps them — some sighting saw the N tiles.
    let mut ids: Vec<ItemId> = fused.iter().filter_map(|c| c.tile.clone()).collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let allowed = sightings
            .iter()
            .map(|s| s.tiles.iter().filter(|t| t.as_ref() == Some(&id)).count())
            .max()
            .unwrap_or(0);
        let mut cols: Vec<usize> = (0..fused.len())
            .filter(|&i| fused[i].tile.as_ref() == Some(&id))
            .collect();
        if cols.len() <= allowed {
            continue;
        }
        // Keep the strongest `allowed` columns: most votes, then earliest
        // round, then leftmost. The rest demote.
        cols.sort_by(|&a, &b| {
            fused[b]
                .votes
                .cmp(&fused[a].votes)
                .then(fused[a].round.cmp(&fused[b].round))
                .then(fused[a].cx.total_cmp(&fused[b].cx))
        });
        for &i in cols.iter().skip(allowed) {
            fused[i].tile = None;
        }
    }

    RoundRow {
        ry: sightings.iter().map(|s| s.row.ry).sum::<f32>() / sightings.len() as f32,
        cells: fused.into_iter().map(|c| (c.cx, c.tile)).collect(),
    }
}

/// Render one tile row as its item ids, `_` for an unmatched tile, left→right.
#[allow(dead_code)]
fn format_tile_row(row: &[Tile]) -> String {
    row.iter()
        .map(|t| t.clone().unwrap_or_else(|| "_".to_string()))
        .collect::<Vec<String>>()
        .join("  ")
}

/// Render a human-readable recognition dump for one box-scan capture, for the
/// `ocr_debug` sidecar. Shows the de-shear slope, every sub-row's classification
/// and per-tile match, this shot's reading-order tiles, the stitch verdict, and
/// the running session tally — enough to see *why* a scan mis-read without the
/// game running. Pure (no I/O); the caller writes it next to the source PNG.
///
/// `reads` is one [`BoxReadResult`] per burst round (a single element on the
/// default 1-round setting) and `merged_rows` the round-unioned rows that
/// actually fed [`merge_capture`]. A multi-round dump additionally records
/// each round's raw per-row read next to the union, so a recovered (or still
/// missing) tile is observable per round.
#[allow(dead_code)]
pub fn format_capture_dump(
    reads: &[BoxReadResult],
    merged_rows: &[Vec<Tile>],
    outcome: CaptureMerge,
    tally: &HashMap<ItemId, u32>,
    unrecognized: usize,
    captures: u32,
) -> String {
    use std::fmt::Write as _;
    let multi = reads.len() > 1;
    let mut s = String::new();
    if multi {
        let _ = writeln!(
            s,
            "=== BOX-SCAN CAPTURE #{captures} ({} rounds) ===",
            reads.len()
        );
    } else {
        let _ = writeln!(s, "=== BOX-SCAN CAPTURE #{captures} ===");
    }
    let slopes: Vec<String> = reads.iter().map(|r| format!("{:+.4}", r.slope)).collect();
    let _ = writeln!(s, "shear slope     : {}", slopes.join(" | "));
    match reads.iter().find_map(|r| r.observed_weight) {
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
        merged_rows.len(),
        merged_rows.iter().map(Vec::len).sum::<usize>()
    );
    let _ = writeln!(s);

    // The captured contents this shot, reconstructed row by row: one line per
    // grid row that resolved to an item, each tile shown as its item id (`_` for
    // a tile that matched nothing), left→right. This concise view sits above the
    // verbose per-row trace below so a scan can be read — and a ground-truth
    // label rebuilt — row by row without wading through the chrome/category
    // lines. On a burst these are the round-unioned rows fed to the merge.
    let _ = writeln!(s, "=== CAPTURED ITEMS (this shot, per row) ===");
    if merged_rows.is_empty() {
        let _ = writeln!(s, "  (no item rows this capture)");
    } else {
        for (i, row) in merged_rows.iter().enumerate() {
            let _ = writeln!(s, "  row {:>2}: {}", i + 1, format_tile_row(row));
        }
    }
    let _ = writeln!(s);

    if multi {
        let _ = writeln!(
            s,
            "=== PER-ROUND READS (unioned into the rows above; issue #165) ==="
        );
        for (i, read) in reads.iter().enumerate() {
            let _ = writeln!(s, "  round {}/{}:", i + 1, reads.len());
            if read.tile_rows.is_empty() {
                let _ = writeln!(s, "    (no item rows this round)");
            }
            for (ri, row) in read.tile_rows.iter().enumerate() {
                let _ = writeln!(s, "    row {:>2}: {}", ri + 1, format_tile_row(row));
            }
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "=== ROWS (reading order, de-sheared) ===");
    for (i, read) in reads.iter().enumerate() {
        if multi {
            let _ = writeln!(s, "  --- round {}/{} ---", i + 1, reads.len());
        }
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
/// the OCR engine, then hands the word boxes to the platform-independent
/// [`read_tiles`].
#[cfg(target_os = "windows")]
pub fn process_box_image(
    img: &image::DynamicImage,
    data: &GameData,
    scope: Option<&str>,
) -> anyhow::Result<BoxReadResult> {
    use crate::ocr::engine;
    use anyhow::Context;
    use image::GenericImageView;

    let (_w, img_h) = img.dimensions();
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
    Ok(read_tiles(&boxes, img_h as f32, data, scope))
}

/// Non-Windows stub: the OCR engine is Windows-only, so there's nothing to
/// read. Mirrors [`crate::ocr::pipeline::process_image`]'s stub.
#[cfg(not(target_os = "windows"))]
pub fn process_box_image(
    _img: &image::DynamicImage,
    _data: &GameData,
    _scope: Option<&str>,
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

    /// `round_rows` + `grid_rows` keep only `Names` rows (chrome / category
    /// dropped) and map each cell to a normalized `(x, matched)` at its pixel
    /// x-center, with the row at its normalized y-center. This is the
    /// position→normalized-coord mapping the feedback overlays (#137 / #138)
    /// rely on.
    #[test]
    fn grid_rows_filters_names_and_normalizes() {
        let read = BoxReadResult {
            tile_rows: vec![],
            observed_weight: None,
            slope: 0.0,
            rows: vec![
                RowReport {
                    ry: 50.0,
                    kind: RowKind::Category,
                    cells: vec![("All".into(), None)],
                    cxs: vec![100.0],
                    chs: vec![18.0],
                },
                RowReport {
                    ry: 100.0,
                    kind: RowKind::Names,
                    cells: vec![("Bolts".into(), Some("bolts".into())), ("???".into(), None)],
                    cxs: vec![200.0, 600.0],
                    chs: vec![18.0, 18.0],
                },
                RowReport {
                    ry: 150.0,
                    kind: RowKind::Chrome,
                    cells: vec![("21.9 / 30".into(), None)],
                    cxs: vec![400.0],
                    chs: vec![18.0],
                },
            ],
        };
        let grid = grid_rows(&round_rows(&read), 800.0, 200.0);
        // Only the single Names row survives.
        assert_eq!(grid.len(), 1);
        let row = &grid[0];
        assert!((row.y - 0.5).abs() < 1e-6, "ry 100 / h 200 = 0.5");
        assert_eq!(row.cells.len(), 2);
        assert!(
            (row.cells[0].x - 0.25).abs() < 1e-6,
            "cx 200 / w 800 = 0.25"
        );
        assert!(row.cells[0].matched, "resolved tile → matched (✓)");
        assert!(
            (row.cells[1].x - 0.75).abs() < 1e-6,
            "cx 600 / w 800 = 0.75"
        );
        assert!(!row.cells[1].matched, "unresolved tile → unmatched (✗)");
    }

    // ===================================================================
    // Native-capture regression fixtures (`screenshots/box/`, `screenshots/stash/`).
    //
    // Real box-screen captures keep getting flushed from the debug dir, so we
    // freeze each one's OCR-engine output (the word boxes; PP-OCRv4 since
    // #181) to JSON next to its PNG. `read_tiles` + `merge_capture` are pure
    // and platform-independent, so these fixtures let us regression-test the
    // whole post-OCR pipeline on every target (incl. Linux CI) without
    // re-running the Windows-only engine. The PNGs are the ground truth; the
    // `.boxes.json` are regenerated from them by `regen_box_fixtures`
    // (Windows, --ignored) — required after ANY engine change, or the replay
    // silently keeps scoring the old engine.
    //
    // Expected results live in `<scan>.label.txt` (`<item_id>  <count>` lines).
    // The `box` scan passes. The `stash` scan stays `#[ignore]`d: its 38-shot
    // row-by-row series (2026-06-11) is gap-free — every grid row is captured,
    // most in 3 consecutive frames — but an exact tally is still blocked by
    // in-game labels that diverge from data.json names (Windproof Matches,
    // Band-aids, Pet Shampoo, …) plus residual glyph misreads. See
    // `stash_scan_matches_label` and `screenshots/CLAUDE.md`.
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
        let scope = scan_scope(category);
        let mut master: Vec<ScanRow> = Vec::new();
        let mut next_id = 0u64;
        for shot in shots {
            let fx = BoxFixture::load(category, shot);
            let res = read_tiles(&fx.label_boxes(), fx.img_h, &data, scope);
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
    /// graded stash gate (`stash_scan_meets_graded_baseline`, every target) and
    /// the combined `eval_report_json` (Windows) to score box + stash.
    pub(crate) fn score_scan(category: &str, shots: &[String], label_file: &str) -> ScanScore {
        let read = run_box_scan(category, shots);
        let label = load_box_label(category, label_file);
        score_tally(category, &read, &label)
    }

    /// Score an already-merged tally against a ground-truth label — the body
    /// of [`score_scan`], split out so a variant pipeline (e.g. the round
    /// union) can score its own tally against the same label.
    pub(crate) fn score_tally(
        scan: &str,
        read: &HashMap<ItemId, u32>,
        label: &HashMap<ItemId, u32>,
    ) -> ScanScore {
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
            scan: scan.to_string(),
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

    /// The catalog category a box-scan fixture is locked to — the match scope
    /// passed to `read_tiles` / `match_item`, mirroring the per-container
    /// category the runtime derives from the `ScanTarget`. The misc world box
    /// and the junk-box stash hold `misc`; the Gunsmith → Storage and the
    /// Magazine & Attachments box hold gun parts (`gunsmith`).
    #[allow(dead_code)]
    pub(crate) fn scan_scope(category: &str) -> Option<&'static str> {
        match category {
            "box" | "stash" => Some("misc"),
            "gunsmith" | "magbox" => Some("gunsmith"),
            _ => None,
        }
    }

    /// Shot fixtures per scan, in scroll order: `(category, [boxes.json names])`.
    /// Single source of truth shared by the gate tests, the eval scorer and the
    /// sidecar writer.
    #[allow(dead_code)]
    pub(crate) fn scan_shots(category: &str) -> Vec<String> {
        match category {
            "box" => (0..3).map(|i| format!("box.shot{i}.boxes.json")).collect(),
            "stash" => (0..38)
                .map(|i| format!("stash.shot{i:02}.boxes.json"))
                .collect(),
            // Gunsmith → Storage gun-part container (issue #175 PR 4). A 4-shot
            // gaze-crop scroll series, scored graded (not exact) like stash —
            // the shots overlap with gaps, so the merge can't reach a complete
            // tally; a full-frame row-by-row recapture would gate it exactly.
            "gunsmith" => (0..4)
                .map(|i| format!("gunsmith.shot{i}.boxes.json"))
                .collect(),
            // The "Magazine & Attachments" world box (gun-part items): a 6-shot
            // gaze-crop burst (#197). Scanned with the gun-part matcher like
            // `gunsmith` — magazines match by name, attachments by scan_alias.
            "magbox" => (0..6)
                .map(|i| format!("magbox.shot{i}.boxes.json"))
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
        let notes: std::collections::HashMap<&str, &str> = [
            (
                "stash",
                "the 38-shot row-by-row series is gap-free and data.json names now \
                 match the game's own strings; the residual misses are engine-level \
                 (busy-icon tiles OCR to no text, e.g. RAM) plus phantom rows from \
                 garbled first sightings — informational, not gated",
            ),
            (
                "gunsmith",
                "a 4-shot gaze-crop scroll series (issue #175 PR 4): the gun-part \
                 short-name matcher (issue #183) resolves each tile against the \
                 part's scan_alias (the storage short name, from the game's \
                 GunSmithItemAdv table); short names the game shares across parts \
                 (e.g. 'AR-15 DD') stay unrecognized by design. The shots overlap \
                 with gaps so the merge is partial — a golden snapshot, not a full \
                 tally; a full-frame row-by-row recapture would gate it exactly",
            ),
        ]
        .into_iter()
        .collect();

        let mut written = 0usize;
        for category in ["box", "stash", "gunsmith", "magbox"] {
            let shots = scan_shots(category);
            // Per-shot: one sidecar per scroll-shot image.
            for shot in &shots {
                let fx = BoxFixture::load(category, shot);
                let read = read_tiles(&fx.label_boxes(), fx.img_h, &data, scan_scope(category));
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
    /// capture images (PNG / JPEG / WebP) across the box-scan categories
    /// (`screenshots/box/`, `screenshots/stash/`, `screenshots/gunsmith/`). Run
    /// after adding/replacing a capture:
    ///   cargo test -p ez-wishlist-overlay regen_box_fixtures -- --ignored
    #[test]
    #[ignore]
    #[cfg(target_os = "windows")]
    fn regen_box_fixtures() {
        use image::GenericImageView;
        for category in ["box", "stash", "gunsmith", "magbox"] {
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
    /// recognized tile across every box/stash shot, mirroring `read_tiles`'
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
        for category in ["box", "stash"] {
            for shot in scan_shots(category) {
                let fx = BoxFixture::load(category, &shot);
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
                            if let Some(id) = match_item(&data, &tokens, scan_scope(category)) {
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
                                    "{category}\t{shot}\t{id}\t{x0:.0}\t{y0:.0}\t{x1:.0}\t{y1:.0}\t{}",
                                    tokens.join(" ")
                                );
                            }
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
        assert_eq!(
            run_box_scan("box", &scan_shots("box")),
            load_box_label("box", "box.label.txt")
        );
    }

    /// `stash` scan — the Johnny's-Service submit terminal (38 scroll shots) →
    /// its full contents.
    ///
    /// The 2026-06-11 series was captured row by row: the 5-column grid scrolls
    /// one row per shot, so every interior row appears in 3 consecutive frames
    /// — there are NO scroll gaps (the blocker in the previous series), and
    /// `stash.label.txt` is a hand-read of the complete 40-row grid. The
    /// redundancy also exercises the row-uniqueness merge: all 40 rows are
    /// pairwise distinct (≥2 differing tiles), so re-seen rows dedup and no two
    /// distinct rows can collapse even with the one-drift tolerance.
    ///
    /// Still `#[ignore]`d because an exact tally needs every tile *recognized*.
    /// The old name divergences (Windproof Matches, Band-aids, Boxed Bolts,
    /// Boxed Nuts, Pet Shampoo) are fixed — data.json now carries the game's
    /// own display names (WFItemsStringTable ground truth) — so what remains
    /// is the engine: busy-diagonal-icon tiles (RAM above all) often produce
    /// no text at all in the frozen OCR, the battery pack digit misreads
    /// battery2 as battery1 (tracked limit), and garbled first sightings
    /// leave phantom rows (the merge keeps a row's first read). The
    /// `eval_report_json` diagnostic scores the scan's graded tile accuracy (a
    /// signal, not a gate); see `screenshots/CLAUDE.md`.
    #[test]
    #[ignore]
    fn stash_scan_matches_label() {
        assert_eq!(
            run_box_scan("stash", &scan_shots("stash")),
            load_box_label("stash", "stash.label.txt")
        );
    }

    /// Graded-accuracy floor for the stash scan: ≥ this many label tiles
    /// captured (`Σ min(read, label)` over the 200-tile ground truth). The
    /// 2026-06-11 baseline reads 178/200; the remaining misses are engine-level
    /// (see [`stash_scan_matches_label`]), so post-processing changes must not
    /// eat into them. **Ratchet:** when a change legitimately raises the score,
    /// bump this floor (and [`STASH_GATE_EXTRA_MAX`]) to the new value in the
    /// same PR — the committed `stash.ocr-result.txt` SUMMARY is the number to
    /// copy.
    const STASH_GATE_TILES_CORRECT: u32 = 190;
    /// Companion cap on over-counted / hallucinated tiles (`Σ max(0, read −
    /// label)`), so the floor above can't be gamed by a looser matcher that
    /// trades precision for recall. Now 6: the icon-label dedup
    /// ([`dedup_icon_labels`]) removed the Aspirin + Nail phantoms (was 16 →
    /// actual 4); the residual is the battery-twin digit misread. The few-tile
    /// margins above the actual (195 / 4) absorb cross-machine regen variance.
    const STASH_GATE_EXTRA_MAX: u32 = 6;

    /// The stash scan's enforced quality gate. Unlike the strict (`#[ignore]`d)
    /// exact-match above, this runs everywhere — Linux CI included, off the
    /// frozen `.boxes.json` — and fails any PR whose matcher / clustering /
    /// merge change drops graded tile accuracy below the committed baseline or
    /// inflates hallucinated tiles past it. `box` needs no graded twin: its
    /// scan is already gated exact by [`box_scan_matches_label`].
    #[test]
    fn stash_scan_meets_graded_baseline() {
        let score = score_scan("stash", &scan_shots("stash"), "stash.label.txt");
        assert!(
            score.tiles_correct >= STASH_GATE_TILES_CORRECT,
            "stash graded accuracy regressed: captured {}/{} label tiles \
             (gate floor {STASH_GATE_TILES_CORRECT}); diffs: {:?}",
            score.tiles_correct,
            score.tiles_total,
            score
                .diffs
                .iter()
                .map(|d| format!("{} {}/{}", d.item_id, d.read, d.label))
                .collect::<Vec<_>>(),
        );
        assert!(
            score.extra <= STASH_GATE_EXTRA_MAX,
            "stash scan over-counts: {} extra tile(s) read beyond the label \
             (gate cap {STASH_GATE_EXTRA_MAX}) — a looser matcher is \
             hallucinating items",
            score.extra,
        );
    }

    /// (Ignored authoring aid) Dump the merged gunsmith-storage scan tally with
    /// item names — used to author `gunsmith.label.txt` and set the gate floor.
    ///   cargo test -p ez-wishlist-overlay dump_gunsmith_scan -- --ignored --nocapture
    #[test]
    #[ignore = "authoring aid — print the gunsmith scan's resolved tally"]
    fn dump_gunsmith_scan() {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let names: HashMap<&str, &str> = data
            .items
            .iter()
            .map(|i| (i.id.as_str(), i.name.as_str()))
            .collect();
        let tally = run_box_scan("gunsmith", &scan_shots("gunsmith"));
        let mut rows: Vec<_> = tally.iter().collect();
        rows.sort_by_key(|(id, _)| id.to_string());
        println!("<<<GUNSMITH_TALLY {} distinct>>>", rows.len());
        for (id, n) in rows {
            println!(
                "{id}\t{n}\t{}",
                names.get(id.as_str()).copied().unwrap_or("?")
            );
        }
        println!("<<<END_GUNSMITH_TALLY>>>");
    }

    /// Floor of distinct gun parts the gunsmith-storage scan must resolve. The
    /// frozen 2026-06-14 captures (PP-OCRv4, #181) resolve **41**; before the
    /// issue-#183 matcher they resolved **0** (every tile dropped as "no item
    /// matched"). A floor (not an exact tally) because the row-uniqueness merge's
    /// dedup count shifts when the matcher / engine changes — what's gated is
    /// "the gun-part matcher keeps resolving real parts", with the spot-checks
    /// below pinning specific tiles.
    const GUNSMITH_GATE_MIN_DISTINCT: usize = 38;

    /// `gunsmith` scan — Neumann's Gunsmith → Storage (issue #175 PR 4 / #183).
    ///
    /// The grid shows hand-authored *short* gun-part names, not the full catalog
    /// names the misc box/stash tiles show, so it needs the gun-part matcher
    /// ([`crate::ocr::match_item`] scoped to the `gunsmith` category), which matches each
    /// tile against the part's `scan_alias` (the storage short name from the
    /// game's `GunSmithItemAdv` table). This gate runs everywhere off the frozen
    /// `.boxes.json` (PP-OCRv4) and asserts the scan resolves a floor of distinct
    /// parts — all genuinely in the `gunsmith` catalog — plus a spread of
    /// spot-checked tiles across part classes. Short names the game shares across
    /// parts stay unrecognized by design, so this is a floor, not an exact match
    /// (see `gunsmith.label.txt`).
    #[test]
    fn gunsmith_storage_scan_resolves_parts() {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let cat: HashMap<&str, Option<&str>> = data
            .items
            .iter()
            .map(|i| (i.id.as_str(), i.category.as_deref()))
            .collect();
        let tally = run_box_scan("gunsmith", &scan_shots("gunsmith"));

        assert!(
            tally.len() >= GUNSMITH_GATE_MIN_DISTINCT,
            "gunsmith scan regressed: resolved {} distinct parts (floor {}); \
             was 0 before the #183 gun-part matcher. Tally: {:?}",
            tally.len(),
            GUNSMITH_GATE_MIN_DISTINCT,
            tally.keys().collect::<std::collections::BTreeSet<_>>(),
        );
        // Every resolved tile must be a real gun part — the gun-part matcher must
        // never cross categories or invent an id.
        for id in tally.keys() {
            assert_eq!(
                cat.get(id.as_str()).copied().flatten(),
                Some("gunsmith"),
                "gunsmith scan resolved a non-gun-part id: {id}",
            );
        }
        // Spot-checks: specific tiles that must keep resolving via their alias,
        // spread across part classes and the alias edge cases (the AR308 family's
        // three distinct short names, an acronym, a magazine round-count).
        for id in [
            "gunsmith_20rail_sight_cobra",          // sight, alias "Cobra"
            "gunsmith_ak_mount_rsr",                // mount, alias "RSR"
            "gunsmith_ar10_barrel_508",             // barrel, alias "AR-10 508mm"
            "gunsmith_ar10_muzzle_ar308",           // AR308 family: compensator = "AR308"
            "gunsmith_ar10_upperreceiver_ar308",    // AR308 family: upper = "AR308 Upper"
            "gunsmith_ar10_lowerreceiver_ar308",    // AR308 family: rifle = "AR-308 DMR"
            "gunsmith_ar10_clip_10",                // magazine, alias "AR-10 10rd"
            "gunsmith_ump_clip_25",                 // magazine, alias "UMP45 25rd"
            "gunsmith_ar15_handguard_4inchris",     // handguard, alias "AR-15 4in RIS"
            "gunsmith_g3_stock_polygreen",          // stock, alias "G3 Green Stock"
            "gunsmith_sks_upperreceiver_dustcover", // upper, alias "SKS Cover"
        ] {
            assert!(
                tally.contains_key(id),
                "gunsmith scan no longer resolves {id}; tally: {:?}",
                tally.keys().collect::<std::collections::BTreeSet<_>>(),
            );
        }
    }

    /// Floor of distinct items the "Magazine & Attachments" box scan must
    /// resolve from the frozen 2026-06-18 captures. The adaptive column split
    /// ([`split_tiles`] / [`split_threshold`], #197) lifted this from ~2 — the
    /// dense rarity-tab grid's long names fused whole rows into one unmatchable
    /// blob — to 7, then the gunsmith-scoped magazine-blob re-split
    /// ([`resplit_magazine_runs`]) + the dropped-`"magazine"`-suffix restore in
    /// [`crate::ocr::match_item`] recovered AR-10 / MP5 / MP9 / PKP for **11/12**.
    /// The floor is 10 (margin below 11) to absorb cross-machine regen variance.
    /// Still a floor, not exact: it's a same-gaze gappy burst (not the stash's
    /// row-by-row series), and the AR-15 STANAG tile OCRs to nothing recoverable
    /// (needs a closer recapture). The box's items are gun parts, so it scans
    /// with the gun-part matcher (`scan_scope` maps `magbox` to `gunsmith`).
    const MAGBOX_GATE_MIN_DISTINCT: usize = 10;

    /// Companion precision cap on over-counted / wrong-part tiles
    /// (`Σ max(0, read − label)`), so the recall floor above can't hide an
    /// alias/suffix precision regression (workflow finding). The current 8:
    /// the gappy burst re-sees the SKS Puf across shots, the AKM appears twice,
    /// and one genuine false tag — the **XM5 6.8x51mm magazine isn't in the
    /// catalog yet**, so its (now-isolated) tile mis-resolves to the nearest
    /// same-caliber magazine (`sa58`). Adding the XM5 magazine to `data.json`
    /// would drop this to 7. The margin to 9 absorbs regen variance.
    const MAGBOX_GATE_EXTRA_MAX: u32 = 9;

    #[test]
    fn magbox_scan_resolves_magazines() {
        let shots = scan_shots("magbox");
        let tally = run_box_scan("magbox", &shots);
        assert!(
            tally.len() >= MAGBOX_GATE_MIN_DISTINCT,
            "magbox scan regressed: resolved {} distinct item(s) (floor {}); the \
             dense rarity-tab grid must not re-fuse into row-wide blobs. Tally: {:?}",
            tally.len(),
            MAGBOX_GATE_MIN_DISTINCT,
            tally.keys().collect::<std::collections::BTreeSet<_>>(),
        );
        // Spot-checks across the recovery paths: the four below sat in fused
        // rows that the geometric split alone left blank (e.g. shot4 row 1 was
        // one blob "EVO3…magazine EVO3…Drum magazine G18C…magazine G3…magazine"),
        // and the next four were recovered by the gunsmith-scoped blob re-split
        // (AR-10/MP5/MP9) + the dropped-"magazine"-suffix restore (PKP).
        for id in [
            "gunsmith_g18c_clip_30rnd",
            "gunsmith_g3_clip_30",
            "gunsmith_evo3_clip_drum",
            "gunsmith_ak_clip_762ak55",
            "gunsmith_ar10_clip_10",
            "gunsmith_mp5_clip_30rnd",
            "gunsmith_mp9_clip_15",
            "gunsmith_pkp_clip_drum",
        ] {
            assert!(
                tally.contains_key(id),
                "magbox scan no longer resolves {id}; tally: {:?}",
                tally.keys().collect::<std::collections::BTreeSet<_>>(),
            );
        }
        // Precision cap: bound the extras so a looser matcher can't trade recall
        // for wrong-part tags undetected behind the floor.
        let score = score_scan("magbox", &shots, "magbox.label.txt");
        assert!(
            score.extra <= MAGBOX_GATE_EXTRA_MAX,
            "magbox over-counts: {} extra tile(s) beyond the label (cap {}); a \
             looser alias/suffix match is mis-tagging. Diffs: {:?}",
            score.extra,
            MAGBOX_GATE_EXTRA_MAX,
            score
                .diffs
                .iter()
                .map(|d| format!("{} {}/{}", d.item_id, d.read, d.label))
                .collect::<Vec<_>>(),
        );
    }

    /// [`split_threshold`] separates a dense grid's intra-name gaps from its
    /// inter-column gaps without fragmenting a single multi-word name — the core
    /// of the #197 column fix, checked directly (no engine, every platform).
    #[test]
    fn split_threshold_finds_the_column_valley() {
        let h = 23.0;
        // A dense magbox-style row: intra-name word gaps ~7–12, inter-column
        // gaps ~19–33 — both below the legacy 1.5·h = 34.5 flat threshold.
        let dense = [8.0, 8.0, 8.0, 19.0, 7.0, 7.0, 10.0, 12.0, 26.0, 33.0];
        let t = split_threshold(&dense, h);
        assert!(
            (12.0..=19.0).contains(&t),
            "threshold {t} should land in the 12–19 valley"
        );
        assert!(
            dense.iter().filter(|&&g| g >= t).count() == 3,
            "exactly the three inter-column gaps (19/26/33) should split"
        );
        // A single multi-word name: all gaps small + similar → no split (the
        // threshold floors at 0.6·h, above every intra-name gap).
        let one_name = [7.0, 8.0, 9.0, 8.0];
        let tn = split_threshold(&one_name, h);
        assert!(
            one_name.iter().all(|&g| g < tn),
            "a single name must not fragment (threshold {tn})"
        );
        // A wide-gutter row (misc/stash style): huge inter-column gaps, tiny
        // intra gaps → splits exactly at the gutters, never inside a name.
        let wide = [6.0, 90.0, 6.0, 6.0, 95.0, 6.0];
        let tw = split_threshold(&wide, h);
        assert!(
            wide.iter().filter(|&&g| g >= tw).count() == 2,
            "wide-gutter row should split only at its two gutters (threshold {tw})"
        );
        // Documented boundary (#197 review): the 0.6*med_h floor protects a name
        // only when ALL its internal gaps are below it. A single name with one
        // internal gap in [0.6*h, 1.5*h] (the lone "high" value) DOES currently
        // fragment — Otsu returns it and the floor doesn't clamp it down. This
        // asserts the *current* behavior so a future change here is conscious.
        let one_wide = [6.0, 6.0, 15.0, 6.0]; // 15 = 0.65*h
        let twb = split_threshold(&one_wide, h);
        assert!(
            one_wide.iter().filter(|&&g| g >= twb).count() == 1,
            "a lone 0.65*h internal gap currently splits the name (threshold {twb})"
        );
    }

    #[test]
    fn ends_magazine_detects_the_anchor_word() {
        assert!(ends_magazine("magazine"));
        assert!(ends_magazine("Magazine,")); // trailing punctuation trimmed
        assert!(ends_magazine("10rndmagazine")); // engine glued it to the count
        assert!(!ends_magazine("magazin")); // OCR-truncated suffix
        assert!(!ends_magazine("AK74"));
    }

    #[test]
    fn resplit_magazine_runs_splits_only_fused_blobs() {
        // A fused 3-magazine run (>=2 anchors) → 3 tiles, the last a truncated
        // "magazin" trailing run after the final anchor.
        let boxes = [
            lb("XM5", 0.0, 0.0),
            lb("magazine", 30.0, 0.0),
            lb("MPS", 60.0, 0.0),
            lb("magazine", 90.0, 0.0),
            lb("MP9", 120.0, 0.0),
            lb("magazin", 150.0, 0.0),
        ];
        let fused: Vec<&LabelBox> = boxes.iter().collect();
        assert_eq!(resplit_magazine_runs(vec![fused]).len(), 3);
        // A single magazine name (one anchor) is left whole.
        let single: Vec<&LabelBox> = boxes[0..2].iter().collect();
        assert_eq!(resplit_magazine_runs(vec![single]).len(), 1);
        // No anchor at all → untouched.
        let none: Vec<&LabelBox> = boxes[4..6].iter().collect();
        assert_eq!(resplit_magazine_runs(vec![none]).len(), 1);
    }

    /// Run a scan with the issue-#165 round union harvesting the scroll
    /// overlap between consecutive shots: each shot is fused with its
    /// successor as a synthetic 2-round burst before merging. The committed
    /// stash series scrolls one row per shot, so the same physical row gets
    /// sightings from genuinely different engine reads — exactly the
    /// redundancy a live burst produces, replayed from the frozen fixtures.
    fn run_box_scan_with_pair_union(category: &str, shots: &[String]) -> HashMap<ItemId, u32> {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let reads: Vec<BoxReadResult> = shots
            .iter()
            .map(|shot| {
                let fx = BoxFixture::load(category, shot);
                read_tiles(&fx.label_boxes(), fx.img_h, &data, scan_scope(category))
            })
            .collect();
        let mut master: Vec<ScanRow> = Vec::new();
        let mut next_id = 0u64;
        for (i, read) in reads.iter().enumerate() {
            let mut rounds = vec![round_rows(read)];
            if let Some(next) = reads.get(i + 1) {
                rounds.push(round_rows(next));
            }
            let fused = union_rounds(&rounds);
            let rows: Vec<Vec<Tile>> = fused.iter().map(RoundRow::tiles).collect();
            merge_capture(&mut master, &mut next_id, &rows);
        }
        tally_rows(&master).0
    }

    /// What the pair-union run captures on the frozen stash series:
    /// strictly more than the single-read baseline's
    /// [`STASH_GATE_TILES_CORRECT`] (178) — the union recovers a RAM tile
    /// the engine read in only one of a row's sightings, the exact
    /// engine-miss class issue #165 targets. **Ratchet** like the baseline
    /// gates: if a change legitimately raises this, bump it in the same PR.
    const STASH_UNION_TILES_CORRECT: u32 = 190;
    /// Companion cap on the pair-union run's over-counted tiles. One above
    /// the single-read [`STASH_GATE_EXTRA_MAX`] (16), and **not** a
    /// hallucination: the union correctly fills the unmatched tile of
    /// `[rat_poison deodorant _ ram pcfan]` to `ceramic_adhesive`, but the
    /// baseline master already holds that physical row twice — its garbled
    /// sibling `[rat_poison deodorant ceramic_adhesive _]` differs by two
    /// drifts, beyond `rows_match`'s one-drift dedup tolerance — so the
    /// correct fill lands in a double-counted row. The union never *mints*
    /// tiles (see `union_never_multiplies_an_item_beyond_any_sighting`).
    const STASH_UNION_EXTRA_MAX: u32 = 6;

    /// Offline validation for the round union (issue #165), on the frozen
    /// stash fixtures: fusing each shot with its successor as a 2-round
    /// burst must read strictly more of the label than the single-read
    /// baseline (the recovered busy-icon tile) without inflating
    /// over-counts beyond the documented fill-into-a-duplicate (see the
    /// constants above). This scores the union on real engine variance —
    /// the per-shot reads of the same physical rows genuinely differ across
    /// frames — before any in-headset testing.
    #[test]
    fn stash_pair_union_improves_graded_score() {
        let read = run_box_scan_with_pair_union("stash", &scan_shots("stash"));
        let label = load_box_label("stash", "stash.label.txt");
        let score = score_tally("stash+union", &read, &label);
        eprintln!(
            "stash pair-union: {}/{} tiles, {} missing, {} extra",
            score.tiles_correct, score.tiles_total, score.missing, score.extra
        );
        assert!(
            score.tiles_correct >= STASH_UNION_TILES_CORRECT,
            "round union regressed: captured {}/{} label tiles (floor \
             {STASH_UNION_TILES_CORRECT}; the single-read baseline reads \
             {STASH_GATE_TILES_CORRECT}); diffs: {:?}",
            score.tiles_correct,
            score.tiles_total,
            score
                .diffs
                .iter()
                .map(|d| format!("{} {}/{}", d.item_id, d.read, d.label))
                .collect::<Vec<_>>(),
        );
        assert!(
            score.extra <= STASH_UNION_EXTRA_MAX,
            "round union over-counts: {} extra tile(s) beyond the label \
             (cap {STASH_UNION_EXTRA_MAX}) — the union must fill tiles, \
             never mint them",
            score.extra,
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

    // --- Round union (issue #165) ---------------------------------------------

    /// One round-row from a spec, tokens left→right at columns 0,1,2… (x =
    /// 100 + 200·col). `_` = a detected-but-unmatched tile (`None`); `.` = no
    /// tile detected at that column at all (the busy-icon engine miss — the
    /// word never existed, so the column is simply absent from this sighting).
    fn rrow(spec: &str) -> RoundRow {
        let cells = spec
            .split_whitespace()
            .enumerate()
            .filter(|(_, t)| *t != ".")
            .map(|(i, t)| {
                let tile = if t == "_" { None } else { Some(t.to_string()) };
                (100.0 + 200.0 * i as f32, tile)
            })
            .collect();
        RoundRow { ry: 0.0, cells }
    }

    /// The unioned rows as bare tile rows, for compact assertions.
    fn union_tiles(rounds: &[Vec<RoundRow>]) -> Vec<Vec<Tile>> {
        union_rounds(rounds).iter().map(RoundRow::tiles).collect()
    }

    #[test]
    fn union_single_round_is_a_passthrough() {
        let rounds = vec![vec![rrow("a b c"), rrow("d _ f")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c"), seq("d _ f")]);
    }

    #[test]
    fn union_fills_a_tile_the_engine_missed() {
        // The RAM case: the busy-icon tile OCRs to *no text at all* in round
        // 1 (absent, not unmatched) but reads in round 2 — the union recovers
        // it without inventing a fourth tile.
        let rounds = vec![vec![rrow("a . c")], vec![rrow("a b c")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c")]);
    }

    #[test]
    fn union_fills_a_tile_that_matched_nothing() {
        // Round 1 detected the tile but couldn't match it (`None`); round 2
        // read it. Any round's successful read counts.
        let rounds = vec![vec![rrow("a _ c")], vec![rrow("a b c")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c")]);
    }

    #[test]
    fn union_majority_resolves_a_conflicting_tile() {
        // The same physical tile (same column) reads `x` once and `c` twice:
        // majority wins, and the row keeps three tiles — the conflict is one
        // tile disputed, not two tiles seen.
        let rounds = vec![
            vec![rrow("a b x")],
            vec![rrow("a b c")],
            vec![rrow("a b c")],
        ];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c")]);
    }

    #[test]
    fn union_tie_goes_to_the_earliest_round() {
        let rounds = vec![vec![rrow("a b x")], vec![rrow("a b c")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b x")]);
    }

    #[test]
    fn union_fills_from_both_sides() {
        // Each round misses a different tile of the same 4-tile row; the
        // union has all four.
        let rounds = vec![vec![rrow("a b c .")], vec![rrow("a b . d")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c d")]);
    }

    #[test]
    fn union_keeps_a_never_matched_tile_unrecognized() {
        // A tile detected in every round but matched in none stays one
        // unrecognized tile — not dropped, not duplicated.
        let rounds = vec![vec![rrow("a _ c")], vec![rrow("a _ c")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a _ c")]);
    }

    #[test]
    fn union_appends_a_row_seen_in_one_round_only() {
        // A row visible only in round 2 (crop-edge wobble) passes through
        // unchanged after the rows both rounds saw.
        let rounds = vec![vec![rrow("a b c")], vec![rrow("a b c"), rrow("d e f")]];
        assert_eq!(union_tiles(&rounds), vec![seq("a b c"), seq("d e f")]);
    }

    #[test]
    fn union_never_groups_rows_of_the_same_round() {
        // Two identical-composition rows in ONE frame are distinct physical
        // rows (the documented duplicate-row layout); each round re-seeing
        // both must fuse pairwise, not collapse to one or grow to four.
        let rounds = vec![
            vec![rrow("nail nail nail"), rrow("nail nail nail")],
            vec![rrow("nail nail nail"), rrow("nail nail nail")],
        ];
        assert_eq!(
            union_tiles(&rounds),
            vec![seq("nail nail nail"), seq("nail nail nail")]
        );
    }

    #[test]
    fn union_flicker_in_one_column_does_not_grow_the_row() {
        // Round 2 reads the third tile as a different item at the SAME
        // column: that's one disputed tile (resolved first-round), never a
        // four-tile row — position separates "disagreeing read" from "two
        // different tiles".
        let rounds = vec![vec![rrow("a b x .")], vec![rrow("a b c .")]];
        let fused = union_tiles(&rounds);
        assert_eq!(fused, vec![seq("a b x")]);
    }

    #[test]
    fn union_never_multiplies_an_item_beyond_any_sighting() {
        // Two junk one-aspirin sightings of the same composition at far-apart
        // columns (the stash's lone ALL-CAPS icon-print rows): grouping is
        // right (identical composition) but the fused row must not mint a
        // second aspirin no round ever saw — the surplus column demotes to
        // an unrecognized tile, so the row keeps deduping against the lone
        // variant in the cross-shot merge.
        let r1 = RoundRow {
            ry: 0.0,
            cells: vec![(100.0, Some("aspire".into()))],
        };
        let r2 = RoundRow {
            ry: 0.0,
            cells: vec![(500.0, Some("aspire".into())), (700.0, None)],
        };
        let fused = union_tiles(&[vec![r1], vec![r2]]);
        assert_eq!(fused.len(), 1);
        let knowns = fused[0].iter().filter(|t| t.is_some()).count();
        assert_eq!(knowns, 1, "no sighting saw two aspirins: {fused:?}");
    }

    #[test]
    fn union_keeps_real_copies_a_sighting_saw_together() {
        // A row genuinely holding two nails: round 1 read both, so two
        // same-item columns are legitimate and survive the multiply cap
        // (round 2 missed one of them).
        let rounds = vec![vec![rrow("nail nail screw")], vec![rrow("nail . screw")]];
        assert_eq!(union_tiles(&rounds), vec![seq("nail nail screw")]);
    }

    #[test]
    fn union_fused_geometry_feeds_the_feedback_grid() {
        // The fused row keeps usable geometry: a tile filled from round 2
        // shows ✓ at its column in the normalized feedback grid.
        let rounds = vec![vec![rrow("a . c")], vec![rrow("a b c")]];
        let fused = union_rounds(&rounds);
        let grid = grid_rows(&fused, 1000.0, 100.0);
        assert_eq!(grid.len(), 1);
        let xs: Vec<f32> = grid[0].cells.iter().map(|c| c.x).collect();
        assert_eq!(xs, vec![0.1, 0.3, 0.5]);
        assert!(grid[0].cells.iter().all(|c| c.matched));
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
            scan_alias: None,
        }
    }

    fn box_data() -> GameData {
        GameData {
            data_version: "test".into(),
            scraped_at: "test".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: Vec::new(),
            research: Vec::new(),
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
        let res = read_tiles(&boxes, 500.0, &data, None);
        assert_eq!(
            res.tile_rows.concat(),
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

    /// Build a `Names` `RowReport` from `(text, id, cx, ch)` cells.
    fn names(ry: f32, cells: &[(&str, Option<&str>, f32, f32)]) -> RowReport {
        RowReport {
            ry,
            kind: RowKind::Names,
            cells: cells
                .iter()
                .map(|(t, id, _, _)| (t.to_string(), id.map(|s| s.to_string())))
                .collect(),
            cxs: cells.iter().map(|c| c.2).collect(),
            chs: cells.iter().map(|c| c.3).collect(),
        }
    }

    /// The icon-printed product name ("ASPIRIN", taller) over the real tile name
    /// ("Aspirin") in the same cell/column is demoted, so the item counts once.
    #[test]
    fn dedup_drops_taller_icon_label_over_name() {
        let med = 28.0;
        let mut rows = vec![
            names(700.0, &[("ASPIRIN", Some("aspire"), 150.0, 38.0)]),
            names(
                843.0,
                &[
                    ("Aspirin", Some("aspire"), 160.0, 28.0),
                    ("CD", Some("cd"), 480.0, 28.0),
                    ("RAM", Some("ram"), 800.0, 28.0),
                ],
            ),
        ];
        dedup_icon_labels(&mut rows, med);
        assert_eq!(rows[0].kind, RowKind::Chrome, "icon-print row emptied");
        assert_eq!(rows[1].kind, RowKind::Names);
        assert_eq!(rows[1].cells.iter().filter(|(_, t)| t.is_some()).count(), 3);
    }

    /// Two back-to-back icon-print/name pairs (every row gap sub-pitch — the case
    /// that poisons a per-shot pitch estimate). Both icon prints are demoted AND
    /// both real names survive: the height test stops a real upper name from
    /// being eaten by the next row's icon print directly below it.
    #[test]
    fn dedup_handles_back_to_back_pairs_keeps_real_names() {
        let med = 28.0;
        let mut rows = vec![
            names(300.0, &[("ASPIRIN", Some("aspire"), 150.0, 38.0)]), // icon-print 1
            names(
                445.0,
                &[
                    ("Aspirin", Some("aspire"), 160.0, 28.0),
                    ("Tape", Some("tape"), 480.0, 28.0),
                ],
            ), // name 1
            names(600.0, &[("ASPIRIN", Some("aspire"), 150.0, 38.0)]), // icon-print 2 (~155 below name 1)
            names(
                745.0,
                &[
                    ("Aspirin", Some("aspire"), 160.0, 28.0),
                    ("CD", Some("cd"), 480.0, 28.0),
                ],
            ), // name 2
        ];
        dedup_icon_labels(&mut rows, med);
        assert_eq!(rows[0].kind, RowKind::Chrome, "icon-print 1 demoted");
        assert_eq!(rows[2].kind, RowKind::Chrome, "icon-print 2 demoted");
        // Both real name rows keep their aspire (NOT eaten by the icon print below).
        assert!(rows[1]
            .cells
            .iter()
            .any(|(_, t)| t.as_deref() == Some("aspire")));
        assert!(rows[3]
            .cells
            .iter()
            .any(|(_, t)| t.as_deref() == Some("aspire")));
    }

    /// Legit same item in a DIFFERENT column (e.g. Civil Radio wrapping col4→col0)
    /// is preserved — the same-column test rejects it.
    #[test]
    fn dedup_keeps_same_item_different_column() {
        let med = 28.0;
        let mut rows = vec![
            names(540.0, &[("Radio", Some("radio"), 900.0, 30.0)]),
            names(700.0, &[("Radio", Some("radio"), 260.0, 28.0)]),
        ];
        dedup_icon_labels(&mut rows, med);
        assert_eq!(rows[0].kind, RowKind::Names);
        assert_eq!(rows[1].kind, RowKind::Names);
    }

    /// Legit same item a FULL grid-row pitch apart, same column (beardoil R23/R24)
    /// is preserved — the vertical gap exceeds the icon-label window.
    #[test]
    fn dedup_keeps_same_item_full_pitch_apart() {
        let med = 28.0;
        let mut rows = vec![
            names(300.0, &[("Beardoil", Some("beardoil"), 160.0, 28.0)]),
            names(620.0, &[("Beardoil", Some("beardoil"), 160.0, 28.0)]),
        ];
        dedup_icon_labels(&mut rows, med);
        assert_eq!(rows[0].kind, RowKind::Names);
        assert_eq!(rows[1].kind, RowKind::Names);
    }

    #[test]
    fn read_tiles_marks_unrecognized_tiles_as_none() {
        let data = box_data();
        let boxes = vec![
            lb("Piezometer", 50.0, 120.0),
            lb("Kalashnikov", 200.0, 120.0), // not in the vocab
            lb("Gunpowder", 360.0, 120.0),
        ];
        let res = read_tiles(&boxes, 500.0, &data, None);
        assert_eq!(
            res.tile_rows.concat(),
            vec![
                Some("piezometer".to_string()),
                None,
                Some("gunpowder".to_string())
            ]
        );
    }

    /// Regression guard for #182: a tilted item-name row and its category
    /// subtitle, ~one text-height apart, must split into **two** sub-rows. The
    /// PP-OCR engine's unclip inflates box heights (here `med_h = 24`), so the
    /// old `1.1·med_h` (26.4) split threshold exceeded the ~24 px name↔category
    /// gap and a slight shear merged them into one dropped "Name Category Name"
    /// row — losing whole grid rows from a live scan.
    #[test]
    fn split_subrows_separates_tilted_name_from_category() {
        let slope = -0.02;
        let med_h = 24.0;
        // Two bands 24 px apart (de-sheared ry ≈ 112 / 136), each tilted by
        // `slope` across a full-width 5-tile row, boxes 24 px tall.
        let band = |ry: f32| -> Vec<LabelBox> {
            (0..5)
                .map(|i| {
                    let x = 100.0 + i as f32 * 200.0;
                    let cx = x + 20.0;
                    LabelBox {
                        text: "x".into(),
                        x,
                        y: (ry + slope * cx) - 12.0, // cy = ry + slope*cx  ⇒  deshear = ry
                        w: 40.0,
                        h: 24.0,
                    }
                })
                .collect()
        };
        let names = band(112.0);
        let cats = band(136.0);
        let refs: Vec<&LabelBox> = names.iter().chain(cats.iter()).collect();
        let subs = split_subrows(&refs, slope, med_h);
        assert_eq!(
            subs.len(),
            2,
            "name and category subtitle must be separate sub-rows, not merged"
        );
        assert!(
            subs.iter().all(|s| s.len() == 5),
            "each sub-row keeps its 5 tiles"
        );
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
        let res = read_tiles(&boxes, 500.0, &data, None);
        let kinds: Vec<RowKind> = res.rows.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&RowKind::Category), "tab strip → Category");
        assert!(kinds.contains(&RowKind::Names), "item row → Names");
        assert!(kinds.contains(&RowKind::Chrome), "weight row → Chrome");
        // Only the Names row contributes tiles.
        assert_eq!(
            res.tile_rows.concat(),
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
        let res = read_tiles(&boxes, 500.0, &data, None);
        let mut master = Vec::new();
        let mut next_id = 0u64;
        let merge = merge_capture(&mut master, &mut next_id, &res.tile_rows);
        let (counts, unrecognized) = tally_rows(&master);
        let dump = format_capture_dump(
            std::slice::from_ref(&res),
            &res.tile_rows,
            merge,
            &counts,
            unrecognized,
            1,
        );
        assert!(dump.contains("BOX-SCAN CAPTURE #1"));
        assert!(dump.contains("shear slope"));
        assert!(dump.contains("new row")); // merge summary: "+N new row(s)"
        assert!(dump.contains("uvlight")); // resolved tile shown in the tally
        assert!(dump.contains("(skipped: tab strip / subtitles)"));
        // Concise per-row reconstruction sits above the verbose trace.
        assert!(dump.contains("CAPTURED ITEMS (this shot, per row)"));
        assert!(dump.contains("row  1: uvlight"));
        // The single-round dump carries no burst sections.
        assert!(!dump.contains("PER-ROUND READS"));
        assert!(!dump.contains("--- round"));
    }

    /// A burst dump records each round's raw read next to the unioned rows
    /// that fed the merge, so a tile recovered by an extra round is
    /// observable per round (issue #165).
    #[test]
    fn format_capture_dump_records_rounds_and_union() {
        let data = box_data();
        // Round 1 misses the middle tile entirely (busy icon → no text);
        // round 2 reads all three.
        let r1 = read_tiles(
            &[lb("Piezometer", 50.0, 120.0), lb("Gunpowder", 360.0, 120.0)],
            500.0,
            &data,
            None,
        );
        let r2 = read_tiles(
            &[
                lb("Piezometer", 50.0, 120.0),
                lb("Gunpowder", 360.0, 120.0),
                lb("Olive", 200.0, 120.0),
                lb("oil", 235.0, 120.0),
            ],
            500.0,
            &data,
            None,
        );
        let reads = vec![r1, r2];
        let per_round: Vec<Vec<RoundRow>> = reads.iter().map(round_rows).collect();
        let fused = union_rounds(&per_round);
        let merged_rows: Vec<Vec<Tile>> = fused.iter().map(RoundRow::tiles).collect();
        let mut master = Vec::new();
        let mut next_id = 0u64;
        let merge = merge_capture(&mut master, &mut next_id, &merged_rows);
        let (counts, unrecognized) = tally_rows(&master);
        let dump = format_capture_dump(&reads, &merged_rows, merge, &counts, unrecognized, 1);
        assert!(dump.contains("BOX-SCAN CAPTURE #1 (2 rounds)"));
        // The union fed the merge: all three items in one row.
        assert!(dump.contains("row  1: piezometer  oliveoil  gunpowder"));
        // Each round's raw read is recorded for comparison.
        assert!(dump.contains("PER-ROUND READS"));
        assert!(dump.contains("round 1/2:"));
        assert!(dump.contains("--- round 2/2 ---"));
        // Both rounds' slopes are shown side by side.
        assert!(dump.contains(" | "));
    }
}
