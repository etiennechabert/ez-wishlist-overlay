//! Box-container screen OCR: read a scrollable grid of item tiles and stitch a
//! series of overlapping scroll captures into one item list.
//!
//! A "box" container shows its contents on an in-game screen as a grid of tiles
//! (icon on top, name label below, ~4 columns) plus a total-weight readout
//! (`21.94 / 30`). The list scrolls, so a single screenshot rarely shows
//! everything; the user takes several while scrolling down and we merge them.
//!
//! This module has two clearly separated halves:
//!   - The **stitch core** ([`stitch`], [`tally`]) operates on reading-order
//!     sequences of [`Tile`] (`Option<ItemId>`, `None` = a tile we couldn't
//!     match). It is pure, platform-independent, and unit-tested on every
//!     target — it carries no OCR or image types.
//!   - The **OCR geometry** ([`process_box_image`], Windows-only) turns one
//!     screenshot into a `Vec<Tile>`. It lives behind `#[cfg(windows)]` like
//!     the rest of [`crate::ocr`].
//!
//! ## Why sequence alignment, not per-item dedup
//!
//! Identical items appear as *separate* tiles (a box with two piezometers shows
//! two tiles), so the owned count is the tile count. Consecutive scroll
//! captures overlap, and the same item can legitimately repeat in non-adjacent
//! rows — so we cannot dedup by item identity. Instead we align each new
//! capture against the running `master` sequence by its overlapping run and
//! append only the genuinely new tail. See [`stitch`].

use crate::data::{GameData, ItemId};
use crate::ocr::match_item::match_item;
use std::collections::HashMap;

/// One grid tile resolved to a known item, or `None` when OCR couldn't match
/// the label to any `Item.name` (below the matcher's threshold).
pub type Tile = Option<ItemId>;

/// Minimum number of *concrete* agreements (positions where both sequences
/// name the same known item) needed to trust an overlap. Below this we refuse
/// to merge rather than risk silently doubling the box: an all-`None` or
/// single-tile "overlap" is just as likely coincidence as a real seam. In
/// practice this means consecutive captures must overlap by at least two
/// recognized tiles — i.e. don't scroll a full page between shots.
const MIN_OVERLAP_CONFIDENCE: usize = 2;

/// What folding one new capture into the running `master` sequence did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StitchOutcome {
    /// The capture was merged. `added` new tiles were appended (0 when the
    /// capture was fully contained — a re-capture or scroll-up), after an
    /// overlap of `overlap` tiles with the existing tail.
    Merged { added: usize, overlap: usize },
    /// No confident overlap with `master` — the capture was *not* merged. The
    /// caller should ask the user to re-take it with more overlap (scroll up a
    /// little). Merging here would either double-count or skip a gap.
    NeedsRecapture,
}

/// Element equality across two equal-length slices, tolerant of unrecognized
/// tiles: a `None` on either side never *breaks* an alignment (it's a
/// wildcard) but never *confirms* it either. Returns the number of concrete
/// agreements (both `Some` and equal), or `None` if any position is a hard
/// contradiction (both `Some`, different items).
fn align(a: &[Tile], b: &[Tile]) -> Option<usize> {
    debug_assert_eq!(a.len(), b.len());
    let mut score = 0;
    for (x, y) in a.iter().zip(b) {
        if let (Some(p), Some(q)) = (x, y) {
            if p == q {
                score += 1;
            } else {
                return None;
            }
        }
    }
    Some(score)
}

fn count_known(tiles: &[Tile]) -> usize {
    tiles.iter().filter(|t| t.is_some()).count()
}

