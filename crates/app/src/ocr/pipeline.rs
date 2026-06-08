//! End-to-end OCR pipeline: PNG path → `OcrOutcome`.
//!
//! Flow:
//!   1. Decode PNG.
//!   2. Run [`engine::recognize_image`] once on the full image → words.
//!   3. [`anchor::detect_panel`] from those words → panel + per-cell rects.
//!      If `None`, this isn't an upgrade panel — return `Ok(None)`.
//!   4. OCR the row-label rect → strict match → `Upgrade.id`.
//!   5. For each cell strip, binarize + template-match → owned count.
//!   6. Assemble [`OcrOutcome`].
//!
//! Non-Windows: returns `Ok(None)` — Windows.Media.Ocr is unavailable.

use crate::data::GameData;
#[cfg(target_os = "windows")]
use crate::ocr::OcrOutcome;
use crate::ocr::OcrPipelineResult;
use anyhow::Result;
use std::path::Path;

/// Thin shim that loads `path` from disk and delegates to
/// [`process_image`]. Kept so tests and any future on-disk-only
/// callers (manual GitHub-issue debugging, fixture sweeps) don't have
/// to inline the `image::open` step.
#[cfg(target_os = "windows")]
#[allow(dead_code)] // used by `fixture_tests` below; never by the runtime.
pub fn process_screenshot(
    path: &Path,
    data: &GameData,
    debug_dumps: bool,
    trace: bool,
) -> Result<OcrPipelineResult> {
    use anyhow::Context;

    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    process_image(img, Some(path), data, debug_dumps, trace)
}

/// Run the OCR pipeline on an already-decoded bitmap. `source_path`
/// is `Some(path)` when the caller also wrote the bitmap to disk
/// (debug-mode VR captures, test fixtures, GitHub-issue
/// repro flows) — the per-cell strip PNGs and `.ocr-debug.txt`
/// sidecar are written next to it. `None` in the runtime's fast
/// path: no disk artifacts, and the debug dumps automatically
/// no-op because they need a path to write next to.
#[cfg(target_os = "windows")]
pub fn process_image(
    img: image::DynamicImage,
    source_path: Option<&Path>,
    data: &GameData,
    debug_dumps: bool,
    trace: bool,
) -> Result<OcrPipelineResult> {
    use crate::ocr::{anchor, engine, match_upgrade, prep, templates};
    use anyhow::Context;
    use image::GenericImageView;

    let (img_w, img_h) = img.dimensions();
    if img_w == 0 || img_h == 0 {
        anyhow::bail!("zero-sized image");
    }

    // `path` is purely informational for tracing / debug-dump paths;
    // when the fast path skipped writing a PNG the source-path
    // tracing field is rendered as "<in-memory>".
    let path_display = || {
        source_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<in-memory>".to_string())
    };

    if trace {
        // Hash the in-memory image. When `source_path` is `Some` the
        // capture path also logged `rgb_fnv` for the bytes it wrote
        // — a mismatch points at corruption between capture and
        // OCR. When `source_path` is `None` the fast path skipped
        // the PNG round-trip entirely, so `decoded_fnv` should
        // always equal the capture's `rgb_fnv` exactly (same
        // buffer, no encode/decode in between).
        let rgb = img.to_rgb8();
        let mut h: u64 = 0xcbf29ce484222325;
        for b in rgb.as_raw() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        let raw = rgb.as_raw();
        tracing::info!(
            path = %path_display(),
            w = img_w,
            h = img_h,
            decoded_fnv = format!("{h:#018x}"),
            first_rgb = format!("[{},{},{}]", raw[0], raw[1], raw[2]),
            "ocr pipeline: image decoded (compare decoded_fnv against capture's rgb_fnv)"
        );
    }

    // Single OCR pass on the whole image. The first-pass words feed
    // both anchor detection (for cell layout) and the strict resolver
    // (for upgrade identification — which slides a window over these
    // tokens to find any module.name match). No further crop+re-OCR is
    // needed — the panel-bounds heuristic isn't reliable enough to
    // pixel-accurately crop the row label, and a single OCR pass with
    // tight matching turns out to be both simpler and more robust.
    let t_engine_start = std::time::Instant::now();
    let full_words = engine::recognize_image(&img).context("first-pass OCR")?;
    if trace {
        let first_words: Vec<String> = full_words
            .iter()
            .take(12)
            .map(|w| format!("{:?}@{:.0},{:.0}", w.text, w.rect.x, w.rect.y))
            .collect();
        tracing::info!(
            path = %path_display(),
            word_count = full_words.len(),
            engine_ms = t_engine_start.elapsed().as_millis() as u64,
            first_words = ?first_words,
            "ocr pipeline: Windows.Media.Ocr first-pass complete"
        );
    }
    let layout = match anchor::detect_panel(&full_words, img_w, img_h) {
        Some(l) => l,
        None => {
            tracing::info!(
                path = %path_display(),
                words = full_words.len(),
                "OCR pipeline: no \"Need to submit items\" anchor in OCR text — \
                 not an upgrade panel",
            );
            return Ok(OcrPipelineResult::NoPanel);
        }
    };

    // Pull the current level from any `LV<digit>` token in the panel.
    // The first-pass OCR sees it cleanly even when the surrounding
    // header text is unreliable (e.g. the panel header "Kitchen" /
    // "Moreitem" cases — Phase 0).
    let current_level = full_words
        .iter()
        .find_map(|w| anchor::parse_level_token(&w.text))
        .unwrap_or(0);

    // Concatenate all OCR tokens; the resolver picks the best
    // window-match against any module.name. Strict in the sense that
    // the only fuzziness allowed is character-level OCR noise within a
    // window (MIN_SCORE = 0.80) — Phase 0 invariant in match_upgrade.rs.
    let panel_text: String = full_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let upgrade_id = match match_upgrade::resolve(data, &panel_text, current_level) {
        Some(id) => id,
        None => {
            // Pull the raw OCR'd title text from the panel header rect
            // (the upgrade-name region above the "Need to submit items"
            // anchor). When we don't have a strict match we surface
            // EXACTLY what the OCR engine read, even if it's a single
            // letter like "e" — the user can see whether the failure
            // is "OCR mis-read the title" vs "title is correct but
            // missing from data.json" without guessing. Stop-tokens
            // like LV<digit> / FACILITY UPGRADE are excluded so the
            // hint stays short and recognisable.
            let header = layout.header;
            let module_hint = {
                let mut tokens: Vec<&str> = full_words
                    .iter()
                    .filter(|w| {
                        let wx = w.rect.x as u32;
                        let wy = w.rect.y as u32;
                        wx >= header.x
                            && wx <= header.x + header.w
                            && wy >= header.y
                            && wy <= header.y + header.h
                    })
                    .filter(|w| anchor::parse_level_token(&w.text).is_none())
                    .filter(|w| {
                        let up = w.text.to_ascii_uppercase();
                        !matches!(up.as_str(), "FACILITY" | "UPGRADE")
                    })
                    .map(|w| w.text.as_str())
                    .collect();
                if tokens.is_empty() {
                    None
                } else {
                    // Header tokens come back roughly top-to-bottom,
                    // left-to-right from the OCR engine; we keep that
                    // order so multi-word titles like "Procurement
                    // System" stay in their natural reading order.
                    let joined = tokens.join(" ");
                    tokens.clear();
                    Some(joined)
                }
            };
            tracing::warn!(
                path = %path_display(),
                module_hint = ?module_hint,
                current_level,
                "OCR pipeline: anchor found but no module.name + level match in data.json — \
                 likely an upgrade missing from the dataset",
            );
            return Ok(OcrPipelineResult::UnknownUpgrade {
                module_hint,
                current_level,
            });
        }
    };

    // Find the resolved upgrade so we can map cells → requirement item_ids.
    let upgrade = data
        .modules
        .iter()
        .flat_map(|m| &m.upgrades)
        .find(|u| u.id == upgrade_id)
        .expect("resolve returned an id absent from data.modules");
    let module = data
        .modules
        .iter()
        .find(|m| m.upgrades.iter().any(|u| u.id == upgrade_id))
        .expect("resolve returned an id absent from data.modules");

    // Cells: prefer the FROM-RAID-anchored boxes when available;
    // otherwise lay them out positionally using the now-known
    // `requirements.len()`. Either way the result is one box per
    // requirement, left-to-right.
    let cells = if layout.cells.len() == upgrade.requirements.len() {
        layout.cells.clone()
    } else {
        if !layout.cells.is_empty() {
            tracing::debug!(
                cells = layout.cells.len(),
                requirements = upgrade.requirements.len(),
                upgrade_id = %upgrade.id,
                "OCR pipeline: FROM RAID cell count mismatched requirements — falling back to positional",
            );
        }
        anchor::positional_cells(&layout, &full_words, upgrade.requirements.len())
    };

    // Binarize the full image once; cell strips are cropped from it.
    let prepped = prep::keep_white_invert(&img);
    let templates = &*templates::EMBEDDED;

    // Multi-Y candidate selection. The strip-Y heuristic in
    // `positional_cells` works for the "vertical card" layout (digits
    // below item names) but fails for the "compact" layout (digits
    // above item names, e.g. Kitchen Area, Water Collector). Strong
    // validation signal: a cell that template-matches `X/Y` with Y
    // equal to the **known** requirement quantity from `data.json` is
    // almost certainly the real digit row at the right offset.
    //
    // We hold the cell **X** positions (from FROM RAID / positional
    // layout, both reliable) and only sweep **Y** across the plausible
    // band. Whichever Y maximises the count of cells with `parsed.Y ==
    // req.quantity` wins. Tie-broken by larger Y (vertical-layout
    // preference — historically the original heuristic).
    let cells = pick_best_strip_y(&cells, &layout, &upgrade.requirements, &prepped, templates);
    // **Safety contract**: only include cells where `split_progress`
    // successfully parsed an `X/Y` shape. Cells we couldn't read get
    // SKIPPED here, so the worker doesn't call `set_collected` and the
    // user's existing count for that item survives. The previous
    // behaviour ("fallback to owned=0 on parse failure") destroyed
    // real progress whenever the strip Y misaligned and the template
    // matcher chewed on FROM RAID letters — turning "I have 8 bolts"
    // into "I have 0 bolts" silently on every bad capture.
    // Sweep any stale sibling files (`.ocr-debug.*.txt`, `.cell*.png`)
    // from previous runs BEFORE we write new ones. Doing this up front
    // is important: the per-cell loop below writes the strips for the
    // current capture, so if we deferred the purge until after the
    // dump it would delete the files we just wrote.
    //
    // Debug dumps are only meaningful when there's an on-disk source
    // PNG to land next to — in the fast path (`source_path = None`)
    // the bitmap is in-memory only, so the dumps short-circuit even
    // when `debug_dumps` is on.
    let debug_dump_path: Option<&Path> = if debug_dumps { source_path } else { None };
    if let Some(p) = debug_dump_path {
        crate::ocr::debug_dump::purge_prior_dumps(p);
    }

    let mut items: Vec<(String, Option<u32>)> = Vec::with_capacity(upgrade.requirements.len());
    let mut debug_cells: Vec<crate::ocr::debug_dump::CellDebug<'_>> = Vec::new();
    for (i, (cell, req)) in cells.iter().zip(upgrade.requirements.iter()).enumerate() {
        let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
        let gray = strip.to_luma8();

        // When `debug_dumps` is on (and we have a source path to land
        // next to), drop the binarised cell strip next to the source
        // screenshot as `<stem>.cell<i>.<HHMMSS>.png`. The strip is
        // small (~150×60 px) and shows the user exactly what the
        // template matcher actually saw, so attaching the four cell
        // PNGs to a GitHub issue is enough for a maintainer to
        // reproduce a wrong reading.
        if let Some(p) = debug_dump_path {
            if let Some(strip_path) = crate::ocr::debug_dump::cell_strip_path_for(p, i) {
                match gray.save(&strip_path) {
                    Ok(()) => tracing::info!(
                        cell = i,
                        item_id = %req.item_id,
                        path = %strip_path.display(),
                        "OCR cell strip saved",
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        path = %strip_path.display(),
                        "OCR cell strip save failed",
                    ),
                }
            }
        }

        let recog = templates::recognize_with_known_needed(&gray, templates, req.quantity);
        let parsed = templates::split_progress(&recog.recognised);
        // Apply the read only when BOTH hold:
        //
        //   (a) Y matches the known requirement quantity. The template
        //       matcher CAN return an "X/Y" shape with the wrong Y
        //       (slash-forcing only fixes the slash position, not the Y
        //       digit's template match), a sign the read is standing on
        //       icon noise rather than the digit row (e.g. IntelligentLv2
        //       cell 0 parsing "84/6" when needed=4).
        //
        //   (b) Every owned/needed digit clears the same confidence floor
        //       the strip-Y picker uses to *select* a cell's geometry
        //       ([`digits_clear_confidence`]). The picker enforces this
        //       gate when choosing the strip, but a cell it could NOT
        //       confirm reverts to base geometry — and without this gate
        //       the main loop re-recognised that base strip and applied
        //       the result on a Y-match alone, writing a low-confidence X
        //       digit. Canonical case: StorageZoneLock3 Gunpowder, whose
        //       narrow owned "1" mis-scores as '2'=0.611 (< 0.65), turning
        //       a correct 1/10 into a confidently-wrong 2/10 in
        //       AppState.collected. Gating here turns those into UNREAD.
        //
        // Either failure preserves the user's existing collected count.
        let owned_opt = parsed
            .filter(|&(_, y)| y == req.quantity)
            .filter(|_| digits_clear_confidence(&recog, req.quantity))
            .map(|(o, _)| o);
        items.push((req.item_id.clone(), owned_opt));
        if owned_opt.is_none() {
            tracing::warn!(
                item_id = %req.item_id,
                recognised = %recog.recognised,
                "OCR: failed to parse owned count for cell — leaving existing collected value untouched",
            );
        }
        if debug_dump_path.is_some() {
            let item_name = data
                .items
                .iter()
                .find(|it| it.id == req.item_id)
                .map(|it| it.name.as_str())
                .unwrap_or(req.item_id.as_str());
            debug_cells.push(crate::ocr::debug_dump::CellDebug {
                index: i,
                item_id: req.item_id.as_str(),
                item_name,
                needed: req.quantity,
                strip: *cell,
                raw_components: recog.raw_components,
                kept_components: recog.kept_components,
                recognised: recog.recognised,
                parsed_owned: owned_opt,
            });
        }
    }

    // When `debug_dumps` is on AND there's a source path, dump
    // everything the pipeline saw to a sibling text file next to the
    // screenshot. The user attaches the bundle (`<stem>.png` +
    // `<stem>.cell*.png` + `<stem>.ocr-debug.*.txt`) to a GitHub
    // issue when a capture's reading is wrong.
    if let Some(p) = debug_dump_path {
        use crate::ocr::debug_dump::{self, OcrDebugDump, Resolution};
        let labels = debug_dump::load_labels(p);
        let dump = OcrDebugDump {
            source_path: p,
            img_w,
            img_h,
            anchor: layout.anchor,
            words: &full_words,
            current_level,
            panel_text: &panel_text,
            resolution: Resolution::Resolved {
                upgrade_id: upgrade.id.as_str(),
                module_name: module.name.as_str(),
                upgrade_level: upgrade.level,
            },
            cells: debug_cells,
            labels,
        };
        let out = debug_dump::debug_path_for(p);
        match debug_dump::write_text(&dump, &out) {
            Ok(()) => tracing::info!(path = %out.display(), "OCR debug dump written"),
            Err(e) => {
                tracing::warn!(error = %e, path = %out.display(), "OCR debug dump write failed")
            }
        }
    }

    Ok(OcrPipelineResult::Identified(OcrOutcome {
        upgrade_id: upgrade.id.clone(),
        upgrade_name: module.name.clone(),
        items,
    }))
}

