//! CPU rasterizer for the **capture guide box** — the head-locked aiming
//! reticle shown while a [`CaptureMode`](super::capture_session::CaptureMode)
//! is active (issue #136, refined in #141).
//!
//! ## Layout: a transparent hole == the OCR crop
//! The captured rectangle (the crop) must contain **only game pixels** — no
//! overlay text, no frame border (issue #141). So the texture is laid out as a
//! transparent **hole** in the middle that corresponds exactly to the crop,
//! with everything of ours kept *outside* it:
//!
//! ```text
//!  ┌───────────────────────────────┐  ← texture (the overlay quad)
//!  │        "Aim at the …"          │  caption  (top margin, above the crop)
//!  │     "Pull RIGHT trigger …"     │  hint
//!  │  ┌─────────────────────────┐   │
//!  │  │                         │   │  ← hole == crop (transparent, captured)
//!  │  │     (see the game)      │   │     frame border drawn just OUTSIDE it
//!  │  └─────────────────────────┘   │
//!  │         [ Capturing… ]         │  status chip (bottom margin, below crop)
//!  └───────────────────────────────┘
//! ```
//!
//! The overlay glue ([`super::runtime::ensure_guide`]) sizes the *hole* to the
//! crop's metric extent (via the eye FOV — see [`super::fov`]) and scales the
//! overlay width up by `tex_w / hole_w` so the hole still lands exactly on the
//! crop. The margins are symmetric so the hole stays centered in the texture
//! and the overlay transform is just the crop center.
//!
//! Like [`super::ocr_render`] it's a pure `tiny_skia` rasterizer (no OpenVR),
//! so it's unit-tested on every target; the Windows-only overlay code just
//! submits the pixmap.

use crate::vr::capture_session::CaptureMode;
use crate::vr::text;
use tiny_skia::{
    Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};

/// Frame stroke width, in px. Thin — a gentle aiming guide, not a bold frame
/// (issue #141 part 3).
const FRAME_STROKE: f32 = 3.0;
/// Length of each corner tick, as a fraction of the shorter hole side.
const CORNER_FRAC: f32 = 0.05;
/// Caption / chip text size, as a fraction of the hole height.
const CAPTION_FRAC: f32 = 0.05;
/// Floor on text size so tiny boxes stay legible.
const MIN_TEXT_PX: f32 = 14.0;
/// Top/bottom margin height as a multiple of the caption text size — enough to
/// hold the two caption lines (top) and the status chip (bottom) outside the
/// captured hole. Applied symmetrically so the hole stays centered.
const MARGIN_Y_TEXT_MULT: f32 = 3.4;
/// Left/right margin as a fraction of the hole width — only has to clear the
/// (thin) frame stroke, since the box is centered.
const MARGIN_X_FRAC: f32 = 0.02;

fn frame_color() -> Color {
    // Subtler than the old bold frame: lower alpha so it reads as a gentle
    // guide rather than a hard border (issue #141 part 3).
    Color::from_rgba8(120, 200, 230, 150)
}
fn caption_color() -> Color {
    Color::from_rgba8(240, 240, 240, 245)
}
fn chip_text_color() -> Color {
    Color::from_rgba8(20, 20, 24, 255)
}
/// Bright off-white for the hideout count digits — reads against the game
/// behind the transparent box.
fn count_color() -> Color {
    Color::from_rgba8(245, 245, 255, 255)
}
/// Green ✓ (a tile matched a catalog item).
fn check_ok_color() -> Color {
    Color::from_rgba8(90, 220, 120, 255)
}
/// Red ✗ (a tile was detected but couldn't be read/matched).
fn check_bad_color() -> Color {
    Color::from_rgba8(235, 90, 80, 255)
}
/// Dark, semi-opaque backing drawn behind each mark so it stays legible over a
/// busy game background seen through the otherwise-transparent hole.
fn mark_backing() -> Color {
    Color::from_rgba8(16, 18, 22, 200)
}