/// Fold a new capture's reading-order tiles into the accumulating `master`
/// sequence, returning what happened.
///
/// Algorithm (see module docs for the rationale):
///   1. Empty `master` → seed it with `new`.
///   2. **Scroll-down (primary):** find the overlap `k` where the suffix of
///      `master` aligns with the prefix of `new` ([`align`]). Among all `k`
///      with no hard contradiction, pick the one with the most concrete
///      agreements (ties → larger `k`, which appends fewer tiles). If that best
///      overlap clears [`MIN_OVERLAP_CONFIDENCE`], append `new[k..]`.
///   3. **Scroll-up / re-capture (fallback):** otherwise, if `new` sits
///      entirely inside `master` (a contiguous run that aligns), it's a view we
///      already have → no-op.
///   4. Otherwise there's no trustworthy seam → [`StitchOutcome::NeedsRecapture`].
pub fn stitch(master: &mut Vec<Tile>, new: &[Tile]) -> StitchOutcome {
    if new.is_empty() {
        return StitchOutcome::Merged {
            added: 0,
            overlap: 0,
        };
    }
    if master.is_empty() {
        master.extend_from_slice(new);
        return StitchOutcome::Merged {
            added: new.len(),
            overlap: 0,
        };
    }

    // Primary: suffix(master, k) vs prefix(new, k). Iterate k ascending so a
    // score tie keeps the larger k (more overlap, fewer tiles appended).
    let max_k = master.len().min(new.len());
    let mut best: Option<(usize, usize)> = None; // (k, score)
    for k in 1..=max_k {
        if let Some(score) = align(&master[master.len() - k..], &new[..k]) {
            best = match best {
                Some((_, best_score)) if best_score > score => best,
                _ => Some((k, score)),
            };
        }
    }
    if let Some((k, score)) = best {
        if score >= MIN_OVERLAP_CONFIDENCE {
            let added = new.len() - k;
            master.extend_from_slice(&new[k..]);
            return StitchOutcome::Merged { added, overlap: k };
        }
    }

    // Fallback: is `new` a contiguous run already inside `master`? (scroll-up
    // or a re-capture of an earlier view). Require either MIN_OVERLAP_CONFIDENCE
    // concrete agreements or — for a short `new` with fewer known tiles — that
    // every known tile lines up, so a mostly-unknown capture can't no-op-match
    // by coincidence.
    if new.len() <= master.len() {
        let known = count_known(new);
        for off in 0..=(master.len() - new.len()) {
            if let Some(score) = align(&master[off..off + new.len()], new) {
                if score >= MIN_OVERLAP_CONFIDENCE || (known > 0 && score == known) {
                    return StitchOutcome::Merged {
                        added: 0,
                        overlap: new.len(),
                    };
                }
            }
        }
    }

    StitchOutcome::NeedsRecapture
}