/// Minimum per-digit template-match score for a recognised `X/Y` strip
/// to be trusted. Applied to every NON-slash component (the `/` glyph is
/// a narrow vertical bar that legitimately scores low against digit
/// templates, so it's exempt — its position is force-assigned by
/// [`templates::recognize_with_known_needed`]).
///
/// Why the floor exists. Without it, components that template-matched
/// noise into an "X/Y" shape slip through:
///
/// - WaterCollectorLv2 cell 2 captured the "FROM RAID" letters and they
///   happened to score "8/6" (Y=6 matched `needed=6`).
/// - CryptoMining cell 0 caught half of a leading "0" that
///   template-matched "3" with a mediocre score (Y=4 matched).
/// - StorageZoneLock3 Gunpowder's narrow owned "1" scores '2'=0.611,
///   turning a correct 1/10 into 2/10.
///
/// Requiring every digit to clear ~0.65 rejects all three (FROM-RAID
/// letters and clipped half-digits score ~0.55-0.65 against digit
/// templates). Tuned to leave the observed minimum correct-match score
/// (~0.696 for narrow "1" glyphs) inside the accept band.
#[cfg(target_os = "windows")]
const MIN_DIGIT_CONFIDENCE: f32 = 0.65;

/// Does every owned/needed digit in a recognised strip clear
/// [`MIN_DIGIT_CONFIDENCE`]? Shared by the strip-Y picker (when *selecting*
/// a cell's geometry) and the main pipeline loop (when *applying* the
/// read), so a cell can't be written with a digit the picker would have
/// rejected. Returns `false` when there are too few components to form a
/// `/<Y>` tail — that's a sign the strip is on the wrong row.
#[cfg(target_os = "windows")]
fn digits_clear_confidence(recog: &crate::ocr::templates::RecognizeDebug, needed: u32) -> bool {
    let y_n = needed.to_string().chars().count();
    let total = recog.kept_components.len();
    if total < y_n + 1 {
        return false;
    }
    // Layout is `<X digits> / <Y digits>`; the slash sits just left of the
    // Y digits. Exempt it — only digits must clear the floor.
    let slash_idx = total - y_n - 1;
    for (i, k) in recog.kept_components.iter().enumerate() {
        if i == slash_idx {
            continue;
        }
        let best = k
            .scores
            .iter()
            .find(|(c, _)| *c != '/')
            .map(|(_, s)| *s)
            .unwrap_or(0.0);
        if best < MIN_DIGIT_CONFIDENCE {
            return false;
        }
    }
    true
}

/// Sweep candidate strip-Y positions across the plausible digit-row
/// band and return whichever set of cells produces the most
/// "trustworthy" parses, where trustworthy means the parsed `Y` side
/// of the `X/Y` token equals the known requirement quantity from
/// `data.json`. Falls back to the input cells when no candidate beats
/// them (or ties them) on score.
///
/// Why this exists: `positional_cells` infers strip-Y by walking
/// down from item-name positions, but the panel UI uses **two
/// distinct cell card layouts** — vertical (digits below item names)
/// and compact horizontal (digits above item names). One heuristic
/// can't hit both. Scoring candidates against the known Y values
/// from `data.json` lets us auto-pick the right layout without
/// having to detect it up front.
#[cfg(target_os = "windows")]
fn pick_best_strip_y(
    base_cells: &[crate::ocr::anchor::BBox],
    layout: &crate::ocr::anchor::PanelLayout,
    requirements: &[crate::data::Requirement],
    prepped: &image::DynamicImage,
    templates: &[crate::ocr::templates::Template],
) -> Vec<crate::ocr::anchor::BBox> {
    use crate::ocr::anchor::BBox;
    use crate::ocr::templates;

    let anchor = layout.anchor;
    let img_h = layout.img_h;
    let img_w = layout.img_w;

    // Strip height stays roughly one chunky-font text-row tall. Anchor
    // height is the most reliable scale signal across captures (it
    // tracks panel zoom / head distance directly).
    let strip_h = anchor.h.max(20);

    // Per-cell X-pad variants — extend the strip leftward to recover
    // leading digits that the positional layout clips at the cell-left
    // edge (Quality cell 0 reading "/2" instead of "2/2"). Right-pad
    // covers the rarer trailing-digit clip. The last entry is the
    // most aggressive — anchor-height-and-a-half on both sides — for
    // captures where the digit row sits well clear of the cell
    // centre (user-reported "OCR'd 3 instead of 2 because we're at
    // the border" on misc_b_disinfectingwipes).
    let x_pad_candidates: [(u32, u32); 5] = [
        (0, 0),
        (anchor.h / 2, 0),
        (anchor.h, 0),
        (anchor.h / 2, anchor.h / 2),
        (anchor.h * 3 / 2, anchor.h / 2),
    ];

    // Candidate top-Y values. Numerators come from empirical layouts:
    //   - compact horizontal (Kitchen Area, Water Collector): digits
    //     ~1.5 anchor heights below the "Need to submit items" line.
    //   - vertical card (Bookcase, Storage Room A): digits ~5-7
    //     anchor heights below.
    // Step density of 1/2 anchor.h covers off-scale panels.
    let mut candidate_y_tops: Vec<u32> = Vec::new();
    for half_h in 2..=14u32 {
        let y = anchor.y + anchor.h * half_h / 2;
        if y + strip_h < img_h {
            candidate_y_tops.push(y);
        }
    }

    // Per-cell independent search: a cell wins its best (Y, x-pad) only
    // when the parse confirms Y == known requirement quantity. Cells
    // that never confirm fall back to the input geometry — the
    // pipeline's per-cell parser will record an UNREAD for those, the
    // safe outcome (existing collected count preserved).
    //
    // Per-cell independence handles tilted captures and panels where
    // the digit row Y differs by a few pixels across cells: each cell
    // anchors to its own correct row instead of a uniform-but-wrong
    // compromise.
    let mut best_cells: Vec<BBox> = base_cells.to_vec();
    let mut confirmed: Vec<bool> = vec![false; base_cells.len()];

    // Confidence of a candidate parse, higher is better. `None` means
    // the parse failed (no `X/Y` shape, or Y didn't match the known
    // requirement quantity — that's the hard validation gate).
    //
    // The u32 tier (instead of a bool) prefers 1-digit X reads over
    // 2-digit ones when both pass validation. Game inventories sit in
    // 0-9 ~95% of the time, and the 2-digit case is overwhelmingly
    // noise that happened to template-match (MoreitemLv1 cell 0
    // reading "88/2" — Y matched needed but X was inflated by an
    // icon-edge component).
    let score_variant = |x: u32, y: u32, w: u32, h: u32, req_q: u32| -> Option<u32> {
        if w == 0 || h == 0 {
            return None;
        }
        let strip = prepped.crop_imm(x, y, w, h);
        let gray = strip.to_luma8();
        let recog = templates::recognize_with_known_needed(&gray, templates, req_q);
        let (parsed_x, parsed_y) = templates::split_progress(&recog.recognised)?;
        if parsed_y != req_q {
            return None;
        }
        // Every non-slash digit must clear the confidence floor — see
        // [`digits_clear_confidence`]. This rejects variants that
        // template-matched FROM-RAID letters or clipped half-digits into
        // an "X/Y" shape whose Y happened to match `needed`. The same
        // gate runs again in the main pipeline loop when the read is
        // applied, so a reverted-to-base cell can't sneak a low-
        // confidence digit into AppState.
        if !digits_clear_confidence(&recog, req_q) {
            return None;
        }

        // Higher tier for 1-digit X (the common case); a positive base
        // ensures any confirmed variant outranks an unconfirmed one.
        Some(if parsed_x < 10 { 100 } else { 50 })
    };

    // Score WITH a pad-bonus tiebreaker so that, among variants
    // passing the same digit-confidence + Y-match gate, the one
    // with more left-padding wins. The base unpadded crop is
    // usually correct, but when a leading digit sits *right* at
    // the cell column's left edge it gets clipped and template-
    // matched as a different digit (e.g. `2` → `3`). The picker
    // can't tell the two apart from match-score alone (both look
    // like valid 1-digit reads), so we add a small constant per
    // pad-pixel on the left so that more padding breaks ties.
    // Multiplier reserves room for the base score; pad won't
    // promote a 2-digit read over a 1-digit read.
    let score_variant_with_pad = |x, y, w, h, req_q, pad_l: u32| -> Option<u32> {
        let s = score_variant(x, y, w, h, req_q)?;
        // Cap the bonus so a wildly padded crop that accidentally
        // includes a neighbour's digit doesn't trump a clean read.
        let pad_bonus = pad_l.min(40);
        Some(s * 1000 + pad_bonus)
    };

    let mut best_scores: Vec<u32> = vec![0; base_cells.len()];

    // First: score the base geometry. Base has pad_l = 0, so the
    // tiebreaker bonus is zero — any padded variant that matches the
    // base's parse score wins.
    for (i, (cell, req)) in base_cells.iter().zip(requirements.iter()).enumerate() {
        if let Some(s) = score_variant_with_pad(cell.x, cell.y, cell.w, cell.h, req.quantity, 0) {
            best_scores[i] = s;
            confirmed[i] = true;
        }
    }

    // Then sweep (Y, x-pad) variants. Upgrade a cell when the variant
    // STRICTLY beats the current best score — never replace a confirmed
    // variant with one of equal score (preserves original geometry on
    // ties; the base/earlier candidate ran first by design).
    for (i, (cell, req)) in base_cells.iter().zip(requirements.iter()).enumerate() {
        for y_top in &candidate_y_tops {
            for (pad_l, pad_r) in &x_pad_candidates {
                let new_x = cell.x.saturating_sub(*pad_l);
                let new_w = (cell.w + pad_l + pad_r).min(img_w.saturating_sub(new_x));
                let new_h = strip_h.min(img_h.saturating_sub(*y_top));
                if let Some(s) =
                    score_variant_with_pad(new_x, *y_top, new_w, new_h, req.quantity, *pad_l)
                {
                    if s > best_scores[i] {
                        best_scores[i] = s;
                        best_cells[i] = BBox {
                            x: new_x,
                            y: *y_top,
                            w: new_w,
                            h: new_h,
                        };
                        confirmed[i] = true;
                    }
                }
            }
        }
    }

    // Consensus-Y alignment across siblings. All cells in an upgrade
    // panel live on the same horizontal row — their digit-row Y
    // positions should agree to within a few pixels (the chunky
    // glyphs are ~12-18 px tall and the picker's strip height tracks
    // that). When the per-cell search wanders off and picks a Y
    // many anchor heights away from its siblings, that cell is
    // almost certainly sitting on a noise row that happened to
    // template-match into a fake "X/Y" shape — the cost-row
    // "400000" giving a Screwdriver cell a fake `0/6` is the
    // canonical example. Reject outliers: any confirmed cell whose
    // chosen Y differs from the median of confirmed cells by more
    // than 2× anchor height gets reverted, which sends the cell
    // through the main loop as UNREAD and preserves the user's
    // existing count.
    let confirmed_ys: Vec<u32> = best_scores
        .iter()
        .zip(best_cells.iter())
        .filter(|(s, _)| **s > 0)
        .map(|(_, c)| c.y)
        .collect();
    if confirmed_ys.len() >= 2 {
        let mut sorted = confirmed_ys.clone();
        sorted.sort();
        let median_y = sorted[sorted.len() / 2];
        let tolerance = anchor.h.saturating_mul(2).max(40);
        for i in 0..best_cells.len() {
            if best_scores[i] > 0 && best_cells[i].y.abs_diff(median_y) > tolerance {
                tracing::info!(
                    cell = i,
                    chosen_y = best_cells[i].y,
                    median_y,
                    tolerance,
                    "OCR picker: rejecting outlier-Y cell (siblings disagree); \
                     cell will be UNREAD"
                );
                // Revert to base geometry so the main loop's
                // recognise+Y-match gate fails cleanly (the base
                // wasn't a strong match either, otherwise it would
                // have been the picker's choice from the start).
                best_cells[i] = base_cells[i];
                best_scores[i] = 0;
                confirmed[i] = false;
            }
        }
    }

    // Consensus-row rescue. The inverse of the outlier pass above: where
    // that *removes* a cell that wandered onto a noise row, this *pulls*
    // a cell the coarse sweep left UNCONFIRMED onto the digit row its
    // confirmed siblings agree on. In the compact (no-FROM-RAID) layout
    // the positional base geometry is unreliable — its Y is poisoned by
    // cost/button numbers ("114", "488500") or lands on the digit row's
    // top edge, where `recognize`'s edge guard (`c.y > 0`) drops the
    // glyphs — so an unconfirmed cell falls back to a strip that reads
    // empty or garbage even though the row itself is fine. The confirmed
    // siblings pinpoint that row to within a few pixels.
    //
    // The coarse sweep steps `anchor.h / 2` (≈12-18 px) — larger than the
    // ~13 px digit height — so a digit row can fall on a strip edge
    // *between* candidates; an easy read survives the misframing, a hard
    // one (narrow leading "1", a cell whose row sits mid-step) doesn't.
    // So for each still-unconfirmed cell we run a FINE Y search (3 px
    // step) over a tight band around the consensus Y, keeping the cell's
    // own X column and the usual x-pad variants. Acceptance is the
    // unchanged `score_variant` gate (Y == known `needed` AND every digit
    // ≥ MIN_DIGIT_CONFIDENCE), identical to what the confirmed cells and
    // the main-loop apply gate require — so a rescued cell is exactly as
    // trustworthy as any other confirmed read and wrong writes stay
    // impossible.
    //
    // Restricted to single-digit `needed`: a two-digit Y (`/10`) read is
    // materially less reliable at this font size — an owned `0` misreads
    // as `6` *above* the confidence floor (`0/10` → `6/10`), which would
    // be a wrong write — so `/10` cells are left for a dedicated lead
    // rather than rescued on the consensus row here.
    let post_outlier_ys: Vec<u32> = best_scores
        .iter()
        .zip(best_cells.iter())
        .filter(|(s, _)| **s > 0)
        .map(|(_, c)| c.y)
        .collect();
    if post_outlier_ys.len() >= 2 {
        let mut sorted = post_outlier_ys;
        sorted.sort_unstable();
        let consensus_y = sorted[sorted.len() / 2] as i32;
        // Tight band: the observed good rescues all land within ±1 anchor
        // height of consensus; a little headroom, no more (a wider band
        // just invites a fluke parse far from the real row).
        let band = (anchor.h as i32 * 3 / 2).max(20);
        for (i, (cell, req)) in base_cells.iter().zip(requirements.iter()).enumerate() {
            if best_scores[i] > 0 || req.quantity >= 10 {
                continue; // already confirmed, or a two-digit-Y cell (see above)
            }
            let mut dy = -band;
            while dy <= band {
                let y_i = consensus_y + dy;
                dy += 3;
                if y_i < 0 || (y_i as u32) + strip_h >= img_h {
                    continue;
                }
                let y_top = y_i as u32;
                for (pad_l, pad_r) in &x_pad_candidates {
                    let new_x = cell.x.saturating_sub(*pad_l);
                    let new_w = (cell.w + pad_l + pad_r).min(img_w.saturating_sub(new_x));
                    let new_h = strip_h.min(img_h.saturating_sub(y_top));
                    if let Some(s) =
                        score_variant_with_pad(new_x, y_top, new_w, new_h, req.quantity, *pad_l)
                    {
                        if s > best_scores[i] {
                            best_scores[i] = s;
                            best_cells[i] = BBox {
                                x: new_x,
                                y: y_top,
                                w: new_w,
                                h: new_h,
                            };
                            confirmed[i] = true;
                        }
                    }
                }
            }
        }
    }

    let _ = confirmed; // Confirmed bits are an analysis aid, not consumed downstream.
    best_cells
}

