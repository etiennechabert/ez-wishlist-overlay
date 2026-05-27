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
        anchor::positional_cells(&layout, upgrade.requirements.len())
    };

    // Binarize the full image once; cell strips are cropped from it.
    let prepped = prep::keep_white_invert(&img);
    let templates = &*templates::EMBEDDED;
    let mut items: Vec<(String, u32)> = Vec::with_capacity(upgrade.requirements.len());
    for (cell, req) in cells.iter().zip(upgrade.requirements.iter()) {
        let strip = prepped.crop_imm(cell.x, cell.y, cell.w, cell.h);
        let gray = strip.to_luma8();
        let recognised = templates::recognize(&gray, templates);
        let owned = templates::split_progress(&recognised)
            .map(|(a, _)| a)
            .unwrap_or(0);
        items.push((req.item_id.clone(), owned));
    }

    Ok(Some(OcrOutcome {
        upgrade_id: upgrade.id.clone(),
        upgrade_name: module.name.clone(),
        items,
    }))
}

#[cfg(not(target_os = "windows"))]
pub fn process_screenshot(_path: &Path, _data: &GameData) -> Result<Option<OcrOutcome>> {
    // Windows.Media.Ocr is not available on non-Windows targets.
    Ok(None)
}

#[cfg(all(test, target_os = "windows"))]
mod fixture_tests {
    //! Integration-style coverage driven by the JPEG fixtures in
    //! `hideout_screenshots/`. Each fixture's filename is the
    //! `Upgrade.id` ground truth (Phase 0 convention — see
    //! `hideout_screenshots/CLAUDE.md`). We check three things per
    //! fixture: (a) the pipeline returns `Some(outcome)`, (b)
    //! `outcome.upgrade_id` equals the filename stem, and (c)
    //! `outcome.items` enumerates the matching upgrade's requirements
    //! in `data.json` order. We do NOT assert owned counts — the JPGs
    //! are not representative of digit-template accuracy on the
    //! lossless native PNGs the runtime sees, and the embedded
    //! `ocr_templates/` directory ships empty so digit recognition
    //! reads everything as 0 until a native batch is captured.

    use crate::ocr;
    use std::path::PathBuf;

    fn load_data() -> crate::data::GameData {
        let raw = include_str!("../assets/data.json");
        serde_json::from_str(raw).expect("data.json is valid")
    }

    fn fixture_dir() -> PathBuf {
        // CARGO_MANIFEST_DIR is `crates/app`; fixtures live at repo root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hideout_screenshots")
    }

    /// Sweep every JPG fixture, report a per-image pass/fail line, and
    /// fail the test if the pass count is below the threshold (per the
    /// design plan: ≥ 18/20). Owned-count digits are not asserted; the
    /// JPGs are the regression-detection floor for identification + cell
    /// ordering, not for digit accuracy.
    /// Diagnostic: run the pipeline against one fixture and print the
    /// row-label OCR + match score regardless of outcome. Helps localise
    /// failures that show up only as `pipeline returned None`.
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn trace_pipeline_for_first_fixture() {
        use crate::ocr::{anchor, engine, match_upgrade};
        let data = load_data();
        let dir = fixture_dir();
        let path = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jpg"))
            .expect("at least one fixture")
            .path();
        let img = image::open(&path).expect("decode fixture");
        let (img_w, img_h) = {
            use image::GenericImageView;
            img.dimensions()
        };
        let full = engine::recognize_image(&img).expect("first OCR");
        let layout = anchor::detect_panel(&full, img_w, img_h)
            .expect("anchor 'Need to submit items' should be present");
        eprintln!("panel:    header={:?}  row_label={:?}", layout.header, layout.row_label);
        eprintln!("cells:    {} from FROM RAID anchors", layout.cells.len());

        let row_crop = img.crop_imm(
            layout.row_label.x,
            layout.row_label.y,
            layout.row_label.w,
            layout.row_label.h,
        );
        let row_words = engine::recognize_image(&row_crop).expect("row OCR");
        let row_text: String = row_words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("row OCR:  {:?}", row_text);

        let header_crop = img.crop_imm(
            layout.header.x,
            layout.header.y,
            layout.header.w,
            layout.header.h,
        );
        let header_words = engine::recognize_image(&header_crop).expect("header OCR");
        eprintln!(
            "header:   {:?}",
            header_words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
        );
        let current_level = header_words
            .iter()
            .find_map(|w| anchor::parse_level_token(&w.text))
            .unwrap_or(0);
        eprintln!("LV:       {current_level}");

        match match_upgrade::resolve(&data, &row_text, current_level) {
            Some(id) => eprintln!("RESOLVED: {id}"),
            None => eprintln!("UNRESOLVED"),
        }
    }

    /// Diagnostic: dump every OCR'd word for one fixture.
    #[test]
    #[ignore = "diagnostic — run with --ignored"]
    fn dump_ocr_words_for_first_fixture() {
        use crate::ocr::engine;
        let dir = fixture_dir();
        let path = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jpg"))
            .expect("at least one fixture")
            .path();
        let img = image::open(&path).expect("decode fixture");
        let words = engine::recognize_image(&img).expect("OCR fixture");
        eprintln!("OCR'd {} words for {}:", words.len(), path.display());
        for w in &words {
            eprintln!(
                "  {:?}  @ ({:.0},{:.0}) {}×{}",
                w.text, w.rect.x, w.rect.y, w.rect.width, w.rect.height,
            );
        }
    }

    #[test]
    fn upgrade_identification_across_jpg_fixtures() {
        let data = load_data();
        let dir = fixture_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().and_then(|s| s.to_str()) == Some("jpg")
            })
            .collect();
        assert!(
            !entries.is_empty(),
            "no fixtures found under {}",
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
                        .map(|u| {
                            u.requirements
                                .iter()
                                .map(|r| r.item_id.clone())
                                .collect()
                        })
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
        eprintln!("\nFixture pass rate: {pass} / {total}");
        // Loose threshold — these are JPEGs and anchor detection on
        // Steam's F12-style compression is less reliable than the
        // native PNGs the runtime sees. Tighten once native fixtures
        // exist alongside.
        let min_required = (total * 9 / 10).max(1); // 90%, round up
        assert!(
            pass >= min_required,
            "only {pass}/{total} fixtures passed (need ≥ {min_required}); failed: {fail:?}",
        );
    }
}
