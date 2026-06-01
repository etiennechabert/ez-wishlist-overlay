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
// NOTE: the thresholds and the tab-row / weight detection are a first pass.
// They need tuning against real box-screen captures (Settings → `ocr_debug`),
// since they depend on exactly how Windows.Media.Ocr segments these labels.
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
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    fn right(&self) -> f32 {
        self.x + self.w
    }
}

/// One screenshot's worth of box-screen reading: the reading-order tiles
/// (clipped edge rows dropped) plus the total-weight readout used as a
/// post-merge sanity checksum.
#[derive(Clone, Debug, Default)]
pub struct BoxReadResult {
    pub tiles: Vec<Tile>,
    pub observed_weight: Option<f32>,
}

/// In-game category tabs that sit in a fixed row above the scrolling grid.
/// They don't scroll, so they must be excluded from the stitched sequence.
/// Hard-coded for the current English UI (one entry per visible word) — revisit
/// if the game relabels or localizes these.
#[allow(dead_code)]
const TAB_WORDS: &[&str] = &[
    "all",
    "medical",
    "supplies",
    "building",
    "combustible",
    "electric",
    "household",
    "intel",
    "tool",
];

/// Number of category-tab words on a row (≥2 ⇒ this is the fixed tab strip).
#[allow(dead_code)]
fn tab_word_hits(row: &[&LabelBox]) -> usize {
    row.iter()
        .filter(|b| {
            let t = b.text.trim().to_lowercase();
            TAB_WORDS.contains(&t.as_str())
        })
        .count()
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

/// Group boxes into visual rows by vertical position (boxes whose centres fall
/// within ~0.7 of the median text height share a row). Returns rows top→bottom.
#[allow(dead_code)]
fn cluster_rows<'a>(boxes: &[&'a LabelBox]) -> Vec<Vec<&'a LabelBox>> {
    if boxes.is_empty() {
        return Vec::new();
    }
    let mut sorted = boxes.to_vec();
    sorted.sort_by(|a, b| a.cy().total_cmp(&b.cy()));
    let tol = (median(sorted.iter().map(|b| b.h)) * 0.7).max(1.0);

    let mut rows: Vec<Vec<&LabelBox>> = Vec::new();
    let mut cur: Vec<&LabelBox> = Vec::new();
    let mut cur_cy = 0.0f32;
    for b in sorted {
        if cur.is_empty() || (b.cy() - cur_cy).abs() <= tol {
            cur.push(b);
        } else {
            rows.push(std::mem::take(&mut cur));
            cur.push(b);
        }
        cur_cy = cur.iter().map(|x| x.cy()).sum::<f32>() / cur.len() as f32;
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
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

/// Best-effort drop of a clipped top/bottom row whose label is cut off (markedly
/// shorter than the median row). The stitch's unknown-tolerance is the real
/// safety net for any clipped row that slips through; this just reduces noise.
#[allow(dead_code)]
fn drop_clipped_rows(rows: &mut Vec<Vec<&LabelBox>>) {
    if rows.len() < 3 {
        return; // need interior rows to establish a "normal" height
    }
    fn row_h(r: &[&LabelBox]) -> f32 {
        r.iter().map(|b| b.h).fold(0.0f32, f32::max)
    }
    let med = median(rows.iter().map(|r| row_h(r)));
    if med <= 0.0 {
        return;
    }
    if let Some(last) = rows.last() {
        if row_h(last) < med * 0.6 {
            rows.pop();
        }
    }
    if row_h(&rows[0]) < med * 0.6 {
        rows.remove(0);
    }
}

/// Turn one screenshot's recognized text boxes into reading-order tiles.
///
/// Excludes the fixed chrome (the category-tab strip and everything above it,
/// plus the weight readout and everything below it), clusters the remaining
/// labels into rows and tiles, resolves each tile via [`match_item`], and drops
/// clipped edge rows. The fixed chrome must be excluded because it doesn't
/// scroll — leaving it in would wreck the cross-capture alignment.
#[allow(dead_code)]
pub fn read_tiles(boxes: &[LabelBox], img_h: f32, data: &GameData) -> BoxReadResult {
    let weight = extract_weight(boxes, img_h);
    let grid_bottom = weight.map(|(_, y)| y).unwrap_or(img_h);

    let candidates: Vec<&LabelBox> = boxes.iter().filter(|b| b.cy() < grid_bottom).collect();
    let mut rows = cluster_rows(&candidates);

    // Drop the category-tab strip and any rows above it (e.g. a window title).
    if let Some(tab_idx) = rows.iter().rposition(|r| tab_word_hits(r) >= 2) {
        rows.drain(..=tab_idx);
    }
    drop_clipped_rows(&mut rows);

    let mut tiles: Vec<Tile> = Vec::new();
    for row in &rows {
        for tile_words in split_tiles(row) {
            let tokens: Vec<&str> = tile_words.iter().map(|b| b.text.as_str()).collect();
            tiles.push(match_item(data, &tokens));
        }
    }

    BoxReadResult {
        tiles,
        observed_weight: weight.map(|(v, _)| v),
    }
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

#[cfg(test)]
mod tests {
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
    // Native-capture regression fixtures (`box_screenshots_native/`).
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
    // The scan tests are `#[ignore]`d for now: the box-scan mis-reads these real
    // captures today (JUNK BOX → 0 items; "Big" → 6 of ~22). Un-ignore once the
    // read_tiles/stitch fixes land — the labels are the target.
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

    fn box_fixture_dir() -> std::path::PathBuf {
        // `BOX_FIXTURE_DIR` points `regen_box_fixtures` at a scratch copy (e.g.
        // to compare OCR across image formats) without touching the committed
        // set. Unset → the committed fixtures next to the crate.
        std::env::var_os("BOX_FIXTURE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../box_screenshots_native")
            })
    }

    impl BoxFixture {
        fn load(name: &str) -> Self {
            let p = box_fixture_dir().join(name);
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
    fn run_box_scan(shots: &[String]) -> HashMap<ItemId, u32> {
        let data = crate::assets::load_game_data().expect("embedded data.json");
        let mut master: Vec<Tile> = Vec::new();
        for shot in shots {
            let fx = BoxFixture::load(shot);
            let res = read_tiles(&fx.label_boxes(), fx.img_h, &data);
            stitch(&mut master, &res.tiles);
        }
        tally(&master).0
    }

    /// Parse a `<scan>.label.txt` ground-truth tally (`<item_id>  <count>`).
    fn load_box_label(name: &str) -> HashMap<ItemId, u32> {
        let p = box_fixture_dir().join(name);
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

    /// (Windows-only, ignored) Regenerate every `<stem>.boxes.json` from the
    /// capture images (PNG / JPEG / WebP) in the fixture dir. Run after
    /// adding/replacing a capture:
    ///   cargo test -p ez-wishlist-overlay regen_box_fixtures -- --ignored
    #[test]
    #[ignore]
    #[cfg(target_os = "windows")]
    fn regen_box_fixtures() {
        use image::GenericImageView;
        for entry in std::fs::read_dir(box_fixture_dir()).expect("fixture dir") {
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

    /// "Big" container scan (3 overlapping scroll shots) → its full contents.
    /// IGNORED: today the stitch refuses shots 2-3 (the overlap row reads in a
    /// different order each shot), so only ~6 of the ~22 items land. Un-ignore
    /// when read_tiles produces a stable reading order.
    #[test]
    #[ignore]
    fn big_container_scan_matches_label() {
        let shots: Vec<String> = (0..3).map(|i| format!("big.shot{i}.boxes.json")).collect();
        assert_eq!(run_box_scan(&shots), load_box_label("big.label.txt"));
    }

    /// JUNK BOX scan (10 overlapping scroll shots) → its full contents.
    /// IGNORED: today the per-item category subtitles (Tool/Household/…) are
    /// mistaken for the tab strip and the whole grid is dropped (0 items).
    /// Un-ignore when read_tiles stops treating subtitle rows as chrome.
    #[test]
    #[ignore]
    fn junkbox_scan_matches_label() {
        let shots: Vec<String> = (0..10)
            .map(|i| format!("junkbox.shot{i:02}.boxes.json"))
            .collect();
        assert_eq!(run_box_scan(&shots), load_box_label("junkbox.label.txt"));
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
}