// `process_screenshot` has no non-Windows stub — its only callers are
// the Windows-gated fixture tests below. Non-Windows code drives the
// pipeline via `process_image` only.
#[cfg(not(target_os = "windows"))]
pub fn process_image(
    _img: image::DynamicImage,
    _source_path: Option<&Path>,
    _data: &GameData,
    _debug_dumps: bool,
    _trace: bool,
) -> Result<OcrPipelineResult> {
    Ok(OcrPipelineResult::NoPanel)
}

#[cfg(all(test, target_os = "windows"))]
mod fixture_tests {
    //! Integration-style coverage driven by the **native-resolution
    //! PNG fixtures** in `screenshots/hideout/`. Each fixture's
    //! filename is the `Upgrade.id` ground truth (e.g.
    //! `BookcaseLv1.png` ↔ `Upgrade.id = "BookcaseLv1"`). We assert
    //! identification + cell ordering match `data.json` here; per-cell
    //! owned-count accuracy is tracked via the `read_native_pngs`
    //! diagnostic (run with `--ignored`) and the sibling
    //! `.ocr-debug.txt` dumps.
    //!
    //! The old Steam-F12 JPGs (formerly `hideout_screenshots/`) were
    //! dropped from the repo entirely — their lossy compression
    //! destroyed the chunky pixel-art digit font and made digit-OCR
    //! results unrepresentative of what the runtime sees on real
    //! captures.

    use crate::ocr;
    use std::path::PathBuf;

    fn load_data() -> crate::data::GameData {
        let raw = include_str!("../assets/data.json");
        serde_json::from_str(raw).expect("data.json is valid")
    }

    fn fixture_dir() -> PathBuf {
        // CARGO_MANIFEST_DIR is `crates/app`; native captures live at repo root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout")
    }

    /// True for the "primary" fixture captures (e.g. `BookcaseLv1.webp`).
    /// Accepts both `.webp` (the committed lossy captures) and `.png`
    /// (still used by the `#[ignore]`d regen diagnostics that dump
    /// upscaled crops). Excludes per-cell strip debug images
    /// (`*.cellN.*.png`) and other sibling files that get written next
    /// to fixtures by the pipeline's debug-build dumps — those would
    /// otherwise be picked up as fixtures themselves and break the
    /// test sweep.
    fn is_primary_fixture(entry: &std::fs::DirEntry) -> bool {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("webp") | Some("png")) {
            return false;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        !stem.contains(".cell") && !stem.contains(".ocr-debug")
    }