/// Count each recognized item across the stitched sequence; report how many
/// tiles stayed unrecognized (surfaced to the user, never written to a
/// container).
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
    pub tiles: Vec<Tile>,
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
    let mut rows: Vec<RowReport> = Vec::new();
    for block in &blocks {
        for sub in split_subrows(block, slope, med_h) {
            let ry = median(sub.iter().map(|b| deshear(b, slope)));
            let cells = split_tiles(&sub);

            // Skip a category sub-row (tab strip or per-item subtitles): one whose
            // tiles are mostly category words.
            let cat = cells.iter().filter(|t| is_category_tile(t)).count();
            if cat > 0 && cat * 2 >= cells.len() {
                rows.push(RowReport {
                    ry,
                    kind: RowKind::Category,
                    cells: cells.iter().map(|t| (join_text(t), None)).collect(),
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
            });
            if kind == RowKind::Names {
                tiles.extend(resolved.into_iter().map(|(_, m)| m));
            }
        }
    }

    BoxReadResult {
        tiles,
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

/// Render a human-readable recognition dump for one box-scan capture, for the
/// `ocr_debug` sidecar. Shows the de-shear slope, every sub-row's classification
/// and per-tile match, this shot's reading-order tiles, the stitch verdict, and
/// the running session tally — enough to see *why* a scan mis-read without the
/// game running. Pure (no I/O); the caller writes it next to the source PNG.
#[allow(dead_code)]
pub fn format_capture_dump(
    read: &BoxReadResult,
    outcome: StitchOutcome,
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
    match outcome {
        StitchOutcome::Merged { added, overlap } => {
            let _ = writeln!(
                s,
                "stitch          : merged (overlap {overlap}, added {added})"
            );
        }
        StitchOutcome::NeedsRecapture => {
            let _ = writeln!(
                s,
                "stitch          : NEEDS RECAPTURE (no confident overlap)"
            );
        }
    }
    let _ = writeln!(s, "tiles this shot : {}", read.tiles.len());
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
    Ok(read_tiles(&boxes, img_h as f32, data))
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

    /// Build a tile sequence from a string spec: each whitespace-separated
    /// token is an item id, or `_` for an unrecognized (`None`) tile.
    fn seq(spec: &str) -> Vec<Tile> {
        spec.split_whitespace()
            .map(|t| if t == "_" { None } else { Some(t.to_string()) })
            .collect()
    }

    fn count(master: &[Tile], id: &str) -> u32 {
        *tally(master).0.get(id).unwrap_or(&0)
    }

    // ===================================================================
    // Native-capture regression fixtures (`screenshots/box/`, `screenshots/stash/`).
    //
    // Real box-screen captures keep getting flushed from the debug dir, so we
    // freeze each one's Windows.Media.Ocr output (the word boxes) to JSON next
    // to its PNG. `read_tiles` + `stitch` are pure and platform-independent, so
    // these fixtures let us regression-test the whole post-OCR pipeline on every
    // target (incl. Linux CI) without re-running the Windows-only, slightly
    // nondeterministic engine. The PNGs are the ground truth; the `.boxes.json`
    // are regenerated from them by `regen_box_fixtures` (Windows, --ignored).
    //
    // Expected results live in `<scan>.label.txt` (`<item_id>  <count>` lines).
    // The `box` scan now passes (issue #109). The `stash` scan stays
    // `#[ignore]`d: its 10 shots can't be stitched into one sequence —
    // consecutive shots past shot 04 don't share a row (scroll gaps) and the OCR
    // drops whole tiles, so the overlap alignment can't bridge them. See
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
    /// `read_tiles` + `stitch` and return the final tally — what would be
    /// written into the container.
    pub(crate) fn run_box_scan(category: &str, shots: &[String]) -> HashMap<ItemId, u32> {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let mut master: Vec<Tile> = Vec::new();
        for shot in shots {
            let fx = BoxFixture::load(category, shot);
            let res = read_tiles(&fx.label_boxes(), fx.img_h, &data);
            stitch(&mut master, &res.tiles);
        }
        tally(&master).0
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

    /// `box` scan — the world container tablet (3 overlapping scroll shots) →
    /// its full 22-tile contents. The de-sheared, layout-aware `read_tiles`
    /// produces a stable reading order across the three shots, so `stitch`
    /// aligns the overlaps and the tally matches `box.label.txt` exactly
    /// (issue #109).
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
    /// STILL `#[ignore]`d after issue #109 — but the cause is the *captures*, not
    /// `read_tiles` (the subtitle-as-tab-strip bug that dropped the whole grid is
    /// fixed; each shot now reads its 15-tile grid). Two capture defects defeat
    /// the sequence stitch and can't be papered over in code:
    ///   - **Scroll gaps:** shots 00–04 each share a full row (a clean scroll),
    ///     but 04→05→06→07→08→09 share *no* row — rows fall between shots, so no
    ///     overlap exists for `stitch` to anchor on.
    ///   - **Dropped tiles:** the OCR omits whole labels (e.g. shot02 is missing
    ///     "Tape"; "Wire Cutter"→"Cutter") which shifts later columns and breaks
    ///     the rigid position-wise overlap alignment even within the 00–04 run.
    ///
    /// Un-ignoring needs better captures (every row overlapping, lossless) and/or
    /// a gap-tolerant stitch. `stash.label.txt` is kept as a verified reference
    /// of the contents; see `screenshots/CLAUDE.md`. The `eval_report_json`
    /// diagnostic still scores this scan's partial tile accuracy (it's a graded
    /// signal, not a gate).
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

    #[test]
    fn seeds_on_first_capture() {
        let mut master = Vec::new();
        let out = stitch(&mut master, &seq("a b c d"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 4,
                overlap: 0
            }
        );
        assert_eq!(master, seq("a b c d"));
    }

    #[test]
    fn appends_clean_overlap() {
        let mut master = seq("a b c d");
        // Scrolled down: shots share [c d], reveal [e f].
        let out = stitch(&mut master, &seq("c d e f"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 2,
                overlap: 2
            }
        );
        assert_eq!(master, seq("a b c d e f"));
    }

    #[test]
    fn keeps_legit_duplicate_rows() {
        // Two real piezometers in different rows must NOT collapse.
        let mut master = seq("a piezometer b c");
        let out = stitch(&mut master, &seq("b c piezometer d"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 2,
                overlap: 2
            }
        );
        assert_eq!(master, seq("a piezometer b c piezometer d"));
        assert_eq!(count(&master, "piezometer"), 2);
    }

    #[test]
    fn unknown_in_overlap_does_not_break_alignment() {
        // One tile failed OCR (`_`) in the overlap region of the new capture;
        // the concrete agreements on either side still anchor the seam.
        let mut master = seq("a b c d");
        let out = stitch(&mut master, &seq("c _ e f"));
        // Overlap k=2: c==c (concrete), d vs _ (wildcard). Score 1 < MIN(2)...
        // so this specific seam is too weak — needs more overlap.
        assert_eq!(out, StitchOutcome::NeedsRecapture);

        // With a third shared tile the seam is confident despite the unknown.
        let mut master = seq("a b c d e");
        let out = stitch(&mut master, &seq("c _ e f g"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 2,
                overlap: 3
            }
        );
        assert_eq!(master, seq("a b c d e f g"));
    }

    #[test]
    fn refuses_no_overlap() {
        // Scrolled too far: no shared tiles. Merging would either gap or
        // double-count, so refuse and ask for a re-shot.
        let mut master = seq("a b c d");
        let out = stitch(&mut master, &seq("e f g h"));
        assert_eq!(out, StitchOutcome::NeedsRecapture);
        assert_eq!(master, seq("a b c d")); // untouched
    }

    #[test]
    fn refuses_single_tile_overlap() {
        // Only one concrete agreement at the seam — too weak to trust.
        let mut master = seq("a b c d");
        let out = stitch(&mut master, &seq("d e f g"));
        assert_eq!(out, StitchOutcome::NeedsRecapture);
        assert_eq!(master, seq("a b c d"));
    }

    #[test]
    fn rejects_contradictory_overlap() {
        // A k with a hard contradiction is discarded; no other k is confident.
        let mut master = seq("a b c d");
        let out = stitch(&mut master, &seq("x y c d")); // tail [c d] but offset
                                                        // k=2 -> suffix [c d] vs prefix [x y]: hard mismatch, rejected.
                                                        // k=4 -> [a b c d] vs [x y c d]: a vs x mismatch, rejected.
        assert_eq!(out, StitchOutcome::NeedsRecapture);
    }

    #[test]
    fn scroll_up_recapture_is_noop() {
        // `new` is a contiguous interior run we already have.
        let mut master = seq("a b c d e f");
        let out = stitch(&mut master, &seq("b c d"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 0,
                overlap: 3
            }
        );
        assert_eq!(master, seq("a b c d e f")); // unchanged
    }

    #[test]
    fn exact_recapture_of_tail_appends_nothing() {
        let mut master = seq("a b c d");
        let out = stitch(&mut master, &seq("c d"));
        assert_eq!(
            out,
            StitchOutcome::Merged {
                added: 0,
                overlap: 2
            }
        );
        assert_eq!(master, seq("a b c d"));
    }

    #[test]
    fn three_capture_scroll_sequence() {
        let mut master = Vec::new();
        assert!(matches!(
            stitch(&mut master, &seq("a b c d")),
            StitchOutcome::Merged { .. }
        ));
        assert!(matches!(
            stitch(&mut master, &seq("c d e f")),
            StitchOutcome::Merged { .. }
        ));
        assert!(matches!(
            stitch(&mut master, &seq("e f g h")),
            StitchOutcome::Merged { .. }
        ));
        assert_eq!(master, seq("a b c d e f g h"));
        let (counts, unrecognized) = tally(&master);
        assert_eq!(unrecognized, 0);
        assert_eq!(counts.len(), 8);
        assert!(counts.values().all(|&c| c == 1));
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
        let outcome = stitch(&mut master, &res.tiles);
        let (counts, unrecognized) = tally(&master);
        let dump = format_capture_dump(&res, outcome, &counts, unrecognized, 1);
        assert!(dump.contains("BOX-SCAN CAPTURE #1"));
        assert!(dump.contains("shear slope"));
        assert!(dump.contains("merged")); // first capture seeds → Merged
        assert!(dump.contains("uvlight")); // resolved tile shown in the tally
        assert!(dump.contains("(skipped: tab strip / subtitles)"));
    }
}