/// A per-item feedback mark painted **over the real items** seen through the
/// guide box's hole (issue #137). Positions are normalized to the crop rect
/// (`x`/`y` are fractions in `0.0..=1.0`, sourced from #138's `NormRect`
/// centers / `GridRow`+`GridCell`); since the hole *is* the crop, the renderer
/// maps them into the hole sub-rect so the marks line up with what's on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GuideMark {
    /// Hideout: the read owned-count at a panel cell. `None` = the cell was
    /// seen but its count couldn't be read (drawn as "?").
    Count { x: f32, y: f32, count: Option<u32> },
    /// Box / stash: a tile matched a catalog item (`true` → green ✓) or was
    /// detected but couldn't be read/matched (`false` → red ✗).
    Check { x: f32, y: f32, matched: bool },
}

impl GuideMark {
    fn xy(&self) -> (f32, f32) {
        match self {
            GuideMark::Count { x, y, .. } | GuideMark::Check { x, y, .. } => (*x, *y),
        }
    }
}

/// Map a normalized crop position to a pixel center inside the texture's
/// **hole** (== the OCR crop, #137). The hole is a sub-rect of the texture —
/// the margins carry the caption / chip / frame (#141) — so a normalized crop
/// coord lands at `hole_origin + frac · hole_size`. Clamped to stay in the hole.
pub fn mark_px(x: f32, y: f32, layout: &GuideTexLayout) -> (f32, f32) {
    (
        layout.hole_x as f32 + x.clamp(0.0, 1.0) * layout.hole_w as f32,
        layout.hole_y as f32 + y.clamp(0.0, 1.0) * layout.hole_h as f32,
    )
}

/// Where the transparent hole (== the OCR crop) sits inside the guide texture,
/// plus the texture's total size. Built by [`layout_for_hole`] from the hole's
/// pixel dimensions (which the overlay glue derives from the crop's metric
/// aspect); the margins around it carry the caption / status chip / frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuideTexLayout {
    pub tex_w: u32,
    pub tex_h: u32,
    pub hole_x: u32,
    pub hole_y: u32,
    pub hole_w: u32,
    pub hole_h: u32,
}

/// Wrap a hole of `hole_w × hole_h` px in symmetric margins big enough for the
/// caption (top) and status chip (bottom). Symmetric margins keep the hole
/// centered, so the overlay transform is exactly the crop center with no extra
/// offset (see the module docs).
pub fn layout_for_hole(hole_w: u32, hole_h: u32) -> GuideTexLayout {
    let hw = hole_w.max(1);
    let hh = hole_h.max(1);
    let cap_px = (hh as f32 * CAPTION_FRAC).max(MIN_TEXT_PX);
    let margin_y = (cap_px * MARGIN_Y_TEXT_MULT).ceil().max(FRAME_STROKE + 2.0) as u32;
    let margin_x = (hw as f32 * MARGIN_X_FRAC).ceil().max(FRAME_STROKE + 2.0) as u32;
    GuideTexLayout {
        tex_w: hw + 2 * margin_x,
        tex_h: hh + 2 * margin_y,
        hole_x: margin_x,
        hole_y: margin_y,
        hole_w: hw,
        hole_h: hh,
    }
}