    /// Sweep every JPG fixture, report a per-image pass/fail line, and
    /// fail the test if the pass count is below the threshold (per the
    /// design plan: ≥ 18/20). Owned-count digits are not asserted; the
    /// JPGs are the regression-detection floor for identification + cell
    /// ordering, not for digit accuracy.
    /// One-shot bootstrap: walk every PNG in `screenshots/hideout/`,
    /// identify the upgrade, crop each cell's owned/needed count strip,
    /// connected-component it, and save labeled PNG templates under
    /// `crates/app/src/assets/ocr_templates/`.
    ///
    /// Auto-labeling strategy: we know the upgrade-Id from the filename
    /// (e.g. `BookcaseLv1.png` → `BookcaseLv1`), so we know each cell's
    /// `requirements[i].quantity` (the Y in "X/Y"). The right-most
    /// `len(str(Y))` connected components are the Y digits (known
    /// labels), the component immediately to their left is the slash,
    /// and everything to the left is the X (owned) digit(s) which we
    /// don't know automatically and therefore skip in this pass.
    ///
    /// First-encounter wins: once `crates/app/src/assets/ocr_templates/
    /// <digit>.png` exists, subsequent encounters for the same digit
    /// are skipped. Re-run after deleting a template if a specific
    /// instance is bad.
    ///
    /// Digits 7 and 9 do not appear in any `requirements.quantity` in
    /// `data.json` (verified by grep), so this pass cannot produce
    /// `7.png` / `9.png`. Capture a panel where you've collected 7 or 9
    /// of an item and follow up with manual extraction.
    /// Dump a 2×-upscaled crop of the whole digit-row band for every
    /// native fixture, anchored on FROM-RAID position. Output PNGs
    /// land in `target/ocr_cells_wide/<UpgradeId>.png` and stay
    /// legible at preview scale — usable for hand-labelling the
    /// ground-truth X values across all 15 fixtures.
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_wide_count_rows() {
        use crate::ocr::{anchor, engine};
        use image::GenericImageView;

        let in_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
        let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/ocr_cells_wide");
        std::fs::create_dir_all(&out_dir).expect("create out dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in &entries {
            let path = entry.path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let img = match image::open(&path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let (img_w, img_h) = img.dimensions();
            let words = match engine::recognize_image(&img) {
                Ok(w) => w,
                Err(_) => continue,
            };
            let layout = match anchor::detect_panel(&words, img_w, img_h) {
                Some(l) => l,
                None => continue,
            };
            let a = layout.anchor;
            // Anchor-relative band. Digit row + FROM RAID + a bit of
            // margin top/bottom for variation across head tilts.
            let top = a.y + a.h * 2;
            let bot = (a.y + a.h * 9).min(img_h);
            let panel_w_est = a.w * 10 / 3;
            let cx = a.x + a.w / 2;
            let left = cx.saturating_sub(panel_w_est / 2).min(img_w);
            let right = (cx + panel_w_est / 2).min(img_w);
            let cw = right - left;
            let ch = bot.saturating_sub(top);
            let crop = img.crop_imm(left, top, cw, ch);
            let up = image::imageops::resize(
                &crop.to_rgba8(),
                cw * 2,
                ch * 2,
                image::imageops::FilterType::Nearest,
            );
            let out = out_dir.join(format!("{stem}.png"));
            up.save(&out).expect("save wide crop");
            eprintln!("saved {}", out.display());
        }
    }

    /// Extract X-digit templates from native PNGs using the
    /// hand-labelled ground truth (`<UpgradeId>.label.txt`). The
    /// bootstrap `extract_digit_templates_from_native_pngs` pulls
    /// templates from the **Y** position (right of the slash), where
    /// the game UI renders digits at h≈8 — too short for the X
    /// position's full-size digits (h≈11) which is what most
    /// real-world reads actually template-match against. Mixing those
    /// height regimes makes the matcher pick wrong digits at the X
    /// position (BookcaseLv1 cell 0 reading "0" instead of "1").
    ///
    /// This walks every fixture, runs the full pipeline to get
    /// refined cell strips, then for each cell where the label's X is
    /// a single digit AND the cell's recognised string parses with Y
    /// matching the known requirement, it saves the FIRST component
    /// (the X digit) as `<X>.png`. Existing templates are skipped
    /// (first-encounter wins), so to regen a specific digit, delete
    /// its `.png` first.
    ///
    /// Run with:
    /// `cargo test -p ez-wishlist-overlay ocr::pipeline::fixture_tests::extract_x_digit_templates -- --ignored --nocapture`
    #[test]
    #[ignore = "one-shot — extracts X-digit templates from labelled fixtures"]
    fn extract_x_digit_templates() {
        use crate::ocr::debug_dump;
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;

        let data = load_data();
        let in_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/assets/ocr_templates");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut saved: std::collections::BTreeMap<char, String> = std::collections::BTreeMap::new();

        for entry in &entries {
            let path = entry.path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();

            let labels = match debug_dump::load_labels(&path) {
                Some(l) => l,
                None => {
                    eprintln!("SKIP {stem}: no label file");
                    continue;
                }
            };

            let img = match image::open(&path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let (img_w, img_h) = img.dimensions();
            let upgrade = match data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem)
            {
                Some(u) => u,
                None => continue,
            };
            let words = match engine::recognize_image(&img) {
                Ok(w) => w,
                Err(_) => continue,
            };
            let layout = match anchor::detect_panel(&words, img_w, img_h) {
                Some(l) => l,
                None => continue,
            };
            let base_cells = if layout.cells.len() == upgrade.requirements.len() {
                layout.cells.clone()
            } else {
                anchor::positional_cells(&layout, &words, upgrade.requirements.len())
            };
            let prepped = prep::keep_white_invert(&img);
            let cells = super::pick_best_strip_y(
                &base_cells,
                &layout,
                &upgrade.requirements,
                &prepped,
                &templates::EMBEDDED,
            );

            for (cell, req) in cells.iter().zip(upgrade.requirements.iter()) {
                let Some(label) = labels.iter().find(|l| l.item_id == req.item_id) else {
                    continue;
                };
                if label.owned >= 10 {
                    continue; // multi-digit X — skip
                }
                let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
                let gray = strip.to_luma8();
                let recog = templates::recognize_with_known_needed(
                    &gray,
                    &templates::EMBEDDED,
                    req.quantity,
                );
                let Some((_, parsed_y)) = templates::split_progress(&recog.recognised) else {
                    continue;
                };
                if parsed_y != req.quantity {
                    continue;
                }
                // Cells with multi-digit Y need >1 X component to parse;
                // single-digit X is at index 0.
                let y_n = req.quantity.to_string().chars().count();
                let total = recog.kept_components.len();
                if total != y_n + 1 + 1 {
                    continue; // expect exactly 1 X + slash + y_n Y digits
                }
                let x_comp = &recog.kept_components[0];
                let digit_char = char::from_digit(label.owned, 10).unwrap();
                let filename = format!("{}.png", digit_char);
                let target = out_dir.join(&filename);
                if target.exists() {
                    continue;
                }
                // Reconstruct the component mask by cropping the binarised
                // strip at the component's bounding box. The mask in
                // KeptComponent isn't preserved across the recognize_*
                // helpers, but the BBox is — re-crop is straightforward.
                let comp_img = gray.view(x_comp.x, x_comp.y, x_comp.w, x_comp.h).to_image();
                match comp_img.save(&target) {
                    Ok(()) => {
                        saved.insert(digit_char, format!("{stem} cell{}", req.item_id));
                        eprintln!(
                            "  saved {filename} ({}×{}) from {stem} ({})",
                            x_comp.w, x_comp.h, req.item_id
                        );
                    }
                    Err(e) => eprintln!("  FAILED to save {filename}: {e}"),
                }
            }
        }

        eprintln!("\n=== X-digit extraction summary ===");
        for digit in "0123456789".chars() {
            match saved.get(&digit) {
                Some(src) => eprintln!("  {digit}: ✓ from {src}"),
                None => {
                    eprintln!("  {digit}: skipped (template already present, or no clean fixture)")
                }
            }
        }
    }

    /// One-shot regen for the "1" digit template. The bootstrap
    /// extraction in `extract_digit_templates_from_native_pngs` saved
    /// a 7×8 fragment as `1.png` — far shorter than real "1" glyphs
    /// in the chunky game font (h≈11-12). That undersized template
    /// scored worse than the closed-loop digits even against narrow
    /// vertical-bar components, so leading "1" reads in `1/5`-style
    /// cells lost to "0" or "/" template matches and propagated as
    /// wrong X values (BookcaseLv1 cell 0, StorageZoneLock3 cell 0).
    ///
    /// This regen writes a 5×11 tall vertical-bar "1" with the
    /// canonical top serif + bottom foot of the game font. Run with:
    /// `cargo test -p ez-wishlist-overlay ocr::pipeline::fixture_tests::regen_one_template -- --ignored --nocapture`
    #[test]
    #[ignore = "one-shot — regenerates assets/ocr_templates/1.png"]
    fn regen_one_template() {
        let glyph = [
            ". # # # .",
            "# # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            ". # # # .",
            "# # # # #",
        ];
        let h = glyph.len() as u32;
        let w = glyph[0].split_whitespace().count() as u32;
        let mut img = image::GrayImage::from_pixel(w, h, image::Luma([255]));
        for (y, row) in glyph.iter().enumerate() {
            for (x, cell) in row.split_whitespace().enumerate() {
                if cell == "#" {
                    img.put_pixel(x as u32, y as u32, image::Luma([0]));
                }
            }
        }
        let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/assets/ocr_templates/1.png");
        img.save(&out).expect("write 1.png");
        eprintln!("wrote {} ({w}×{h})", out.display());
    }

    /// One-shot regen for the "0" digit template. The bootstrap
    /// extractor in `extract_digit_templates_from_native_pngs` pulls
    /// "0" from `/10` cells, but those only exist in StorageZoneLock3
    /// captures and the "0" glyph there fragments into two
    /// disconnected arcs at that panel's small scale — the resulting
    /// 0.png is an L-shaped half-glyph that scores misleadingly high
    /// against narrow vertical-bar components (the real "1"s),
    /// turning "1/5" reads into "0/5".
    ///
    /// This regen writes a known-good 7×11 closed-loop "0" matching
    /// the game's chunky pixel font. Run with:
    /// `cargo test -p ez-wishlist-overlay ocr::pipeline::fixture_tests::regen_zero_template -- --ignored --nocapture`
    #[test]
    #[ignore = "one-shot — regenerates assets/ocr_templates/0.png"]
    fn regen_zero_template() {
        let glyph = [
            ". # # # # # .",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            "# # . . . # #",
            ". # # # # # .",
        ];
        let h = glyph.len() as u32;
        let w = glyph[0].split_whitespace().count() as u32;
        let mut img = image::GrayImage::from_pixel(w, h, image::Luma([255]));
        for (y, row) in glyph.iter().enumerate() {
            for (x, cell) in row.split_whitespace().enumerate() {
                if cell == "#" {
                    img.put_pixel(x as u32, y as u32, image::Luma([0]));
                }
            }
        }
        let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/assets/ocr_templates/0.png");
        img.save(&out).expect("write 0.png");
        eprintln!("wrote {} ({w}×{h})", out.display());
    }

    /// Save an upscaled BookcaseLv1 cell 0 binarized strip so we can
    /// tell at a glance whether the leftmost glyph is genuinely "1"
    /// or "0".
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn upscale_bookcase_cell0() {
        use crate::ocr::{anchor, engine, prep};
        use image::GenericImageView;
        let data = load_data();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../screenshots/hideout/BookcaseLv1.webp");
        let img = image::open(&path).expect("open");
        let (img_w, img_h) = img.dimensions();
        let words = engine::recognize_image(&img).expect("OCR");
        let layout = anchor::detect_panel(&words, img_w, img_h).expect("anchor");
        let upgrade = data
            .modules
            .iter()
            .flat_map(|m| &m.upgrades)
            .find(|u| u.id == "BookcaseLv1")
            .unwrap();
        let cells = if layout.cells.len() == upgrade.requirements.len() {
            layout.cells.clone()
        } else {
            anchor::positional_cells(&layout, &words, upgrade.requirements.len())
        };
        let prepped = prep::keep_white_invert(&img);
        for (i, cell) in cells.iter().enumerate() {
            let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
            let up = image::imageops::resize(
                &strip.to_rgba8(),
                strip.width() * 4,
                strip.height() * 4,
                image::imageops::FilterType::Nearest,
            );
            let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../target/ocr_cells/BookcaseLv1_cell{i}_4x.png"));
            up.save(&out).expect("save");
            eprintln!("saved {}", out.display());
        }
    }

    /// Print the per-template scores for each component in
    /// BookcaseLv1 cell 0's count strip. Helps explain template
    /// confusions like "1" → "0".
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_template_scores_bookcase_cell0() {
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;
        let data = load_data();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../screenshots/hideout/BookcaseLv1.webp");
        let img = image::open(&path).expect("open");
        let (img_w, img_h) = img.dimensions();
        let words = engine::recognize_image(&img).expect("OCR");
        let layout = anchor::detect_panel(&words, img_w, img_h).expect("anchor");
        let upgrade = data
            .modules
            .iter()
            .flat_map(|m| &m.upgrades)
            .find(|u| u.id == "BookcaseLv1")
            .unwrap();
        let cells = if layout.cells.len() == upgrade.requirements.len() {
            layout.cells.clone()
        } else {
            anchor::positional_cells(&layout, &words, upgrade.requirements.len())
        };
        let prepped = prep::keep_white_invert(&img);
        let cell = &cells[0];
        let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
        let gray = strip.to_luma8();
        let img_h = gray.height();
        let mut comps = templates::find_components(&gray);
        comps.retain(|c| c.w >= 2 && c.h >= 8 && c.y > 0 && c.y + c.h < img_h);
        if !comps.is_empty() {
            let min_y = comps.iter().map(|c| c.y).min().unwrap();
            let max_h = comps.iter().map(|c| c.h).max().unwrap();
            let row_cutoff = min_y + max_h;
            comps.retain(|c| c.y <= row_cutoff);
        }
        comps.sort_by_key(|c| c.x);
        for (i, c) in comps.iter().enumerate() {
            eprintln!("comp {i}: x={} y={} w={} h={}", c.x, c.y, c.w, c.h);
            let mut scores: Vec<(char, f32)> = templates::EMBEDDED
                .iter()
                .map(|t| (t.label, templates::score(c, t)))
                .collect();
            scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            for (label, s) in &scores {
                eprintln!("   {:?} = {:.3}", label, s);
            }
        }
    }

    /// One-shot: list the connected components find_components produces
    /// for BookcaseLv1's first cell strip, so we can see why the
    /// leading "1" digit isn't reaching the template matcher.
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_components_bookcase_cell0() {
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;
        let data = load_data();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../screenshots/hideout/BookcaseLv1.webp");
        let img = image::open(&path).expect("open Bookcase");
        let (img_w, img_h) = img.dimensions();
        let words = engine::recognize_image(&img).expect("OCR");
        let layout = anchor::detect_panel(&words, img_w, img_h).expect("anchor");
        let upgrade = data
            .modules
            .iter()
            .flat_map(|m| &m.upgrades)
            .find(|u| u.id == "BookcaseLv1")
            .unwrap();
        let cells = if layout.cells.len() == upgrade.requirements.len() {
            layout.cells.clone()
        } else {
            anchor::positional_cells(&layout, &words, upgrade.requirements.len())
        };
        let prepped = prep::keep_white_invert(&img);
        for (idx, cell) in cells.iter().enumerate() {
            let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
            let gray = strip.to_luma8();
            let comps = templates::find_components(&gray);
            eprintln!(
                "cell {idx}: rect {}×{} @ ({},{}); {} comps before filter:",
                cell.w,
                cell.h,
                cell.x,
                cell.y,
                comps.len(),
            );
            for c in &comps {
                eprintln!("  x={:>3} y={:>3} w={:>3} h={:>3}", c.x, c.y, c.w, c.h);
            }
            let recognised = templates::recognize(&gray, &templates::EMBEDDED);
            eprintln!("  → recognised: {:?}", recognised);
        }
    }

    /// Dump each fixture's per-cell count strip as a standalone PNG
    /// under `/tmp/ocr_cells/` so a human can read the X/Y ground
    /// truth at sane resolution. The full 3K-per-eye PNGs scale down
    /// past readability in any preview, but a ~400×60 strip stays
    /// legible. Output filenames: `<UpgradeId>_cell<N>.png`.
    #[test]
    #[ignore = "diagnostic — run with --ignored to dump labelled cell strips"]
    fn dump_native_count_strips() {
        use crate::ocr::{anchor, engine, prep};
        use image::GenericImageView;

        let data = load_data();
        let in_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ocr_cells");
        std::fs::create_dir_all(&out_dir).expect("create output dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in &entries {
            let path = entry.path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let img = match image::open(&path) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("SKIP {stem}: open failed: {e}");
                    continue;
                }
            };
            let (img_w, img_h) = img.dimensions();
            let upgrade = data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem);
            let words = match engine::recognize_image(&img) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("SKIP {stem}: OCR failed: {e}");
                    continue;
                }
            };
            let layout = match anchor::detect_panel(&words, img_w, img_h) {
                Some(l) => l,
                None => {
                    eprintln!("SKIP {stem}: panel anchor not found");
                    continue;
                }
            };
            let cells = if let Some(u) = upgrade {
                if layout.cells.len() == u.requirements.len() {
                    layout.cells.clone()
                } else {
                    anchor::positional_cells(&layout, &words, u.requirements.len())
                }
            } else {
                layout.cells.clone()
            };
            for (idx, cell) in cells.iter().enumerate() {
                let strip = img.crop_imm(cell.x, cell.y, cell.w, cell.h);
                let out_path = out_dir.join(format!("{stem}_cell{idx}.png"));
                if let Err(e) = strip.save(&out_path) {
                    eprintln!("SKIP {stem} cell{idx}: save failed: {e}");
                    continue;
                }
                let req_label = upgrade
                    .and_then(|u| u.requirements.get(idx))
                    .map(|r| format!("{} needed={}", r.item_id, r.quantity))
                    .unwrap_or_else(|| "(no requirement metadata)".into());
                eprintln!("saved {} ({req_label})", out_path.display());
            }

            // Also dump a contextual crop showing the full panel (header
            // + cell strip) for orientation. Useful when the cell strip
            // is too cropped to identify which item it belongs to.
            let panel_crop = {
                let cell_top = cells.iter().map(|c| c.y).min().unwrap_or(layout.anchor.y);
                let cell_bot = cells
                    .iter()
                    .map(|c| c.y + c.h)
                    .max()
                    .unwrap_or(layout.anchor.y + layout.anchor.h);
                let cell_left = cells.iter().map(|c| c.x).min().unwrap_or(0);
                let cell_right = cells.iter().map(|c| c.x + c.w).max().unwrap_or(img_w);
                let pad_y = 12u32;
                let pad_x = 12u32;
                let cx = cell_left.saturating_sub(pad_x);
                let cy = cell_top.saturating_sub(pad_y);
                let cw = (cell_right + pad_x).min(img_w) - cx;
                let ch = (cell_bot + pad_y).min(img_h) - cy;
                img.crop_imm(cx, cy, cw, ch)
            };
            let panel_path = out_dir.join(format!("{stem}_panel.png"));
            let _ = panel_crop.save(&panel_path);
            // Drop a stable suppression of prep::keep_white_invert too
            // — useful for inspecting the binarization the pipeline
            // actually feeds the template matcher.
            let prepped = prep::keep_white_invert(&img);
            for (idx, cell) in cells.iter().enumerate() {
                let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
                let out_path = out_dir.join(format!("{stem}_cell{idx}_binarized.png"));
                let _ = strip.save(&out_path);
            }
            let _ = panel_path;
        }
    }

    #[test]
    #[ignore = "bootstrap — run with --ignored after populating screenshots/hideout/"]
    fn extract_digit_templates_from_native_pngs() {
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;

        let data = load_data();
        let in_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/assets/ocr_templates");
        std::fs::create_dir_all(&out_dir).expect("create output dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        assert!(!entries.is_empty(), "no PNGs in {}", in_dir.display());

        let mut saved: std::collections::BTreeMap<char, String> = std::collections::BTreeMap::new();

        for entry in &entries {
            let path = entry.path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();

            let img = match image::open(&path) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("SKIP  {stem}: open failed: {e}");
                    continue;
                }
            };
            let (img_w, img_h) = img.dimensions();
            let upgrade = match data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem)
            {
                Some(u) => u,
                None => {
                    eprintln!("SKIP  {stem}: filename doesn't match any Upgrade.id");
                    continue;
                }
            };

            let full = match engine::recognize_image(&img) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("SKIP  {stem}: first-pass OCR failed: {e}");
                    continue;
                }
            };
            let layout = match anchor::detect_panel(&full, img_w, img_h) {
                Some(l) => l,
                None => {
                    eprintln!("SKIP  {stem}: no panel detected");
                    continue;
                }
            };
            eprintln!(
                "{stem}: anchor y={} h={}; FROM-RAID cells: {}",
                layout.anchor.y,
                layout.anchor.h,
                layout.cells.len(),
            );

            let cells = if layout.cells.len() == upgrade.requirements.len() {
                layout.cells.clone()
            } else {
                anchor::positional_cells(&layout, &full, upgrade.requirements.len())
            };
            if cells.len() != upgrade.requirements.len() {
                eprintln!(
                    "SKIP  {stem}: cell count {} != requirements {}",
                    cells.len(),
                    upgrade.requirements.len(),
                );
                continue;
            }

            let prepped = prep::keep_white_invert(&img);
            for (cell_idx, (cell, req)) in cells.iter().zip(upgrade.requirements.iter()).enumerate()
            {
                let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
                // Pad with 12 px of white border so any digit that
                // would otherwise touch the strip edges ends up safely
                // inside, where
                // `find_components`' edge guard won't drop it.
                let strip_w = strip.width();
                let strip_h = strip.height();
                const PAD: u32 = 12;
                let mut gray = image::GrayImage::from_pixel(
                    strip_w + PAD * 2,
                    strip_h + PAD * 2,
                    image::Luma([255]),
                );
                let strip_gray = strip.to_luma8();
                for y in 0..strip_h {
                    for x in 0..strip_w {
                        gray.put_pixel(x + PAD, y + PAD, *strip_gray.get_pixel(x, y));
                    }
                }
                let mut comps = templates::find_components(&gray);
                let padded_h = gray.height();
                // Drop tiny noise + edge-touching artifacts.
                comps.retain(|c| c.w >= 4 && c.h >= 8 && c.y > 0 && c.y + c.h < padded_h);
                // The strip is wide (3 text-rows tall) so it can
                // contain TWO horizontal rows of components: the
                // digit row at top, and the FROM RAID label at
                // bottom. Cluster by Y row and keep only the topmost
                // cluster — that's the digits.
                if !comps.is_empty() {
                    let min_y = comps.iter().map(|c| c.y).min().unwrap();
                    let max_h = comps.iter().map(|c| c.h).max().unwrap();
                    // Tolerance: components within ±h of min_y belong
                    // to the topmost row.
                    let row_cutoff = min_y + max_h;
                    comps.retain(|c| c.y <= row_cutoff);
                }
                comps.sort_by_key(|c| c.x);
                if std::env::var_os("OCR_EXTRACT_DEBUG").is_some() {
                    eprintln!(
                        "  {stem} cell{cell_idx} (Y={}): {} components in top row: {:?}",
                        req.quantity,
                        comps.len(),
                        comps
                            .iter()
                            .map(|c| (c.x, c.y, c.w, c.h))
                            .collect::<Vec<_>>(),
                    );
                }

                let y_str = req.quantity.to_string();
                let y_n = y_str.len();
                if comps.len() < y_n + 1 {
                    eprintln!(
                        "  {stem} cell {cell_idx}: only {} components (need ≥ {} for Y={} + slash)",
                        comps.len(),
                        y_n + 1,
                        req.quantity,
                    );
                    continue;
                }

                // Last y_n components = Y digits (known labels).
                let y_start = comps.len() - y_n;
                for (digit_char, comp) in y_str.chars().zip(&comps[y_start..]) {
                    save_template_if_missing(
                        comp, digit_char, &out_dir, &mut saved, &stem, cell_idx,
                    );
                }
                // The one immediately left of the Y digits = slash.
                let slash_comp = &comps[y_start - 1];
                save_template_if_missing(slash_comp, '/', &out_dir, &mut saved, &stem, cell_idx);
            }
        }

        eprintln!("\n=== template extraction summary ===");
        for digit in "0123456789".chars().chain(std::iter::once('/')) {
            match saved.get(&digit) {
                Some(src) => eprintln!("  {digit}: ✓ from {src}"),
                None => eprintln!("  {digit}: MISSING — need a panel with this digit"),
            }
        }
    }

    fn save_template_if_missing(
        comp: &crate::ocr::templates::Component,
        label: char,
        out_dir: &std::path::Path,
        saved: &mut std::collections::BTreeMap<char, String>,
        source_stem: &str,
        cell_idx: usize,
    ) {
        let filename = match label {
            '/' => "slash.png".to_string(),
            c => format!("{c}.png"),
        };
        let path = out_dir.join(&filename);
        if path.exists() {
            return;
        }
        let mut img = image::GrayImage::from_pixel(comp.w, comp.h, image::Luma([255]));
        for y in 0..comp.h {
            for x in 0..comp.w {
                if comp.mask[(y * comp.w + x) as usize] {
                    img.put_pixel(x, y, image::Luma([0]));
                }
            }
        }
        match img.save(&path) {
            Ok(()) => {
                saved.insert(label, format!("{source_stem} cell{cell_idx}"));
                eprintln!(
                    "  saved {filename} ({}×{}) from {source_stem} cell{cell_idx}",
                    comp.w, comp.h
                );
            }
            Err(e) => eprintln!("  FAILED to save {filename}: {e}"),
        }
    }

    /// Diagnostic: run the full pipeline against every native PNG and
    /// print the resolved upgrade + per-cell `(item_id, owned)` it
    /// produces. Validates whether the 9 committed templates (0-6, 8,
    /// slash; missing 7 and 9) produce sensible owned counts in
    /// practice. Run with:
    /// `cargo test -p ez-wishlist-overlay ocr::pipeline::fixture_tests::read_native_pngs -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn read_native_pngs() {
        let data = load_data();
        let in_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        assert!(!entries.is_empty(), "no native PNGs to test against");

        eprintln!(
            "templates loaded: {}",
            crate::ocr::templates::EMBEDDED.len()
        );
        eprintln!();

        for entry in &entries {
            let path = entry.path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            match super::process_screenshot(&path, &data, true, false) {
                Ok(ocr::OcrPipelineResult::Identified(outcome)) => {
                    let upgrade = data
                        .modules
                        .iter()
                        .flat_map(|m| &m.upgrades)
                        .find(|u| u.id == outcome.upgrade_id);
                    let id_ok = outcome.upgrade_id == stem;
                    eprintln!(
                        "{} {stem} -> {} ({})",
                        if id_ok { "OK  " } else { "MISS" },
                        outcome.upgrade_id,
                        outcome.upgrade_name,
                    );
                    for (i, (item_id, owned)) in outcome.items.iter().enumerate() {
                        let need = upgrade
                            .and_then(|u| u.requirements.get(i))
                            .map(|r| r.quantity)
                            .unwrap_or(0);
                        match owned {
                            Some(o) => eprintln!("       cell {i}: {item_id} = {o}/{need}"),
                            None => eprintln!(
                                "       cell {i}: {item_id} = UNREAD/{need} (cell unreadable)"
                            ),
                        }
                    }
                }
                Ok(ocr::OcrPipelineResult::NoPanel) => {
                    eprintln!("NONE  {stem}: pipeline returned NoPanel");
                }
                Ok(ocr::OcrPipelineResult::UnknownUpgrade {
                    module_hint,
                    current_level,
                }) => {
                    eprintln!(
                        "UNK   {stem}: unknown upgrade (hint={module_hint:?} lv={current_level})",
                    );
                }
                Err(e) => eprintln!("ERR   {stem}: {e:#}"),
            }
        }
    }

    /// Structured, **noise-aware** eval report scoring all three assets
    /// **independently** (the `ocr-tune` skill / `scripts/ocr-eval.ps1` /
    /// `scripts/ocr-eval-compare.ps1`).
    ///
    /// Emits one JSON object with a per-asset section:
    ///   - `hideout` — runs the full pipeline over every `screenshots/hideout/`
    ///     fixture `OCR_EVAL_RUNS` times (default 3) so the Windows.Media.Ocr
    ///     run-to-run jitter (~±2 cells, documented on
    ///     `owned_count_accuracy_floor_on_native_pngs`) shows up as a
    ///     `min/median/max` band instead of one fragile number. A change is
    ///     only a real improvement when it clears that band.
    ///   - `box` / `stash` — deterministic, graded tile accuracy
    ///     (`tiles_correct / tiles_total`, `exact_match`, `missing`, `extra`)
    ///     from the frozen `.boxes.json` fixtures, via
    ///     `box_scan::tests::score_scan`. No live engine, so no band.
    ///   - `units` — per-asset isolated-OCR tally over the hand-cropped
    ///     `screenshots/<asset>/units/` tiles (`gated_ok/gated_total` + the
    ///     `#hard` count), via `unit_ocr_tests::score_units`. Live engine, one
    ///     pass each.
    ///
    /// This is the *fine-grained signal* the tuning loop diffs before vs
    /// after a change (per-fixture + per-cell deltas, the noise band,
    /// `wrong_writes_max` for data-safety, and the box/stash tile scores).
    /// The hard pass/fail **gates** stay where they are — `identification_…`
    /// (15/15) and `owned_count_accuracy_floor_…` (≥45) and `box_scan_…`
    /// (exact) — so a regression that breaks a gate fails the normal
    /// `cargo test ocr` run and the loop reverts regardless of this JSON.
    ///
    /// Output: the path in `OCR_EVAL_OUT` when set (parent dirs created),
    /// else stdout between `<<<OCR_EVAL_JSON>>>` / `<<<END_OCR_EVAL_JSON>>>`
    /// markers so a wrapper can lift it out of libtest noise. Uses
    /// `debug_dumps=false`, so it writes no `.cell*/.ocr-debug` siblings.
    ///
    /// Run (Windows dev-shell):
    /// ```text
    /// $env:OCR_EVAL_RUNS=5; $env:OCR_EVAL_OUT="C:\zt\ocr-eval\baseline.json"
    /// cargo test -p ez-wishlist-overlay --target x86_64-pc-windows-msvc \
    ///   ocr::pipeline::fixture_tests::eval_report_json -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "diagnostic — structured eval report for the OCR-tuning loop; run with --ignored"]
    fn eval_report_json() {
        use crate::ocr::debug_dump;

        #[derive(serde::Serialize)]
        struct CellReport {
            index: usize,
            item_id: String,
            needed: u32,
            label_owned: Option<u32>,
            label_needed: Option<u32>,
            /// One entry per run: the read owned-count as a string, or
            /// `"UNREAD"` when the cell didn't parse that run.
            reads: Vec<String>,
            /// How many of the `runs` reads matched the ground-truth label.
            correct_runs: usize,
        }
        #[derive(serde::Serialize)]
        struct FixtureReport {
            stem: String,
            /// True only when **every** run identified the upgrade as `stem`.
            identified: bool,
            /// What the first run actually resolved to (for misident triage).
            identified_as: String,
            labelled: usize,
            correct_min: usize,
            correct_median: usize,
            correct_max: usize,
            cells: Vec<CellReport>,
        }
        #[derive(serde::Serialize)]
        struct Totals {
            fixtures: usize,
            identified: usize,
            labelled: usize,
            correct_min: usize,
            correct_median: usize,
            correct_max: usize,
            /// Worst-case count of labelled cells that read a *wrong* value
            /// (not UNREAD) across the runs. Must stay 0 — a non-zero here
            /// means the pipeline would overwrite real progress with garbage.
            wrong_writes_max: usize,
        }
        /// The hideout asset: live-OCR owned-count band + identification.
        #[derive(serde::Serialize)]
        struct HideoutReport {
            totals: Totals,
            fixtures: Vec<FixtureReport>,
        }
        /// A box-scan asset (box / stash): deterministic, graded tile accuracy
        /// from the frozen `.boxes.json` fixtures. Scored independently of the
        /// noisy hideout band — these don't run the live engine.
        #[derive(serde::Serialize)]
        struct ScanAssetReport {
            scans: Vec<crate::ocr::box_scan::tests::ScanScore>,
            /// Aggregate `tiles_correct / tiles_total` across this asset's
            /// scans, in [0, 1].
            score: f64,
            tiles_correct: u32,
            tiles_total: u32,
            /// True when every scan in this asset matches its label exactly
            /// (the gate-equivalent for `box`).
            all_exact: bool,
            /// Why this asset isn't gated, when applicable (e.g. stash).
            note: Option<String>,
        }
        /// Combined report scoring each asset type **independently**: `hideout`
        /// (live OCR, noise band), `box` and `stash` (deterministic tile score).
        #[derive(serde::Serialize)]
        struct Report {
            runs: usize,
            templates_loaded: usize,
            hideout: HideoutReport,
            #[serde(rename = "box")]
            box_: ScanAssetReport,
            stash: ScanAssetReport,
            /// Per-asset isolated-OCR unit tallies (gated pass / total + `#hard`),
            /// from `unit_ocr_tests`. The hand-cropped `screenshots/<asset>/units/`
            /// tiles, scored independently of the full-panel/full-scan numbers.
            units: Vec<super::unit_ocr_tests::UnitScore>,
            /// Per-unit read detail (one entry per committed crop, all assets),
            /// each with its per-run OCR reads. Backs the per-image
            /// `units/<file>.ocr-result.txt` sidecars.
            unit_results: Vec<super::unit_ocr_tests::UnitResult>,
        }

        fn median(mut v: Vec<usize>) -> usize {
            if v.is_empty() {
                return 0;
            }
            v.sort_unstable();
            v[v.len() / 2]
        }

        let runs: usize = std::env::var("OCR_EVAL_RUNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(3);

        let data = load_data();
        let dir = fixture_dir();
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        assert!(
            !entries.is_empty(),
            "no native fixtures under {}",
            dir.display()
        );

        let mut fixtures: Vec<FixtureReport> = Vec::new();
        // Per-run owned-count totals across all fixtures — the noise band.
        let mut tot_correct_per_run = vec![0usize; runs];
        let mut tot_wrong_per_run = vec![0usize; runs];
        let mut tot_labelled = 0usize;
        let mut identified_fixtures = 0usize;

        for entry in &entries {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let labels = match debug_dump::load_labels(&path) {
                Some(l) => l,
                None => continue,
            };

            // `stem` is the ground-truth Upgrade.id, so the requirement
            // list (and thus the cell order + needed counts) is known
            // independently of what the pipeline resolves — that keeps
            // `labelled` stable even if a run mis-identifies.
            let upgrade = data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem);

            let mut per_run_items: Vec<Vec<(String, Option<u32>)>> = Vec::with_capacity(runs);
            let mut identified_as = String::new();
            let mut all_identified = runs > 0;
            for _ in 0..runs {
                match super::process_screenshot(&path, &data, false, false) {
                    Ok(ocr::OcrPipelineResult::Identified(o)) => {
                        if identified_as.is_empty() {
                            identified_as = o.upgrade_id.clone();
                        }
                        if o.upgrade_id != stem {
                            all_identified = false;
                        }
                        per_run_items.push(o.items);
                    }
                    _ => {
                        all_identified = false;
                        per_run_items.push(Vec::new());
                    }
                }
            }
            if all_identified {
                identified_fixtures += 1;
            }

            let reqs: &[_] = match upgrade {
                Some(u) => u.requirements.as_slice(),
                None => &[],
            };
            let mut cells: Vec<CellReport> = Vec::new();
            let mut fixture_labelled = 0usize;
            let mut fix_correct_per_run = vec![0usize; runs];
            for (i, req) in reqs.iter().enumerate() {
                let label = labels.iter().find(|l| l.item_id == req.item_id);
                // A cell only counts toward accuracy when its label's
                // `needed` agrees with data.json's quantity (mirrors the
                // floor test); otherwise the label row is stale/unusable.
                let is_labelled = label.map(|l| l.needed == req.quantity).unwrap_or(false);
                if is_labelled {
                    fixture_labelled += 1;
                }
                let mut reads: Vec<String> = Vec::with_capacity(runs);
                let mut correct_runs = 0usize;
                for (r, items) in per_run_items.iter().enumerate() {
                    let read = items.get(i).and_then(|(_, owned)| *owned);
                    reads.push(match read {
                        Some(n) => n.to_string(),
                        None => "UNREAD".to_string(),
                    });
                    if let (true, Some(lbl), Some(rv)) = (is_labelled, label, read) {
                        if lbl.owned == rv {
                            correct_runs += 1;
                            fix_correct_per_run[r] += 1;
                        } else {
                            tot_wrong_per_run[r] += 1;
                        }
                    }
                }
                cells.push(CellReport {
                    index: i,
                    item_id: req.item_id.clone(),
                    needed: req.quantity,
                    label_owned: label.map(|l| l.owned),
                    label_needed: label.map(|l| l.needed),
                    reads,
                    correct_runs,
                });
            }
            for (r, c) in fix_correct_per_run.iter().enumerate() {
                tot_correct_per_run[r] += c;
            }
            tot_labelled += fixture_labelled;

            fixtures.push(FixtureReport {
                stem,
                identified: all_identified,
                identified_as,
                labelled: fixture_labelled,
                correct_min: *fix_correct_per_run.iter().min().unwrap_or(&0),
                correct_median: median(fix_correct_per_run.clone()),
                correct_max: *fix_correct_per_run.iter().max().unwrap_or(&0),
                cells,
            });
        }

        let totals = Totals {
            fixtures: fixtures.len(),
            identified: identified_fixtures,
            labelled: tot_labelled,
            correct_min: *tot_correct_per_run.iter().min().unwrap_or(&0),
            correct_median: median(tot_correct_per_run.clone()),
            correct_max: *tot_correct_per_run.iter().max().unwrap_or(&0),
            wrong_writes_max: *tot_wrong_per_run.iter().max().unwrap_or(&0),
        };

        // --- box + stash: deterministic, graded tile accuracy from the frozen
        //     `.boxes.json` fixtures (reuses the box-scan test scorer). Scored
        //     independently of the noisy hideout band — no live engine here. ---
        let asset_report = |scans: Vec<crate::ocr::box_scan::tests::ScanScore>,
                            note: Option<&str>|
         -> ScanAssetReport {
            let tiles_correct: u32 = scans.iter().map(|s| s.tiles_correct).sum();
            let tiles_total: u32 = scans.iter().map(|s| s.tiles_total).sum();
            ScanAssetReport {
                score: if tiles_total > 0 {
                    tiles_correct as f64 / tiles_total as f64
                } else {
                    0.0
                },
                tiles_correct,
                tiles_total,
                all_exact: scans.iter().all(|s| s.exact_match),
                scans,
                note: note.map(str::to_string),
            }
        };

        let box_shots: Vec<String> = (0..3).map(|i| format!("box.shot{i}.boxes.json")).collect();
        let box_ = asset_report(
            vec![crate::ocr::box_scan::tests::score_scan(
                "box",
                &box_shots,
                "box.label.txt",
            )],
            None,
        );

        let stash_shots: Vec<String> = (0..20)
            .map(|i| format!("stash.shot{i:02}.boxes.json"))
            .collect();
        let stash = asset_report(
            vec![crate::ocr::box_scan::tests::score_scan(
                "stash",
                &stash_shots,
                "stash.label.txt",
            )],
            Some("captures have real scroll gaps (rows in no shot); informational, not gated"),
        );

        // Per-asset unit tallies + per-unit read detail. Each crop is OCR'd
        // `runs` times (the live engine is non-deterministic, just like the
        // hideout panels) so a committed unit result records run-to-run jitter
        // rather than a single lucky read.
        let mut unit_scores = Vec::new();
        let mut unit_results = Vec::new();
        for a in ["hideout", "box", "stash"] {
            let (score, results, _fails) = super::unit_ocr_tests::score_units(a, runs);
            unit_scores.push(score);
            unit_results.extend(results);
        }

        let report = Report {
            runs,
            templates_loaded: crate::ocr::templates::EMBEDDED.len(),
            hideout: HideoutReport { totals, fixtures },
            box_,
            stash,
            units: unit_scores,
            unit_results,
        };
        let json = serde_json::to_string_pretty(&report).expect("serialize eval report");

        if let Some(out) = std::env::var_os("OCR_EVAL_OUT") {
            let out = std::path::PathBuf::from(out);
            if let Some(parent) = out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&out, &json).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
            eprintln!("wrote eval report -> {}", out.display());
        } else {
            println!("<<<OCR_EVAL_JSON>>>");
            println!("{json}");
            println!("<<<END_OCR_EVAL_JSON>>>");
        }
        // Human-readable per-asset scorecard on stderr regardless of output sink.
        let h = &report.hideout.totals;
        eprintln!(
            "eval hideout: {}/{} identified; owned-count min/med/max = {}/{}/{} of {} labelled \
             (runs={}, wrong_writes_max={})",
            h.identified,
            h.fixtures,
            h.correct_min,
            h.correct_median,
            h.correct_max,
            h.labelled,
            report.runs,
            h.wrong_writes_max,
        );
        eprintln!(
            "eval box:     {}/{} tiles (exact={});  stash: {}/{} tiles (exact={})",
            report.box_.tiles_correct,
            report.box_.tiles_total,
            report.box_.all_exact,
            report.stash.tiles_correct,
            report.stash.tiles_total,
            report.stash.all_exact,
        );
        let units = report
            .units
            .iter()
            .map(|u| {
                format!(
                    "{} {}/{} gated, {} hard",
                    u.asset, u.gated_ok, u.gated_total, u.hard_total
                )
            })
            .collect::<Vec<_>>()
            .join(";  ");
        eprintln!("eval units:   {units}");
    }

    /// Diagnostic: dump every OCR'd word for one native PNG, with
    /// focus on the area below the anchor (where FROM RAID labels +
    /// counts live). Path is read from the `OCR_DUMP_PATH` env var
    /// when set so you can point it at user-reported captures
    /// without rebuilding; defaults to CryptoMiningLv2 for the
    /// in-repo fixture sweep.
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_native_png_words() {
        use crate::ocr::engine;
        let path = match std::env::var_os("OCR_DUMP_PATH") {
            Some(p) => std::path::PathBuf::from(p),
            None => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../screenshots/hideout/CryptoMiningLv2.webp"),
        };
        eprintln!("OCR dump for: {}", path.display());
        let img = image::open(&path).expect("open PNG");
        let words = engine::recognize_image(&img).expect("OCR");
        eprintln!("{} words total:", words.len());
        let mut sorted: Vec<_> = words.iter().collect();
        sorted.sort_by(|a, b| a.rect.y.partial_cmp(&b.rect.y).unwrap());
        for w in &sorted {
            eprintln!(
                "  y={:>4.0} x={:>4.0} h={:>3.0}  {:?}",
                w.rect.y, w.rect.x, w.rect.height, w.text,
            );
        }
    }

    /// Diagnostic (read-only): for every fixture, find the cells the
    /// production picker leaves UNREAD, then probe whether a *fine* Y
    /// search centred on the panel's **consensus digit-row** (the median
    /// Y of the cells that DID confirm) would recover them. For each
    /// unread cell it reports the best `X/Y` read found in that band —
    /// its min per-digit confidence, the Y offset from consensus, the
    /// parsed owned value, whether that matches the hand label, and
    /// whether it clears the apply gate (`digits_clear_confidence`).
    ///
    /// This separates the UNREAD failure modes without touching the
    /// pipeline: a cell whose fine probe finds a correct, appliable read
    /// is recoverable by a geometry/alignment fix; one whose probe still
    /// reads the wrong digit (or finds nothing) is a glyph / two-digit-Y
    /// limitation that needs a different lead.
    #[test]
    #[ignore = "diagnostic — run with --ignored --nocapture"]
    fn probe_fine_y_rescue() {
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;

        let data = load_data();
        let dir = fixture_dir();
        let templates = &*templates::EMBEDDED;
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        entries.sort_by_key(|e| e.file_name());

        // Min per-(non-slash)-digit template score for a recognised X/Y,
        // or None when there aren't enough components to form `/<Y>`.
        let min_digit_conf = |recog: &templates::RecognizeDebug, needed: u32| -> Option<f32> {
            let y_n = needed.to_string().chars().count();
            let total = recog.kept_components.len();
            if total < y_n + 1 {
                return None;
            }
            let slash_idx = total - y_n - 1;
            let mut m = f32::INFINITY;
            for (i, k) in recog.kept_components.iter().enumerate() {
                if i == slash_idx {
                    continue;
                }
                let best = k
                    .scores
                    .iter()
                    .find(|(c, _)| *c != '/')
                    .map(|(_, s)| *s)
                    .unwrap_or(0.0);
                m = m.min(best);
            }
            m.is_finite().then_some(m)
        };

        for entry in &entries {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let Some(labels) = crate::ocr::debug_dump::load_labels(&path) else {
                continue;
            };
            let Ok(img) = image::open(&path) else {
                continue;
            };
            let (img_w, img_h) = img.dimensions();
            let Ok(words) = engine::recognize_image(&img) else {
                continue;
            };
            let Some(layout) = anchor::detect_panel(&words, img_w, img_h) else {
                continue;
            };
            let Some(upgrade) = data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem)
            else {
                continue;
            };
            let reqs = &upgrade.requirements;
            let base_cells = if layout.cells.len() == reqs.len() {
                layout.cells.clone()
            } else {
                anchor::positional_cells(&layout, &words, reqs.len())
            };
            let prepped = prep::keep_white_invert(&img);
            let picked = super::pick_best_strip_y(&base_cells, &layout, reqs, &prepped, templates);
            let anchor = layout.anchor;

            // Production read of each picked strip → which cells are appliable.
            let mut appliable_y: Vec<u32> = Vec::new();
            let mut picked_read: Vec<(String, bool)> = Vec::new();
            for (cell, req) in picked.iter().zip(reqs.iter()) {
                let gray = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h).to_luma8();
                let recog = templates::recognize_with_known_needed(&gray, templates, req.quantity);
                let appl = templates::split_progress(&recog.recognised)
                    .map(|(_, y)| y == req.quantity)
                    .unwrap_or(false)
                    && super::digits_clear_confidence(&recog, req.quantity);
                if appl {
                    appliable_y.push(cell.y);
                }
                picked_read.push((recog.recognised, appl));
            }
            if appliable_y.is_empty() {
                eprintln!("{stem}: no appliable cells — cannot form consensus row");
                continue;
            }
            appliable_y.sort_unstable();
            let consensus_y = appliable_y[appliable_y.len() / 2];

            let h = anchor.h.max(20);
            let pads: [(u32, u32); 4] = [
                (0, 0),
                (anchor.h / 2, 0),
                (anchor.h, anchor.h / 2),
                (anchor.h * 3 / 2, anchor.h / 2),
            ];
            let lo = consensus_y as i32 - 2 * anchor.h as i32;
            let hi = consensus_y as i32 + 2 * anchor.h as i32;

            for (i, (_, req)) in picked.iter().zip(reqs.iter()).enumerate() {
                if picked_read[i].1 {
                    continue; // already appliable
                }
                let label = labels.iter().find(|l| l.item_id == req.item_id);
                let lbl_owned = label.map(|l| l.owned);
                let bx = base_cells[i].x;
                let bw = base_cells[i].w;
                // Best Y-matching read in the band, ranked by min digit conf.
                let mut best: Option<(f32, i32, String, u32, bool)> = None;
                let mut y = lo;
                while y <= hi {
                    if y >= 0 && (y as u32) + h < img_h {
                        for (pl, pr) in pads.iter() {
                            let nx = bx.saturating_sub(*pl);
                            let nw = (bw + pl + pr).min(img_w.saturating_sub(nx));
                            if nw == 0 {
                                continue;
                            }
                            let gray = prepped.crop_imm(nx, y as u32, nw, h).to_luma8();
                            let recog = templates::recognize_with_known_needed(
                                &gray,
                                templates,
                                req.quantity,
                            );
                            let Some((px, py)) = templates::split_progress(&recog.recognised)
                            else {
                                continue;
                            };
                            if py != req.quantity {
                                continue;
                            }
                            let Some(mc) = min_digit_conf(&recog, req.quantity) else {
                                continue;
                            };
                            let appl = super::digits_clear_confidence(&recog, req.quantity);
                            if best.as_ref().map(|b| mc > b.0).unwrap_or(true) {
                                best =
                                    Some((mc, y - consensus_y as i32, recog.recognised, px, appl));
                            }
                        }
                    }
                    y += 2;
                }
                match best {
                    Some((mc, dy, read, px, appl)) => {
                        let correct = lbl_owned == Some(px);
                        eprintln!(
                            "  {stem} cell{i} {:<24} need={:<2} label={:?} | pickedRead={:?} \
                             FINE best={:?} minconf={:.3} dy={:+} parsedX={} correctX={} appliable={}",
                            req.item_id, req.quantity, lbl_owned, picked_read[i].0,
                            read, mc, dy, px, correct, appl,
                        );
                    }
                    None => eprintln!(
                        "  {stem} cell{i} {:<24} need={:<2} label={:?} | pickedRead={:?} \
                         FINE no Y-matching read in band",
                        req.item_id, req.quantity, lbl_owned, picked_read[i].0,
                    ),
                }
            }
        }
    }

    /// Identification + cell-ordering regression. For every native
    /// capture in `screenshots/hideout/`:
    ///   - pipeline must return `Some(outcome)`,
    ///   - `outcome.upgrade_id` must equal the filename stem (the
    ///     ground-truth Upgrade.id),
    ///   - `outcome.items` must list every requirement of that upgrade
    ///     in `data.json` declaration order.
    ///
    /// Per-cell `Option<u32>` accuracy isn't asserted here — that's
    /// tracked via the `read_native_pngs` diagnostic and the sibling
    /// `.ocr-debug.txt` dumps. We require 100% pass on the native set
    /// (it's the runtime's real input — any regression here breaks
    /// production OCR).
    #[test]
    fn identification_and_cell_ordering_on_native_pngs() {
        let data = load_data();
        let dir = fixture_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();
        assert!(
            !entries.is_empty(),
            "no native capture fixtures under {}",
            dir.display(),
        );

        let mut pass = 0usize;
        let mut fail: Vec<String> = Vec::new();
        for entry in &entries {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            match super::process_screenshot(&path, &data, true, false) {
                Ok(ocr::OcrPipelineResult::Identified(outcome)) => {
                    let upgrade = data
                        .modules
                        .iter()
                        .flat_map(|m| &m.upgrades)
                        .find(|u| u.id == stem);
                    let req_ids: Vec<String> = upgrade
                        .map(|u| u.requirements.iter().map(|r| r.item_id.clone()).collect())
                        .unwrap_or_default();
                    let outcome_ids: Vec<String> =
                        outcome.items.iter().map(|(id, _)| id.clone()).collect();
                    if outcome.upgrade_id == stem && outcome_ids == req_ids {
                        eprintln!("PASS  {stem}");
                        pass += 1;
                    } else {
                        eprintln!(
                            "FAIL  {stem}: got upgrade_id={:?}, items={:?} (expected items={:?})",
                            outcome.upgrade_id, outcome_ids, req_ids,
                        );
                        fail.push(stem);
                    }
                }
                Ok(ocr::OcrPipelineResult::NoPanel) => {
                    eprintln!("FAIL  {stem}: pipeline returned NoPanel");
                    fail.push(stem);
                }
                Ok(ocr::OcrPipelineResult::UnknownUpgrade { .. }) => {
                    eprintln!("FAIL  {stem}: pipeline returned UnknownUpgrade");
                    fail.push(stem);
                }
                Err(e) => {
                    eprintln!("FAIL  {stem}: pipeline error: {e:#}");
                    fail.push(stem);
                }
            }
        }

        let total = entries.len();
        eprintln!("\nNative-PNG fixture pass rate: {pass} / {total}");
        assert_eq!(
            pass, total,
            "all native PNG fixtures must pass identification + cell ordering; failed: {fail:?}",
        );
    }

    /// Owned-count accuracy regression. For each native capture
    /// fixture paired with a `<UpgradeId>.label.txt`, count cells where
    /// the pipeline reads the same `owned/needed` pair the user hand-
    /// labelled. Asserts a minimum floor so future strip-Y, X-pad, or
    /// template changes can't silently undo the closed-loop gains.
    ///
    /// On the lossless captures the baseline was 48/59 (after issue
    /// #58's hole-count discriminator). The fixtures are now WebP q99
    /// (issue #110, to shrink the repo ~90%); lossy compression
    /// perturbs the chunky pixel-art digits enough to cost ~2 reads, so
    /// it lands ~45-47/59 and can flicker down to the 45 floor (the OCR
    /// engine itself is non-deterministic by ±2 cells run-to-run; that
    /// flicker predates the WebP swap). The floor stays at 45 (the
    /// issue's mandated guardrail): if cross-environment flicker ever
    /// pushes a run below it, re-encode the fixtures lossless rather
    /// than lowering the gate.
    ///
    /// Wrong reads count against accuracy; UNREAD cells count as
    /// "no opinion" and are excluded from the labelled total — they
    /// preserve the user's existing collected value at runtime, so
    /// they're data-safe even when they don't match.
    #[test]
    fn owned_count_accuracy_floor_on_native_pngs() {
        use crate::ocr::debug_dump;
        let data = load_data();
        let dir = fixture_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(is_primary_fixture)
            .collect();

        let mut correct = 0usize;
        let mut labelled = 0usize;
        let mut wrong_writes = 0usize;
        let mut per_fixture: Vec<(String, usize, usize)> = Vec::new();
        for entry in &entries {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let labels = match debug_dump::load_labels(&path) {
                Some(l) => l,
                None => continue,
            };
            let outcome = match super::process_screenshot(&path, &data, false, false) {
                Ok(ocr::OcrPipelineResult::Identified(o)) => o,
                _ => continue,
            };
            let upgrade = data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == outcome.upgrade_id);
            let mut fixture_correct = 0usize;
            let mut fixture_labelled = 0usize;
            for (i, (item_id, owned)) in outcome.items.iter().enumerate() {
                let Some(label) = labels.iter().find(|l| l.item_id == *item_id) else {
                    continue;
                };
                let need = upgrade
                    .and_then(|u| u.requirements.get(i))
                    .map(|r| r.quantity)
                    .unwrap_or(0);
                fixture_labelled += 1;
                if need != label.needed {
                    continue;
                }
                match owned {
                    Some(o) if *o == label.owned => fixture_correct += 1,
                    Some(_) => wrong_writes += 1,
                    None => {}
                }
            }
            correct += fixture_correct;
            labelled += fixture_labelled;
            per_fixture.push((stem, fixture_correct, fixture_labelled));
        }
        per_fixture.sort_by(|a, b| a.0.cmp(&b.0));
        eprintln!("\nOwned-count accuracy per fixture (correct/labelled):");
        for (stem, c, t) in &per_fixture {
            eprintln!("  {c}/{t}  {stem}");
        }
        eprintln!("Total: {correct}/{labelled} correct, {wrong_writes} wrong writes");

        // Floor at 50. The FOV-cropped WebP-q99 fixtures (17 panels, 68
        // labelled cells) read 55/68 stably (runs=3, no flicker) — up from
        // the old set's ~45-47/59, because the tighter crop enlarges the
        // pixel-art count digits. Floor set ~5 below the observed read to
        // lock in the gain while tolerating engine jitter; a sub-50 read
        // means a real regression (or re-encode lossless), not a floor to lower.
        assert!(
            correct >= 50,
            "owned-count accuracy regressed below floor: {correct}/{labelled} (want ≥ 50)"
        );
    }
}

