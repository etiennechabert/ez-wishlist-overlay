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

    // First pass: OCR the whole image to get word-level boxes for anchor
    // detection. Run on the raw RGB image — Windows.Media.Ocr handles the
    // mixed white-on-dark text natively; preprocessing helps Tesseract but
    // not WinRT.
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

    // Read the row-label rect for the canonical module name. Crop, OCR
    // again on just the small region (faster + cleaner than filtering
    // first-pass words by Y-band, since the crop concentrates pixels).
    let row_crop = img.crop_imm(
        layout.row_label.x,
        layout.row_label.y,
        layout.row_label.w,
        layout.row_label.h,
    );
    let row_words = engine::recognize_image(&row_crop).context("row-label OCR")?;
    let row_text: String = row_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    // Read the header rect for LV<digit>.
    let header_crop = img.crop_imm(
        layout.header.x,
        layout.header.y,
        layout.header.w,
        layout.header.h,
    );
    let header_words = engine::recognize_image(&header_crop).context("header OCR")?;
    let current_level = header_words
        .iter()
        .find_map(|w| anchor::parse_level_token(&w.text))
        .unwrap_or(0);

    // Strict-match the row label to an upgrade id.
    let upgrade_id = match match_upgrade::resolve(data, &row_text, current_level) {
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

    if layout.cells.len() != upgrade.requirements.len() {
        tracing::debug!(
            cells = layout.cells.len(),
            requirements = upgrade.requirements.len(),
            upgrade_id = %upgrade.id,
            "OCR pipeline: cell count != requirement count — dropping (likely partial detection)",
        );
        return Ok(None);
    }

    // Binarize the full image once; cell strips are cropped from it.
    let prepped = prep::keep_white_invert(&img);
    let templates = &*templates::EMBEDDED;
    let mut items: Vec<(String, u32)> = Vec::with_capacity(upgrade.requirements.len());
    for (cell, req) in layout.cells.iter().zip(upgrade.requirements.iter()) {
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
