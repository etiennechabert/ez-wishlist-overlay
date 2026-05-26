//! Image preprocessing for the OCR second pass.
//!
//! Windows.Media.Ocr is robust on big, high-contrast text but
//! consistently misses the in-game cell-progress digits (e.g. "1/3")
//! on its first pass over the full screenshot — they're small,
//! anti-aliased, and overlap a colored progress-bar fill. This module
//! prepares per-cell crops in a form the engine handles much better:
//!
//!   1. Crop to a known sub-region (one progress strip),
//!   2. Upscale (Lanczos3) so the glyphs become several times their
//!      original pixel size,
//!   3. Convert to grayscale + apply a luma threshold so the white
//!      digits become solid black on solid white, dropping the
//!      progress-bar fill color out entirely.
//!
//! Step 3 is the part that the full-image pass can't do (a single
//! global threshold ruins the high-contrast item names elsewhere).
//! Per-cell preprocessing lets us tune for each tiny region.

use anyhow::{Context, Result};
use image::{imageops, DynamicImage, GenericImageView, GrayImage, ImageBuffer, Luma};

/// Crop + upscale + binarize. Used by the second-pass OCR on a tight
/// progress-bar strip — the combination of (small ROI, 4× Lanczos
/// upscale, high-contrast threshold) is what makes the in-game cell
/// digits readable by Windows OCR. Earlier attempts:
/// - **Grayscale only** (no threshold): OCR returned empty/garbage for
///   most cells. The bar's colored fill confused it.
/// - **Whole-cell crop, binarized**: OCR returned text but missed the
///   small digits in the noise.
/// - **Tight strip, binarized**: works — OCR returns the digit run
///   (sometimes mis-reading the slash as a "1", which the post-parser
///   recovers heuristically).
///
/// The crop rect is clamped to the image bounds, so callers can pass
/// generous regions without worrying about off-by-one against the
/// source dimensions.
pub fn crop_upscale(
    img: &DynamicImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    scale: u32,
) -> Result<DynamicImage> {
    let (img_w, img_h) = img.dimensions();
    if x >= img_w || y >= img_h {
        anyhow::bail!(
            "crop origin ({x},{y}) is outside image bounds ({img_w}x{img_h})"
        );
    }
    let w = w.min(img_w - x);
    let h = h.min(img_h - y);
    if w == 0 || h == 0 {
        anyhow::bail!("crop width/height clamped to zero");
    }
    let scale = scale.max(1);

    let cropped = img.crop_imm(x, y, w, h);
    let new_w = w * scale;
    let new_h = h * scale;
    let upscaled = imageops::resize(&cropped, new_w, new_h, imageops::FilterType::Lanczos3);

    // Threshold against a high-luma percentile rather than the median.
    // The progress digits are near-white (~240+); the bar fill and
    // chrome are mid-gray. Using the 80th-percentile pivot keeps only
    // the brightest pixels (the digits) — much cleaner than median,
    // which let the bar fill bleed through. We then INVERT so the
    // digits are black-on-white (Windows OCR is trained on dark text
    // on light backgrounds — white-on-black, like the in-game UI,
    // its text-detector mostly ignores).
    //
    // We also pad the output with a fat white border. OCR's first
    // stage looks for text *regions* and a small digit fragment
    // sitting alone in a huge solid field gets passed over; padding
    // gives the digits a normal-document-like surround.
    let gray = DynamicImage::ImageRgba8(upscaled).to_luma8();
    let pivot = high_luma_pivot(&gray);

    let pad = (new_h / 4).max(20);
    let padded_w = new_w + 2 * pad;
    let padded_h = new_h + 2 * pad;
    let padded: GrayImage = ImageBuffer::from_fn(padded_w, padded_h, |px, py| {
        // Inside the original area: invert binarize (digit→black, bg→white).
        // Outside: pure white padding.
        if px < pad || py < pad || px >= pad + new_w || py >= pad + new_h {
            return Luma([255]);
        }
        let v = gray.get_pixel(px - pad, py - pad).0[0];
        if v >= pivot {
            Luma([0]) // was bright (digit) → now black
        } else {
            Luma([255]) // was dark (bg) → now white
        }
    });

    Ok(DynamicImage::ImageLuma8(padded))
}

/// 80th-percentile luma — the cutoff above which we have the
/// brightest 20% of pixels. For a progress-bar strip those are
/// almost always the digits and the bar's white border, with the
/// colored fill + dark backing dropping cleanly to black.
fn high_luma_pivot(g: &GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for p in g.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u32 = hist.iter().sum();
    let cut = (total as f32 * 0.80) as u32;
    let mut acc = 0u32;
    for (v, count) in hist.iter().enumerate() {
        acc += count;
        if acc >= cut {
            return v as u8;
        }
    }
    200
}

/// Helper for the extractor: given a cell's expected pixel rect, save a
/// preprocessed copy of it to disk as a sibling file named
/// `<basename>.cell-N.png`. Useful for debugging which crops the OCR
/// is actually seeing. No-op when the path has no parent / non-utf8
/// chars — debug aid, not user-facing.
#[cfg(debug_assertions)]
#[allow(dead_code)]
pub fn debug_save_crop(img: &DynamicImage, source: &std::path::Path, cell_idx: usize) {
    let Some(parent) = source.parent() else { return };
    let Some(stem) = source.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let out = parent.join(format!("{stem}.cell-{cell_idx}.png"));
    if let Err(e) = img.save(&out) {
        tracing::debug!(error = %e, "debug crop save failed");
    }
}