/// Validates the hideout upgrade **database** (`data.json`) against the
/// hand-labelled ground truth in `screenshots/hideout/*.label.txt`. The in-game
/// panel is the source of truth (see `screenshots/CLAUDE.md`), so a divergence
/// here means `data.json` drifted and must be patched — not the label. Pure data
/// check (no OCR engine), so it runs on **every** target.
#[cfg(test)]
mod hideout_data_validation {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// `item_id → needed`, parsed from `<item_id>  <owned>/<needed>` label
    /// lines. `#` comments + blank lines ignored; malformed rows skipped.
    fn label_needs(text: &str) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(id), Some(xy)) = (it.next(), it.next()) else {
                continue;
            };
            if let Some((_, needed)) = xy.split_once('/') {
                if let Ok(n) = needed.parse::<u32>() {
                    out.insert(id.to_string(), n);
                }
            }
        }
        out
    }

    /// Every `screenshots/hideout/<UpgradeId>.label.txt` must agree with that
    /// upgrade's `requirements` in `data.json`: the same set of `item_id`s, each
    /// with the same `needed` quantity. The screenshots are ground truth, so a
    /// failure means patch `data.json` (per `screenshots/CLAUDE.md`), not the
    /// label.
    #[test]
    fn hideout_labels_match_data_json() {
        let data: crate::data::GameData =
            serde_json::from_str(include_str!("../assets/data.json")).expect("data.json parses");
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");

        let mut checked = 0usize;
        let mut errs: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            // Drive off each capture; read its sibling label.
            if path.extension().and_then(|s| s.to_str()) != Some("webp") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let label_path = dir.join(format!("{stem}.label.txt"));
            let Ok(text) = std::fs::read_to_string(&label_path) else {
                errs.push(format!("{stem}: missing {stem}.label.txt"));
                continue;
            };
            let Some(upgrade) = data
                .modules
                .iter()
                .flat_map(|m| &m.upgrades)
                .find(|u| u.id == stem)
            else {
                errs.push(format!("{stem}: no matching Upgrade.id in data.json"));
                continue;
            };

            let got = label_needs(&text);
            let want: BTreeMap<String, u32> = upgrade
                .requirements
                .iter()
                .map(|r| (r.item_id.clone(), r.quantity))
                .collect();
            if got != want {
                errs.push(format!(
                    "{stem}: label (item_id→needed) {got:?} != data.json requirements {want:?}"
                ));
            }
            checked += 1;
        }

        assert!(
            checked >= 15,
            "expected >= 15 hideout fixtures, found {checked}"
        );
        assert!(
            errs.is_empty(),
            "hideout screenshot labels disagree with data.json (screenshots are ground \
             truth — patch data.json, see screenshots/CLAUDE.md):\n  {}",
            errs.join("\n  "),
        );
    }
}

