//! Minimal CPU text rasterizer for the VR overlay.
//!
//! Loads a system TTF once via [`fontdue`] and composites grayscale glyph
//! bitmaps directly onto a [`tiny_skia::Pixmap`]. No layout engine — just
//! left-to-right placement of single-line text, which is all the overlay
//! currently needs (cell-level "X/Y" progress strings + future "+N more"
//! / placeholder copy).
//!
//! Font lookup is best-effort: on Windows we expect Consolas / Segoe UI
//! to be present (this is what production ships on); on Linux/macOS we
//! try a couple of common open-source fonts so the CI test runners don't
//! crash if they happen to invoke a code path that draws text. If nothing
//! can be loaded, [`draw_text`] silently no-ops — the rest of the cell
//! (icon + progress bar) still renders, just without the numeric overlay.

use fontdue::{Font, FontSettings};
use std::sync::OnceLock;
use tiny_skia::{Color, Pixmap};

/// Candidate paths checked in order. First successful TTF load wins.
/// `.ttc` (TrueType Collection) files are intentionally skipped because
/// fontdue's `from_bytes` doesn't handle the multi-face wrapper format.
const FONT_CANDIDATES: &[&str] = &[
    // Windows — Consolas is monospace (digits align cleanly in the
    // progress chip) and ships on every modern Win10/11 install.
    "C:/Windows/Fonts/consola.ttf",
    "C:/Windows/Fonts/segoeui.ttf",
    "C:/Windows/Fonts/arial.ttf",
    // Linux (Debian/Ubuntu paths). Useful so `cargo test` on CI doesn't
    // blow up if a future test starts exercising text rendering.
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

fn font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        for path in FONT_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if let Ok(f) = Font::from_bytes(bytes, FontSettings::default()) {
                tracing::debug!(font = path, "loaded VR overlay font");
                return Some(f);
            }
        }
        tracing::warn!(
            "no usable font found among {:?}; VR overlay text rendering disabled",
            FONT_CANDIDATES
        );
        None
    })
    .as_ref()
}

/// Width in pixels that `text` would occupy when rendered at `size_px`.
/// Used by callers that want to right-align or center text. Returns 0
/// when no font is loaded (matching [`draw_text`]'s silent no-op).
pub fn measure_width(text: &str, size_px: f32) -> f32 {
    let Some(font) = font() else { return 0.0 };
    text.chars()
        .map(|ch| font.metrics(ch, size_px).advance_width)
        .sum()
}

/// Draw `text` onto `pixmap` with its baseline at (`x`, `baseline_y`).
/// `+y` is down (standard pixmap coordinates). Glyph coverage from
/// fontdue is treated as alpha and source-over-blended into the
/// destination's premultiplied RGBA buffer. No-op when no font loaded.
pub fn draw_text(
    pixmap: &mut Pixmap,
    text: &str,
    x: f32,
    baseline_y: f32,
    size_px: f32,
    color: Color,
) {
    let Some(font) = font() else { return };

    let dst_w = pixmap.width() as i32;
    let dst_h = pixmap.height() as i32;
    let stride = pixmap.width() as usize * 4;
    let r = (color.red() * 255.0) as u16;
    let g = (color.green() * 255.0) as u16;
    let b = (color.blue() * 255.0) as u16;
    let color_a = (color.alpha() * 255.0) as u16;

    let mut pen_x = x;
    let pixels = pixmap.data_mut();
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        if metrics.width == 0 || metrics.height == 0 {
            // Whitespace or unmappable glyph — still advance the pen.
            pen_x += metrics.advance_width;
            continue;
        }
        // fontdue: glyph bitmap origin is top-left. `ymin` is the offset
        // from the baseline to the BOTTOM of the bitmap (negative for
        // glyphs that sit on or above the baseline, which is the case for
        // all our digits + "/"). So the bitmap's top row lands at:
        //   baseline_y - ymin - height.
        let gx = (pen_x + metrics.xmin as f32) as i32;
        let gy = (baseline_y - metrics.ymin as f32 - metrics.height as f32) as i32;
        let bw = metrics.width as i32;
        let bh = metrics.height as i32;

        for row in 0..bh {
            let dy = gy + row;
            if dy < 0 || dy >= dst_h {
                continue;
            }
            for col in 0..bw {
                let dx = gx + col;
                if dx < 0 || dx >= dst_w {
                    continue;
                }
                let glyph_a = bitmap[(row * bw + col) as usize] as u16;
                if glyph_a == 0 {
                    continue;
                }
                // Final source alpha after the user-provided color tint.
                let sa = (glyph_a * color_a) / 255;
                let inv_sa = 255 - sa;
                let idx = dy as usize * stride + dx as usize * 4;
                // Pixmap stores premultiplied alpha. Source channels get
                // pre-multiplied by `sa` on write; destination's already
                // premul'd contribution is scaled by (1 - sa).
                let sr = (r * sa) / 255;
                let sg = (g * sa) / 255;
                let sb = (b * sa) / 255;
                pixels[idx] = (sr + (pixels[idx] as u16 * inv_sa) / 255).min(255) as u8;
                pixels[idx + 1] = (sg + (pixels[idx + 1] as u16 * inv_sa) / 255).min(255) as u8;
                pixels[idx + 2] = (sb + (pixels[idx + 2] as u16 * inv_sa) / 255).min(255) as u8;
                pixels[idx + 3] = (sa + (pixels[idx + 3] as u16 * inv_sa) / 255).min(255) as u8;
            }
        }
        pen_x += metrics.advance_width;
    }
}