/// Render the guide box for `mode` into a fresh RGBA pixmap of `layout.tex_w ×
/// layout.tex_h`. The hole (`layout.hole_*`) is left transparent so the user
/// sees the game through it and the capture grabs only game pixels; the frame
/// is drawn just outside the hole, the caption above it, and the status chip
/// below it.
///
/// The bottom status chip shows `chip_label` filled with `chip_rgb` — normally
/// the [`CaptureState`](super::capture_session::CaptureState) phase, but for a
/// few seconds after a capture the caller passes the OCR result confirmation
/// instead. `trigger_label` ("LEFT" / "RIGHT", or "" to omit) hints which
/// controller trigger captures.
pub fn render(
    mode: &CaptureMode,
    chip_label: &str,
    chip_rgb: (u8, u8, u8),
    trigger_label: &str,
    marks: &[GuideMark],
    layout: &GuideTexLayout,
) -> Pixmap {
    // Pixmap::new zero-fills → fully transparent; we only paint the frame +
    // text in the margins, leaving the hole see-through.
    let mut pix =
        Pixmap::new(layout.tex_w.max(1), layout.tex_h.max(1)).expect("guide pixmap alloc");
    draw_frame(&mut pix, layout, frame_color());

    let w = pix.width() as f32;
    let cap_px = (layout.hole_h as f32 * CAPTION_FRAC).max(MIN_TEXT_PX);
    let hole_top = layout.hole_y as f32;
    let hole_bottom = (layout.hole_y + layout.hole_h) as f32;

    // Caption (what to aim at) + trigger hint — both in the TOP margin, above
    // the hole, so neither lands in the captured rectangle. Stacked bottom-up:
    // the hint sits just above the hole, the caption above the hint.
    let caption = mode.guide_caption();
    let hint = if trigger_label.is_empty() {
        String::new()
    } else {
        format!("Pull {trigger_label} trigger to capture")
    };
    let hint_px = (cap_px * 0.82).max(12.0);
    let hint_baseline = hole_top - cap_px * 0.4;
    let caption_baseline = if hint.is_empty() {
        hint_baseline
    } else {
        hint_baseline - hint_px * 1.2
    };
    if !caption.is_empty() {
        let tw = text::measure_width(caption, cap_px);
        text::draw_text(
            &mut pix,
            caption,
            (w - tw) / 2.0,
            caption_baseline,
            cap_px,
            caption_color(),
        );
    }
    if !hint.is_empty() {
        let hw = text::measure_width(&hint, hint_px);
        text::draw_text(
            &mut pix,
            &hint,
            (w - hw) / 2.0,
            hint_baseline,
            hint_px,
            caption_color(),
        );
    }

    // Per-item OCR marks (#137), painted inside the hole over the real items.
    // Drawn before the chip so the bottom-margin chip never sits under a mark.
    draw_marks(&mut pix, marks, layout);

    // Status chip — a filled pill-ish rect centered in the BOTTOM margin, below
    // the hole, colored + labelled by the caller. Below the crop so it never
    // occludes a grid tile in the capture (the bug #141 calls out).
    let label = chip_label;
    let (r, g, b) = chip_rgb;
    let lw = text::measure_width(label, cap_px);
    let chip_pad = cap_px * 0.5;
    let chip_w = (lw + chip_pad * 2.0).min(w - FRAME_STROKE * 2.0);
    let chip_h = cap_px * 1.6;
    let chip_x = (w - chip_w) / 2.0;
    let chip_y = hole_bottom + cap_px * 0.4;
    if let Some(rect) = Rect::from_xywh(chip_x, chip_y, chip_w, chip_h) {
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(r, g, b, 235));
        paint.anti_alias = true;
        pix.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Baseline so the text sits vertically centered in the chip.
    let baseline = chip_y + chip_h - (chip_h - cap_px) / 2.0 - cap_px * 0.18;
    text::draw_text(
        &mut pix,
        label,
        chip_x + chip_pad,
        baseline,
        cap_px,
        chip_text_color(),
    );

    pix
}