/// Per-item **isolated-OCR** validation. Each asset folder has a `units/`
/// subdirectory of hand-cropped **whole item tiles** (icon + name, plus the
/// owned/needed counter for hideout); `units/labels.txt` maps
/// `<file>  <expected text>`. These tests OCR each lone crop and require the
/// engine to recover every token of the expected text — proving each item
/// reads correctly in an isolated shape, not just embedded in the full
/// panel/scan. Windows-only (uses the live engine); the committed crop set is
/// the curated, known-OCRable one, so a failure is a real regression.
#[cfg(all(test, target_os = "windows"))]
mod unit_ocr_tests {
    use crate::ocr::engine;
    use std::path::{Path, PathBuf};

    fn units_dir(asset: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../screenshots")
            .join(asset)
            .join("units")
    }

    /// `(file, expected text, is_hard)` from `units/labels.txt`. A normal line
    /// `<file>  <text…>` is a **gated** unit (must OCR correctly). A line
    /// `#hard  <file>  <text…>` is a **known-hard** unit — still OCR'd and
    /// reported as an improvement target, but it does not fail the gate (the
    /// engine can't yet read that tile in isolation, e.g. a stylised "UV" or a
    /// tiny angled hideout name). Other `#…` lines are comments.
    fn load_unit_labels(dir: &Path) -> Vec<(String, String, bool)> {
        let Ok(text) = std::fs::read_to_string(dir.join("labels.txt")) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let (line, hard) = match line.strip_prefix("#hard") {
                Some(rest) => (rest.trim(), true),
                None if line.starts_with('#') => continue,
                None => (line, false),
            };
            let mut it = line.splitn(2, char::is_whitespace);
            if let (Some(f), Some(e)) = (it.next(), it.next()) {
                out.push((f.to_string(), e.trim().to_string(), hard));
            }
        }
        out
    }

    /// OCR a crop → its words lowercased, space-joined.
    fn ocr_text(path: &Path) -> String {
        let img = image::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        engine::recognize_image(&img)
            .unwrap_or_default()
            .iter()
            .map(|w| w.text.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every alphanumeric token of `expect` appears (substring-either-way, to
    /// tolerate the engine splitting/merging glyphs) in the OCR `haystack`.
    fn reads_expected(haystack: &str, expect: &str) -> bool {
        expect.split_whitespace().all(|tok| {
            let t: String = tok
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect();
            t.is_empty()
                || haystack
                    .split_whitespace()
                    .map(|w| -> String { w.chars().filter(|c| c.is_alphanumeric()).collect() })
                    .any(|w| !w.is_empty() && (w.contains(&t) || t.contains(&w)))
        })
    }

    /// Assert every committed unit crop for `asset` OCRs to its expected text.
    /// Skips (vacuously) if the asset has no labelled units yet.
    /// Per-asset isolated-OCR unit tally, surfaced in the eval scorecard.
    #[derive(serde::Serialize, Clone)]
    pub(crate) struct UnitScore {
        pub asset: String,
        /// Gated units that OCR their name in isolation, and the gated total.
        pub gated_ok: usize,
        pub gated_total: usize,
        /// `#hard` units that nonetheless read (bonus), and the `#hard` total.
        pub hard_ok: usize,
        pub hard_total: usize,
    }

    /// One committed unit crop's per-run OCR reads — the data behind a per-image
    /// `units/<file>.ocr-result.txt` sidecar. `ok_runs == reads.len()` ⇒ the
    /// tile read its expected name on every run (a clean PASS); `0` ⇒ never;
    /// in between ⇒ FLAKY.
    #[derive(serde::Serialize, Clone)]
    pub(crate) struct UnitResult {
        pub asset: String,
        pub file: String,
        /// The in-game display name the tile shows (ground truth for the read).
        pub expected: String,
        /// A `#hard` unit: reported as an OCR-improvement target, not gated.
        pub hard: bool,
        /// One entry per run: the OCR text (lowercased, space-joined words), or
        /// `"<missing>"` when the crop file is absent.
        pub reads: Vec<String>,
        /// How many of the runs satisfied `reads_expected`.
        pub ok_runs: usize,
    }

    /// OCR every committed unit crop for `asset` `runs` times and tally gated /
    /// `#hard` pass counts (printing a per-unit line). Returns the aggregate
    /// score, the per-unit read detail (for the committed sidecars), and the
    /// list of **gated** failures (empty ⇒ the gate passes). Shared by the gate
    /// test (`assert_units`, `runs = 1`) and the `eval_report_json` scorecard.
    pub(crate) fn score_units(
        asset: &str,
        runs: usize,
    ) -> (UnitScore, Vec<UnitResult>, Vec<String>) {
        let runs = runs.max(1);
        let dir = units_dir(asset);
        let labels = load_unit_labels(&dir);
        let mut fails = Vec::new();
        let mut results = Vec::new();
        let (mut gated_total, mut gated_ok, mut hard_total, mut hard_ok) = (0usize, 0, 0, 0);
        for (file, expect, hard) in &labels {
            let path = dir.join(file);
            let exists = path.exists();
            let mut reads = Vec::with_capacity(runs);
            let mut ok_runs = 0usize;
            for _ in 0..runs {
                let got = if exists {
                    ocr_text(&path)
                } else {
                    "<missing>".to_string()
                };
                if exists && reads_expected(&got, expect) {
                    ok_runs += 1;
                }
                reads.push(got);
            }
            // A clean pass means the tile read its name on *every* run; a unit
            // that only read on some runs is FLAKY, not a pass.
            let ok = exists && ok_runs == runs;
            let tag = if *hard { "hard" } else { "gate" };
            let mark = if ok { "ok " } else { "ERR" };
            eprintln!("  [{mark}|{tag}] {asset}/{file}: expect={expect:?} ok_runs={ok_runs}/{runs} reads={reads:?}");
            if *hard {
                hard_total += 1;
                if ok {
                    hard_ok += 1;
                }
            } else {
                gated_total += 1;
                if ok {
                    gated_ok += 1;
                } else {
                    fails.push(format!(
                        "{asset}/{file}: expected {expect:?}, OCR ok on {ok_runs}/{runs} runs (reads {reads:?})"
                    ));
                }
            }
            results.push(UnitResult {
                asset: asset.to_string(),
                file: file.clone(),
                expected: expect.clone(),
                hard: *hard,
                reads,
                ok_runs,
            });
        }
        eprintln!(
            "{asset} units: {gated_ok}/{gated_total} gated passed ({hard_total} known-hard reported)"
        );
        (
            UnitScore {
                asset: asset.to_string(),
                gated_ok,
                gated_total,
                hard_ok,
                hard_total,
            },
            results,
            fails,
        )
    }

    fn assert_units(asset: &str) {
        let dir = units_dir(asset);
        if load_unit_labels(&dir).is_empty() {
            eprintln!("{asset}: no units in {} — skipping", dir.display());
            return;
        }
        let (_score, _results, fails) = score_units(asset, 1);
        assert!(
            fails.is_empty(),
            "isolated-OCR unit failures (fix the crop or the pipeline, or mark the line \
             `#hard` if it's a tracked OCR limitation):\n  {}",
            fails.join("\n  "),
        );
    }

    #[test]
    fn box_units_ocr_in_isolation() {
        assert_units("box");
    }

    #[test]
    fn stash_units_ocr_in_isolation() {
        assert_units("stash");
    }

    #[test]
    fn hideout_units_ocr_in_isolation() {
        assert_units("hideout");
    }
}
