//! CPU rasterizer for the **capture guide box** — the head-locked aiming
//! reticle shown while a [`CaptureMode`](super::capture_session::CaptureMode)
//! is active (issue #136).
//!
//! It draws a mostly-transparent frame the user lines the panel/container up
//! inside, a caption telling them what to aim at, and a status chip showing
//! the current [`CaptureState`]. The frame's pixel surface is defined to
//! correspond to the OCR crop rectangle, so a later PR can paint per-item
//! markers on it at normalized crop coordinates (#137).
//!
//! Like [`super::ocr_render`] it's a pure `tiny_skia` rasterizer (no OpenVR),
//! so it's unit-tested on every target; the Windows-only overlay code just
//! submits the pixmap.

use crate::vr::capture_session::{CaptureMode, CaptureState};
use crate::vr::text;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Frame stroke width, in px.
const FRAME_STROKE: f32 = 6.0;
/// Length of each corner tick, as a fraction of the shorter side.
const CORNER_FRAC: f32 = 0.06;
/// Caption / chip text size, as a fraction of the canvas height.
const CAPTION_FRAC: f32 = 0.045;

fn frame_color() -> Color {
    Color::from_rgba8(120, 200, 230, 230)
}
fn caption_color() -> Color {
    Color::from_rgba8(240, 240, 240, 245)
}
fn chip_text_color() -> Color {
    Color::from_rgba8(20, 20, 24, 255)
}

/// Render the guide box for `mode` + `state` to a fresh RGBA pixmap of
/// `px_w × px_h` (the caller sizes this to the crop rect's pixel aspect). The
/// interior is left transparent so the user sees the game through it.
pub fn render(mode: &CaptureMode, state: CaptureState, px_w: u32, px_h: u32) -> Pixmap {
    // Pixmap::new zero-fills → fully transparent; we only paint the frame +
    // text, leaving the centre see-through.
    let mut pix = Pixmap::new(px_w.max(1), px_h.max(1)).expect("guide pixmap alloc");
    draw_frame(&mut pix, frame_color());

    let w = pix.width() as f32;
    let h = pix.height() as f32;
    let text_px = (h * CAPTION_FRAC).max(14.0);

    // Caption (what to aim at) — centered just inside the top edge.
    let caption = mode.guide_caption();
    if !caption.is_empty() {
        let tw = text::measure_width(caption, text_px);
        text::draw_text(
            &mut pix,
            caption,
            (w - tw) / 2.0,
            FRAME_STROKE + text_px * 1.2,
            text_px,
            caption_color(),
        );
    }

    // Status chip — a filled pill-ish rect centered just inside the bottom
    // edge, colored by the capture phase, with the phase label on it.
    let label = state.label();
    let (r, g, b) = state.rgb();
    let lw = text::measure_width(label, text_px);
    let chip_pad = text_px * 0.5;
    let chip_w = (lw + chip_pad * 2.0).min(w - FRAME_STROKE * 2.0);
    let chip_h = text_px * 1.6;
    let chip_x = (w - chip_w) / 2.0;
    let chip_y = h - FRAME_STROKE - chip_h - text_px * 0.4;
    if let Some(rect) = Rect::from_xywh(chip_x, chip_y, chip_w, chip_h) {
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(r, g, b, 235));
        paint.anti_alias = true;
        pix.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Baseline so the text sits vertically centered in the chip.
    let baseline = chip_y + chip_h - (chip_h - text_px) / 2.0 - text_px * 0.18;
    text::draw_text(
        &mut pix,
        label,
        chip_x + chip_pad,
        baseline,
        text_px,
        chip_text_color(),
    );

    pix
}

/// Stroke the frame border + four corner ticks (a reticle look that reads as
/// "line the panel up inside here").
fn draw_frame(pix: &mut Pixmap, color: Color) {
    let w = pix.width() as f32;
    let h = pix.height() as f32;
    let inset = FRAME_STROKE / 2.0;

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: FRAME_STROKE,
        ..Default::default()
    };

    if let Some(rect) = Rect::from_xywh(inset, inset, w - FRAME_STROKE, h - FRAME_STROKE) {
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            pix.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    // Brighter, thicker corner ticks so the box's extent is unmistakable even
    // if the thin frame washes out against a busy background.
    let tick = (w.min(h) * CORNER_FRAC).max(FRAME_STROKE * 2.0);
    let t = FRAME_STROKE; // tick thickness
    let mut tick_paint = Paint::default();
    tick_paint.set_color(color);
    let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)];
    for (cx, cy) in corners {
        let sx = if cx == 0.0 { 0.0 } else { w - tick };
        let sy_h = if cy == 0.0 { 0.0 } else { h - t };
        let sy_v = if cy == 0.0 { 0.0 } else { h - tick };
        let sx_v = if cx == 0.0 { 0.0 } else { w - t };
        if let Some(r) = Rect::from_xywh(sx, sy_h, tick, t) {
            pix.fill_rect(r, &tick_paint, Transform::identity(), None);
        }
        if let Some(r) = Rect::from_xywh(sx_v, sy_v, t, tick) {
            pix.fill_rect(r, &tick_paint, Transform::identity(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::ScanTarget;

    fn alpha_at(pix: &Pixmap, x: u32, y: u32) -> u8 {
        let i = ((y * pix.width() + x) * 4 + 3) as usize;
        pix.data()[i]
    }

    #[test]
    fn renders_requested_size() {
        let pix = render(&CaptureMode::Hideout, CaptureState::Ready, 800, 600);
        assert_eq!((pix.width(), pix.height()), (800, 600));
    }

    #[test]
    fn frame_painted_centre_transparent() {
        let pix = render(
            &CaptureMode::Box(ScanTarget::Stash),
            CaptureState::Capturing,
            400,
            300,
        );
        // A corner pixel sits on the frame/tick → opaque-ish.
        assert!(alpha_at(&pix, 1, 1) > 0, "frame drawn at corner");
        // Dead centre is inside the box, away from caption/chip → see-through.
        assert_eq!(
            alpha_at(&pix, 200, 150),
            0,
            "interior stays transparent for see-through aiming"
        );
    }

    #[test]
    fn handles_degenerate_size() {
        // Must not panic on a 1x1 (clamped) request.
        let pix = render(&CaptureMode::Hideout, CaptureState::RunningOcr, 0, 0);
        assert!(pix.width() >= 1 && pix.height() >= 1);
    }
}