/// Compose a **side-by-side stereo** texture from a rendered guide `content`: a
/// double-wide pixmap with `content` in one half and the other half transparent
/// (issue #143). Submitted to an overlay with `VROverlayFlags_SideBySide_Parallel`
/// (left half → left eye, right half → right eye), this shows the box in only
/// one eye. `eye_is_left` selects which half holds the content — the capture eye
/// — so the doubled box vanishes from the other eye.
pub fn side_by_side(content: &Pixmap, eye_is_left: bool) -> Pixmap {
    let w = content.width();
    let h = content.height();
    let mut dbl = Pixmap::new(w * 2, h.max(1)).expect("side-by-side pixmap alloc");
    let dx = if eye_is_left { 0 } else { w as i32 };
    dbl.draw_pixmap(
        dx,
        0,
        content.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    dbl
}

/// Paint every per-item mark (#137) into the hole at its normalized crop
/// position. No-op when `marks` is empty (the steady state, no recent capture).
fn draw_marks(pix: &mut Pixmap, marks: &[GuideMark], layout: &GuideTexLayout) {
    if marks.is_empty() {
        return;
    }
    // Size relative to the HOLE (the crop), not the full texture, so it reads
    // consistently regardless of margin size; clamped to a legible band.
    let sz = (layout.hole_h as f32 * 0.06).clamp(22.0, 80.0);
    for mark in marks {
        let (x, y) = mark.xy();
        let (cx, cy) = mark_px(x, y, layout);
        match mark {
            GuideMark::Count { count, .. } => {
                let text = match count {
                    Some(n) => n.to_string(),
                    None => "?".to_string(),
                };
                draw_count(pix, cx, cy, sz, &text);
            }
            GuideMark::Check { matched, .. } => draw_check(pix, cx, cy, sz, *matched),
        }
    }
}

/// Hideout: a read owned-count, drawn centered at `(cx, cy)` over a dark pill so
/// the digits read against the game behind the transparent hole.
fn draw_count(pix: &mut Pixmap, cx: f32, cy: f32, sz: f32, text: &str) {
    let tw = text::measure_width(text, sz).max(sz * 0.5);
    let pad = sz * 0.35;
    let pw = tw + pad * 2.0;
    let ph = sz + pad;
    if let Some(rect) = Rect::from_xywh(cx - pw / 2.0, cy - ph / 2.0, pw, ph) {
        let mut paint = Paint::default();
        paint.set_color(mark_backing());
        paint.anti_alias = true;
        pix.fill_rect(rect, &paint, Transform::identity(), None);
    }
    // Baseline ≈ center + ~0.35·size puts the digit body visually centered.
    let baseline = cy + sz * 0.35;
    text::draw_text(pix, text, cx - tw / 2.0, baseline, sz, count_color());
}

/// Box / stash: a green ✓ (matched) or red ✗ (unread) over a dark disc. Drawn as
/// stroked vector paths rather than font glyphs so it renders crisply and
/// doesn't depend on the loaded font carrying the ✓/✗ code points.
fn draw_check(pix: &mut Pixmap, cx: f32, cy: f32, sz: f32, matched: bool) {
    let r = sz * 0.6;
    if let Some(disc) = PathBuilder::from_circle(cx, cy, r) {
        let mut bg = Paint::default();
        bg.set_color(mark_backing());
        bg.anti_alias = true;
        pix.fill_path(&disc, &bg, FillRule::Winding, Transform::identity(), None);
    }
    let mut paint = Paint::default();
    paint.set_color(if matched {
        check_ok_color()
    } else {
        check_bad_color()
    });
    paint.anti_alias = true;
    let stroke = Stroke {
        width: (sz * 0.16).max(2.0),
        line_cap: LineCap::Round,
        ..Default::default()
    };
    let mut pb = PathBuilder::new();
    if matched {
        // Checkmark: short arm down to a low-left vertex, long arm up-right.
        pb.move_to(cx - r * 0.45, cy + r * 0.02);
        pb.line_to(cx - r * 0.08, cy + r * 0.40);
        pb.line_to(cx + r * 0.50, cy - r * 0.40);
    } else {
        // Cross: two diagonals through the center.
        let d = r * 0.42;
        pb.move_to(cx - d, cy - d);
        pb.line_to(cx + d, cy + d);
        pb.move_to(cx + d, cy - d);
        pb.line_to(cx - d, cy + d);
    }
    if let Some(path) = pb.finish() {
        pix.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Stroke a thin frame border just **outside** the hole, plus subtle corner
/// ticks — a reticle look that reads as "line the panel up inside here" without
/// painting anything into the captured hole.
fn draw_frame(pix: &mut Pixmap, l: &GuideTexLayout, color: Color) {
    let x = l.hole_x as f32;
    let y = l.hole_y as f32;
    let w = l.hole_w as f32;
    let h = l.hole_h as f32;
    let s = FRAME_STROKE;

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: s,
        ..Default::default()
    };

    // Border path inflated by s/2 so the stroke's *inner* edge lands on the
    // hole boundary — the whole stroke sits outside the hole, keeping our
    // pixels out of the captured rectangle.
    if let Some(rect) = Rect::from_xywh(x - s / 2.0, y - s / 2.0, w + s, h + s) {
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            pix.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    // Corner ticks: short bars hugging the *outer* edge of each hole corner, so
    // the box is easy to find even where the thin frame washes out — still
    // entirely outside the hole.
    let tick = (w.min(h) * CORNER_FRAC).max(s * 3.0);
    let (ox0, oy0) = (x - s, y - s); // outer border bounds
    let (ox1, oy1) = (x + w + s, y + h + s);
    let bars = [
        // top-left
        (ox0, oy0, tick, s),
        (ox0, oy0, s, tick),
        // top-right
        (ox1 - tick, oy0, tick, s),
        (ox1 - s, oy0, s, tick),
        // bottom-left
        (ox0, oy1 - s, tick, s),
        (ox0, oy1 - tick, s, tick),
        // bottom-right
        (ox1 - tick, oy1 - s, tick, s),
        (ox1 - s, oy1 - tick, s, tick),
    ];
    for (bx, by, bw, bh) in bars {
        if let Some(rect) = Rect::from_xywh(bx, by, bw, bh) {
            pix.fill_rect(rect, &paint, Transform::identity(), None);
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
    fn layout_centers_hole_in_texture() {
        let l = layout_for_hole(400, 300);
        // Symmetric margins → hole centered → overlay transform == crop center.
        assert_eq!(l.hole_x * 2 + l.hole_w, l.tex_w, "x symmetric");
        assert_eq!(l.hole_y * 2 + l.hole_h, l.tex_h, "y symmetric");
        // Margins exist on every side (room for caption / chip / frame).
        assert!(l.tex_w > l.hole_w && l.tex_h > l.hole_h);
        assert!(l.hole_x > 0 && l.hole_y > 0);
    }

    #[test]
    fn renders_requested_size() {
        let layout = layout_for_hole(800, 600);
        let pix = render(
            &CaptureMode::Hideout,
            "Ready — pull trigger",
            (80, 180, 100),
            "RIGHT",
            &[],
            &layout,
        );
        assert_eq!((pix.width(), pix.height()), (layout.tex_w, layout.tex_h));
    }

    #[test]
    fn hole_interior_transparent_frame_just_outside() {
        let layout = layout_for_hole(400, 200);
        let pix = render(
            &CaptureMode::Box(ScanTarget::Stash),
            "Capturing…",
            (200, 180, 80),
            "RIGHT",
            &[],
            &layout,
        );
        let cy = layout.hole_y + layout.hole_h / 2;
        // Dead centre of the hole is see-through (this is what gets captured).
        assert_eq!(
            alpha_at(&pix, layout.hole_x + layout.hole_w / 2, cy),
            0,
            "hole interior stays transparent for see-through aiming"
        );
        // A pixel just outside the left hole edge sits on the frame → painted.
        assert!(
            alpha_at(&pix, layout.hole_x - 1, cy) > 0,
            "frame drawn just outside the hole"
        );
        // Well inside the hole near the left edge stays clear (border is outside).
        assert_eq!(
            alpha_at(&pix, layout.hole_x + 3, cy),
            0,
            "no frame pixels inside the captured hole"
        );
    }

    #[test]
    fn side_by_side_puts_content_in_capture_eye_half() {
        let layout = layout_for_hole(200, 100);
        let content = render(
            &CaptureMode::Hideout,
            "Ready",
            (80, 180, 100),
            "RIGHT",
            &[],
            &layout,
        );
        let w = content.width();
        let cy = layout.hole_y + layout.hole_h / 2;
        let fx = layout.hole_x - 1; // a painted frame pixel just outside the hole
        assert!(
            alpha_at(&content, fx, cy) > 0,
            "frame pixel painted in content"
        );

        // Right capture eye (eye_is_left = false): content in the RIGHT half.
        let r = side_by_side(&content, false);
        assert_eq!((r.width(), r.height()), (w * 2, content.height()));
        assert_eq!(alpha_at(&r, fx, cy), 0, "left half clear for right-eye box");
        assert!(alpha_at(&r, fx + w, cy) > 0, "content in right half");

        // Left capture eye: content in the LEFT half.
        let l = side_by_side(&content, true);
        assert!(alpha_at(&l, fx, cy) > 0, "content in left half");
        assert_eq!(
            alpha_at(&l, fx + w, cy),
            0,
            "right half clear for left-eye box"
        );
    }

    #[test]
    fn handles_degenerate_size() {
        // Must not panic on a 0×0 (clamped) hole.
        let layout = layout_for_hole(0, 0);
        let pix = render(
            &CaptureMode::Hideout,
            "Reading…",
            (89, 190, 175),
            "RIGHT",
            &[],
            &layout,
        );
        assert!(pix.width() >= 1 && pix.height() >= 1);
    }

    /// Snapshot: save a PNG to disk for manual inspection if `RENDER_SNAPSHOT=1`.
    /// Lets you eyeball the #141 layout — transparent hole == crop, caption
    /// above, status chip below, subtle frame outside the hole. The hole is a
    /// stash-shaped landscape rect (aspect ~1.86).
    #[test]
    fn snapshot_for_manual_review() {
        let layout = layout_for_hole(1024, 550);
        let pix = render(
            &CaptureMode::Box(ScanTarget::Stash),
            "Capturing…",
            (200, 180, 80),
            "RIGHT",
            &[],
            &layout,
        );
        if std::env::var("RENDER_SNAPSHOT").is_ok() {
            let path = std::env::temp_dir().join("ez-wishlist-overlay-guide-snapshot.png");
            pix.save_png(&path).expect("save guide snapshot");
            eprintln!("guide snapshot saved to {}", path.display());
        }
    }

    #[test]
    fn mark_px_maps_normalized_into_the_hole() {
        let l = layout_for_hole(800, 600);
        // Corners + center map through the hole origin + fraction·hole-size — so
        // marks land in the crop, not the margins.
        assert_eq!(mark_px(0.0, 0.0, &l), (l.hole_x as f32, l.hole_y as f32));
        assert_eq!(
            mark_px(1.0, 1.0, &l),
            ((l.hole_x + l.hole_w) as f32, (l.hole_y + l.hole_h) as f32)
        );
        assert_eq!(
            mark_px(0.5, 0.5, &l),
            (l.hole_x as f32 + 400.0, l.hole_y as f32 + 300.0)
        );
        // Out-of-range clamps to the hole edge (never into the margins).
        assert_eq!(
            mark_px(-1.0, 2.0, &l),
            (l.hole_x as f32, (l.hole_y + l.hole_h) as f32)
        );
    }

    /// A mark must paint pixels inside the (otherwise-transparent) hole near the
    /// spot its normalized position maps to — that's the whole point of #137.
    #[test]
    fn marks_paint_inside_the_hole() {
        let layout = layout_for_hole(400, 400);
        let marks = [
            GuideMark::Count {
                x: 0.5,
                y: 0.5,
                count: Some(7),
            },
            GuideMark::Check {
                x: 0.25,
                y: 0.5,
                matched: true,
            },
            GuideMark::Check {
                x: 0.75,
                y: 0.5,
                matched: false,
            },
        ];
        let pix = render(
            &CaptureMode::Hideout,
            "Saved",
            (80, 180, 100),
            "RIGHT",
            &marks,
            &layout,
        );
        for (nx, what) in [(0.25f32, "✓"), (0.5, "count"), (0.75, "✗")] {
            let (mx, my) = mark_px(nx, 0.5, &layout);
            assert!(
                opaque_in_window(&pix, mx as u32, my as u32, 60),
                "{what} mark should paint near its mapped hole position"
            );
        }
    }

    /// True if any pixel within `±rad` of `(cx, cy)` is non-transparent.
    fn opaque_in_window(pix: &Pixmap, cx: u32, cy: u32, rad: u32) -> bool {
        let x0 = cx.saturating_sub(rad);
        let y0 = cy.saturating_sub(rad);
        let x1 = (cx + rad).min(pix.width().saturating_sub(1));
        let y1 = (cy + rad).min(pix.height().saturating_sub(1));
        for y in y0..=y1 {
            for x in x0..=x1 {
                if alpha_at(pix, x, y) > 0 {
                    return true;
                }
            }
        }
        false
    }
}
