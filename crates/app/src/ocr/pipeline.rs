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
use crate::ocr::OcrOutcome;
use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "windows")]
pub fn process_screenshot(path: &Path, data: &GameData) -> Result<Option<OcrOutcome>> {
    use crate::ocr::{anchor, engine, match_upgrade, prep, templates};
    use anyhow::Context;
    use image::GenericImageView;

    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    let (img_w, img_h) = img.dimensions();
    if img_w == 0 || img_h == 0 {
        anyhow::bail!("zero-sized image");
    }

    // Single OCR pass on the whole image. The first-pass words feed
    // both anchor detection (for cell layout) and the strict resolver
    // (for upgrade identification — which slides a window over these
    // tokens to find any module.name match). No further crop+re-OCR is
    // needed — the panel-bounds heuristic isn't reliable enough to
    // pixel-accurately crop the row label, and a single OCR pass with
    // tight matching turns out to be both simpler and more robust.
    let full_words = engine::recognize_image(&img).context("first-pass OCR")?;
    let layout = match anchor::detect_panel(&full_words, img_w, img_h) {
        Some(l) => l,
        None => {
            tracing::debug!(
                path = %path.display(),
                words = full_words.len(),
                "OCR pipeline: not an upgrade menu (no submit anchor)",
            );
            return Ok(None);
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
        None => return Ok(None),
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
    let mut items: Vec<(String, Option<u32>)> = Vec::with_capacity(upgrade.requirements.len());
    // Per-cell intermediate state for the debug dump (debug builds only).
    #[cfg(debug_assertions)]
    let mut debug_cells: Vec<crate::ocr::debug_dump::CellDebug<'_>> = Vec::new();
    for (i, (cell, req)) in cells.iter().zip(upgrade.requirements.iter()).enumerate() {
        let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
        let gray = strip.to_luma8();

        // In debug builds, drop the binarised cell strip next to the
        // source screenshot as `<stem>.cell<i>.<HHMMSS>.png` and log
        // its path so the user can open it in any viewer to see what
        // the template matcher actually saw (vs what the screenshot
        // looks like to a human). The strip is small (~150×60 px) so
        // 4 of these per capture is a negligible disk hit. Release
        // builds skip the write entirely.
        #[cfg(debug_assertions)]
        {
            if let Some(strip_path) = crate::ocr::debug_dump::cell_strip_path_for(path, i) {
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
        // Reject the parse unless Y matches the known required quantity.
        // The template matcher CAN return an "X/Y" shape with the wrong
        // Y (slash-forcing only fixes the slash position, not the Y
        // digit's template match), and that's a sign the read is
        // standing on icon noise rather than the digit row. Treating
        // those as UNREAD preserves the user's existing count instead
        // of overwriting it with a confidently-wrong X value (e.g.
        // IntelligentLv2 cell 0 parsing "84/6" when needed=4 — X=84
        // would otherwise land in AppState.collected).
        let owned_opt = parsed.and_then(|(o, y)| (y == req.quantity).then_some(o));
        items.push((req.item_id.clone(), owned_opt));
        if owned_opt.is_none() {
            tracing::warn!(
                item_id = %req.item_id,
                recognised = %recog.recognised,
                "OCR: failed to parse owned count for cell — leaving existing collected value untouched",
            );
        }
        #[cfg(debug_assertions)]
        {
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
        // Silence unused warnings in release builds where debug_cells
        // doesn't exist.
        #[cfg(not(debug_assertions))]
        let _ = i;
    }

    // In debug builds, dump everything the pipeline saw to a sibling
    // file next to the screenshot. The user can open this to see why
    // a specific capture read counts the way it did (or didn't). We
    // sweep any prior `<stem>.ocr-debug.*.txt` first so there's
    // always at most one dump per source — successive runs (the
    // fixture test in particular) used to accumulate noise.
    #[cfg(debug_assertions)]
    {
        use crate::ocr::debug_dump::{self, OcrDebugDump, Resolution};
        debug_dump::purge_prior_dumps(path);
        let labels = debug_dump::load_labels(path);
        let dump = OcrDebugDump {
            source_path: path,
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
        let out = debug_dump::debug_path_for(path);
        match debug_dump::write_text(&dump, &out) {
            Ok(()) => tracing::info!(path = %out.display(), "OCR debug dump written"),
            Err(e) => {
                tracing::warn!(error = %e, path = %out.display(), "OCR debug dump write failed")
            }
        }
    }

    Ok(Some(OcrOutcome {
        upgrade_id: upgrade.id.clone(),
        upgrade_name: module.name.clone(),
        items,
    }))
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
    // covers the rarer trailing-digit clip.
    let x_pad_candidates: [(u32, u32); 4] = [
        (0, 0),
        (anchor.h / 2, 0),
        (anchor.h, 0),
        (anchor.h / 2, anchor.h / 2),
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
        // Higher tier for 1-digit X (the common case); a positive base
        // ensures any confirmed variant outranks an unconfirmed one.
        Some(if parsed_x < 10 { 100 } else { 50 })
    };

    let mut best_scores: Vec<u32> = vec![0; base_cells.len()];

    // First: score the base geometry.
    for (i, (cell, req)) in base_cells.iter().zip(requirements.iter()).enumerate() {
        if let Some(s) = score_variant(cell.x, cell.y, cell.w, cell.h, req.quantity) {
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
                if let Some(s) = score_variant(new_x, *y_top, new_w, new_h, req.quantity) {
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

    let _ = confirmed; // Confirmed bits are an analysis aid, not consumed downstream.
    best_cells
}

#[cfg(not(target_os = "windows"))]
pub fn process_screenshot(_path: &Path, _data: &GameData) -> Result<Option<OcrOutcome>> {
    // Windows.Media.Ocr is not available on non-Windows targets.
    Ok(None)
}

#[cfg(all(test, target_os = "windows"))]
mod fixture_tests {
    //! Integration-style coverage driven by the **native-resolution
    //! PNG fixtures** in `hideout_screenshots_native/`. Each fixture's
    //! filename is the `Upgrade.id` ground truth (e.g.
    //! `BookcaseLv1.png` ↔ `Upgrade.id = "BookcaseLv1"`). We assert
    //! identification + cell ordering match `data.json` here; per-cell
    //! owned-count accuracy is tracked via the `read_native_pngs`
    //! diagnostic (run with `--ignored`) and the sibling
    //! `.ocr-debug.txt` dumps.
    //!
    //! The old `hideout_screenshots/` Steam-F12 JPGs are no longer
    //! part of the test suite — their lossy compression destroyed
    //! the chunky pixel-art digit font and made digit-OCR results
    //! unrepresentative of what the runtime sees on real captures.

    use crate::ocr;
    use std::path::PathBuf;

    fn load_data() -> crate::data::GameData {
        let raw = include_str!("../assets/data.json");
        serde_json::from_str(raw).expect("data.json is valid")
    }

    fn fixture_dir() -> PathBuf {
        // CARGO_MANIFEST_DIR is `crates/app`; native PNGs live at repo root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hideout_screenshots_native")
    }

    /// Sweep every JPG fixture, report a per-image pass/fail line, and
    /// fail the test if the pass count is below the threshold (per the
    /// design plan: ≥ 18/20). Owned-count digits are not asserted; the
    /// JPGs are the regression-detection floor for identification + cell
    /// ordering, not for digit accuracy.
    /// One-shot bootstrap: walk every PNG in `hideout_screenshots_native/`,
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

        let in_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native");
        let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/ocr_cells_wide");
        std::fs::create_dir_all(&out_dir).expect("create out dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
        let in_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/assets/ocr_templates");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
            .join("../../hideout_screenshots_native/BookcaseLv1.png");
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
            .join("../../hideout_screenshots_native/BookcaseLv1.png");
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
            .join("../../hideout_screenshots_native/BookcaseLv1.png");
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
        let in_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ocr_cells");
        std::fs::create_dir_all(&out_dir).expect("create output dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
    #[ignore = "bootstrap — run with --ignored after populating hideout_screenshots_native/"]
    fn extract_digit_templates_from_native_pngs() {
        use crate::ocr::{anchor, engine, prep, templates};
        use image::GenericImageView;

        let data = load_data();
        let in_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native");
        let out_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/assets/ocr_templates");
        std::fs::create_dir_all(&out_dir).expect("create output dir");

        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
        let in_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native");
        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", in_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
            match ocr::process_screenshot(&path, &data) {
                Ok(Some(outcome)) => {
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
                Ok(None) => eprintln!("NONE  {stem}: pipeline returned None"),
                Err(e) => eprintln!("ERR   {stem}: {e:#}"),
            }
        }
    }

    /// Diagnostic: dump every OCR'd word for one native PNG, with
    /// focus on the area below the anchor (where FROM RAID labels +
    /// counts live).
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_native_png_words() {
        use crate::ocr::engine;
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hideout_screenshots_native/CryptoMiningLv2.png");
        let img = image::open(&path).expect("open native PNG");
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

    /// Identification + cell-ordering regression. For every native PNG
    /// in `hideout_screenshots_native/`:
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
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
            .collect();
        assert!(
            !entries.is_empty(),
            "no native PNG fixtures under {}",
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
            match ocr::process_screenshot(&path, &data) {
                Ok(Some(outcome)) => {
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
                Ok(None) => {
                    eprintln!("FAIL  {stem}: pipeline returned None (panel not detected)");
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

    /// Owned-count accuracy regression. For each native PNG fixture
    /// paired with a `<UpgradeId>.label.txt`, count cells where the
    /// pipeline reads the same `owned/needed` pair the user hand-
    /// labelled. Asserts a minimum floor so future strip-Y, X-pad, or
    /// template changes can't silently undo the closed-loop gains
    /// (baseline at 40/59 after iter 5; gate at 35 leaves headroom
    /// for incidental UI noise on real-world captures).
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
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
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
            let outcome = match ocr::process_screenshot(&path, &data) {
                Ok(Some(o)) => o,
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

        // Floor below baseline of 40/59 so incidental fluctuations
        // (e.g. one fixture flickering by 1 cell on a different
        // build environment) don't block CI; large regressions still
        // trip the gate.
        assert!(
            correct >= 35,
            "owned-count accuracy regressed below floor: {correct}/{labelled} (want ≥ 35)"
        );
    }
}
