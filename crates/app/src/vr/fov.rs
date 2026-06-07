//! Eye-FOV → head-locked guide-box geometry (issue #141).
//!
//! The capture guide box must **exactly outline its own OCR crop** so that
//! *what you frame == what's captured == what's OCR'd*. The mirror frame maps
//! linearly to the eye's tangent space, so the crop rect's edges map to
//! frustum tangents, and tangents × distance give head-locked metric extents.
//!
//! This module is the pure, cross-platform heart of that mapping: it takes the
//! eye's projection tangents ([`EyeFov`], from `IVRSystem::GetProjectionRaw`)
//! plus a normalized crop and returns a metric [`GuidePlacement`]. Keeping the
//! math here (rather than in the Windows-only OpenVR glue) means the box↔crop
//! correspondence is unit-tested on every target; only the `projection_raw`
//! query + overlay transform live behind `cfg(windows)`.
//!
//! It replaces the old flat `GUIDE_WIDTH_PER_CROP_W = 1.25` constant, which
//! ignored the headset's real per-eye FOV (horizontal tangent span ≈ 2.0, not
//! 1.25) and so rendered the box at ~60 % of the size needed to outline its
//! crop.

use crate::settings::CaptureCrop;

/// Tangents of the eye frustum's four half-angles, exactly as
/// [`openvr`]'s `IVRSystem::GetProjectionRaw` reports them: signed tangents of
/// the half-angle from the center view axis to each clipping plane. The full
/// mirror frame spans these horizontally and vertically.
///
/// **The sign convention of `top`/`bottom` (and occasionally `left`/`right`)
/// varies by runtime** — this headset reports `top`/`bottom` inverted relative
/// to the image (the more-negative `top` is actually the *downward* plane). So
/// nothing here trusts the field names to say which edge is which: the geometry
/// derives the frame edges and the gaze from the **min/max** of the tangents
/// (see [`gaze_fraction`]), and only ever uses the magnitudes via [`span_x`] /
/// [`span_y`] for sizing.
///
/// [`span_x`]: EyeFov::span_x
/// [`span_y`]: EyeFov::span_y
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeFov {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl EyeFov {
    /// A symmetric ~90°-ish fallback used when `projection_raw` can't be
    /// queried (no `IVRSystem` this tick). Horizontal tangent span 2.0,
    /// vertical 2.2 — close enough to a real headset that the box is still a
    /// usable aiming guide until the real FOV is read next frame.
    pub const FALLBACK: EyeFov = EyeFov {
        left: -1.0,
        right: 1.0,
        top: -1.1,
        bottom: 1.1,
    };

    /// Horizontal tangent span of the frame (always ≥ 0).
    pub fn span_x(&self) -> f32 {
        (self.right - self.left).abs()
    }

    /// Vertical tangent span of the frame (always ≥ 0).
    pub fn span_y(&self) -> f32 {
        (self.bottom - self.top).abs()
    }
}

/// Head-locked metric placement for the capture guide box's **crop region**
/// (the transparent hole), in the HMD's local frame (OpenVR: +X right, +Y up,
/// −Z forward). `width_m`/`height_m` are the metric size of the crop region at
/// the box plane; `center_*_m` is where that region's middle sits relative to
/// the gaze; `distance_m` is how far in front it floats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuidePlacement {
    pub width_m: f32,
    pub height_m: f32,
    pub center_x_m: f32,
    pub center_y_m: f32,
    pub distance_m: f32,
}

impl GuidePlacement {
    /// Aspect ratio (width / height) of the crop region. The guide texture's
    /// hole must be rendered at this pixel aspect so it maps onto the metric
    /// quad without stretch distortion.
    pub fn aspect(&self) -> f32 {
        if self.height_m.abs() < f32::EPSILON {
            1.0
        } else {
            self.width_m / self.height_m
        }
    }
}

/// Metric size + placement for the head-locked guide box, from the crop and the
/// eye's FOV spans. Core of issue #141 — see the module docs.
///
/// The crop is sanitized first (clamped to a sane sub-rect), so callers can pass
/// a raw per-mode crop directly.
///
/// ## Straight-ahead box placement
/// The center is the crop's offset **from the frame center**, scaled by the FOV
/// span. All our per-mode crops are frame-centered, so the box lands at
/// `(0, 0, −D)` — straight ahead. That's deliberate: the box is the aiming
/// reticle the user sees *binocularly*, so it belongs on the gaze. (The first
/// cut of #141 instead placed it from the eye's *absolute* projection tangents,
/// baking in the frustum asymmetry `mid_tan ≠ 0` — in-headset that floated the
/// box well off the gaze, forcing the user to look down/aside to aim.)
///
/// The trickier part — making the *captured rectangle* line up with this
/// straight-ahead box in the chosen capture eye's asymmetric mirror — is handled
/// separately by [`gaze_centered_crop`], which positions the **crop**, not the
/// box. The size here tracks the real per-eye spans (the part of #141 that was
/// right from the start).
pub fn guide_placement(fov: EyeFov, crop: &CaptureCrop, distance_m: f32) -> GuidePlacement {
    let mut c = *crop;
    c.sanitize();
    let d = distance_m.max(0.01);

    let span_x = fov.span_x();
    let span_y = fov.span_y();

    // Crop center as a fractional offset from the frame center (gaze axis).
    let off_u = (c.x + c.w * 0.5) - 0.5;
    let off_v = (c.y + c.h * 0.5) - 0.5;

    GuidePlacement {
        width_m: c.w * span_x * d,
        height_m: c.h * span_y * d,
        center_x_m: off_u * span_x * d,
        // Frame +v is down; the HMD frame is Y-up, so negate: a crop above the
        // frame center places the box up, below the center places it down.
        center_y_m: -off_v * span_y * d,
        distance_m: d,
    }
}

/// Frame fraction `(u, v)` where the eye's forward (gaze) axis — tangent
/// `(0,0)` — lands in the upright mirror image. For a symmetric FOV this is the
/// frame center `(0.5, 0.5)`; for an asymmetric one it shifts (a real right-eye
/// capture put it at `(0.38, 0.40)` — left of and *above* center, because the
/// eye has more outward + downward FOV).
///
/// **Robust to OpenVR's per-runtime sign convention.** `GetProjectionRaw`'s
/// `top`/`bottom` (and occasionally `left`/`right`) signs vary by runtime — this
/// headset reports `top`/`bottom` *inverted* relative to the image (`top` is the
/// downward plane). Since the captured mirror is upright, we don't trust the
/// field names: the **more-negative** tangent is the frame's left/bottom edge
/// and the **more-positive** is the right/top edge. The gaze sits between them
/// in proportion to those magnitudes (issue #141 in-headset finding).
pub fn gaze_fraction(fov: EyeFov) -> (f32, f32) {
    let span_x = fov.span_x();
    let span_y = fov.span_y();
    let left_edge = fov.left.min(fov.right); // world-left  (most negative)
    let up_edge = fov.top.max(fov.bottom); // world-up    (most positive)
    let u = if span_x > f32::EPSILON {
        -left_edge / span_x
    } else {
        0.5
    };
    let v = if span_y > f32::EPSILON {
        up_edge / span_y
    } else {
        0.5
    };
    (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))
}

/// Frame fraction `(u, v)` where the **head-locked straight-ahead guide box**
/// (head-space `(0, 0, −distance)`) appears in the *capture eye's* mirror.
///
/// This is [`gaze_fraction`] shifted by the eye's **parallax**: the capture eye
/// sits `(eye_dx, eye_dy)` off the head origin (≈ ±IPD/2 horizontally), so a
/// point straight ahead of the *head* is offset in that *eye's* view. Centering
/// the crop here, rather than on the bare gaze, makes the captured rect line up
/// with the box's left/right edges instead of clipping one side and showing the
/// other's border (issue #141 in-headset finding). `eye_dx`/`eye_dy` come from
/// `IVRSystem::GetEyeToHeadTransform`.
pub fn box_apparent_fraction(fov: EyeFov, eye_dx: f32, eye_dy: f32, distance: f32) -> (f32, f32) {
    let (gu, gv) = gaze_fraction(fov);
    let span_x = fov.span_x();
    let span_y = fov.span_y();
    let d = distance.max(0.01);
    // A point straight ahead of the head, seen from an eye offset +dx to the
    // right, appears shifted left in that eye → smaller u.
    let u = if span_x > f32::EPSILON {
        gu - eye_dx / (d * span_x)
    } else {
        gu
    };
    // Frame v increases downward; an eye offset +dy up sees the point lower → +v.
    let v = if span_y > f32::EPSILON {
        gv + eye_dy / (d * span_y)
    } else {
        gv
    };
    (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))
}

/// Re-center a per-mode crop on where the straight-ahead guide box **appears in
/// the capture eye** (issue #141 in-headset fix).
///
/// The compositor mirror is the raw, **asymmetric** eye render, so the gaze does
/// *not* sit at the frame center (see [`gaze_fraction`]), and the capture eye's
/// **parallax** shifts the box off the gaze too (see [`box_apparent_fraction`]).
/// The guide box is drawn straight ahead, so it visually outlines the region
/// around that apparent point; a crop taken from the frame center (or even the
/// bare gaze) would capture off to the side of what the user framed. This shifts
/// the crop so its center lands on the box's apparent fraction (plus any
/// intentional offset the base crop has from the frame center), keeping the
/// crop's size — so `gaze_centered_crop` == what the straight-ahead box
/// outlines. A symmetric FOV with no eye offset leaves a centered crop unchanged.
pub fn gaze_centered_crop(
    fov: EyeFov,
    eye_dx: f32,
    eye_dy: f32,
    distance: f32,
    base: &CaptureCrop,
) -> CaptureCrop {
    let (cu, cv) = box_apparent_fraction(fov, eye_dx, eye_dy, distance);
    // Preserve any intentional offset the base crop has from the frame center,
    // but applied relative to the box's apparent center instead.
    let off_u = (base.x + base.w * 0.5) - 0.5;
    let off_v = (base.y + base.h * 0.5) - 0.5;
    let mut c = CaptureCrop {
        x: cu + off_u - base.w * 0.5,
        y: cv + off_v - base.h * 0.5,
        w: base.w,
        h: base.h,
    };
    c.sanitize();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(x: f32, y: f32, w: f32, h: f32) -> CaptureCrop {
        CaptureCrop { x, y, w, h }
    }

    /// Symmetric square FOV: the full frame maps to a box of span×D on each
    /// axis, centered on the gaze.
    #[test]
    fn full_frame_symmetric_is_centered() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let p = guide_placement(fov, &crop(0.0, 0.0, 1.0, 1.0), 1.0);
        assert!((p.width_m - 2.0).abs() < 1e-5);
        assert!((p.height_m - 2.0).abs() < 1e-5);
        assert!(p.center_x_m.abs() < 1e-5);
        assert!(p.center_y_m.abs() < 1e-5);
        assert!((p.distance_m - 1.0).abs() < 1e-5);
    }

    /// The exact bug #141 fixes: a crop's box width is `crop_w × span_x × D`.
    /// With span_x = 2.0 that's `crop_w × 2.0`, not the old `crop_w × 1.25`.
    #[test]
    fn width_uses_horizontal_tangent_span_not_flat_constant() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let p = guide_placement(fov, &crop(0.24, 0.18, 0.52, 0.64), 1.0);
        assert!((p.width_m - 0.52 * 2.0).abs() < 1e-5);
        // The old flat constant would have produced 0.52 × 1.25 = 0.65 — i.e.
        // ~60 % of the correct 1.04 m, the documented undersize.
        assert!(p.width_m > 0.52 * 1.25 + 0.1);
    }

    /// Non-square FOV: height tracks the *vertical* tangent span, independent
    /// of the horizontal one.
    #[test]
    fn height_uses_vertical_tangent_span() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0, // span_x 2.0
            top: -1.5,
            bottom: 1.5, // span_y 3.0
        };
        let p = guide_placement(fov, &crop(0.0, 0.0, 1.0, 1.0), 1.0);
        assert!((p.width_m - 2.0).abs() < 1e-5);
        assert!((p.height_m - 3.0).abs() < 1e-5);
    }

    /// A crop in the top half of the frame places the box up (+Y); the bottom
    /// half places it down (−Y).
    #[test]
    fn vertical_crop_position_maps_to_world_up() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let top = guide_placement(fov, &crop(0.0, 0.0, 1.0, 0.5), 1.0);
        let bottom = guide_placement(fov, &crop(0.0, 0.5, 1.0, 0.5), 1.0);
        assert!(top.center_y_m > 0.4, "top-half crop → box up: {top:?}");
        assert!(
            bottom.center_y_m < -0.4,
            "bottom-half crop → box down: {bottom:?}"
        );
    }

    /// A crop in the left/right half places the box left/right (±X).
    #[test]
    fn horizontal_crop_position_maps_to_x() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let left = guide_placement(fov, &crop(0.0, 0.0, 0.5, 1.0), 1.0);
        let right = guide_placement(fov, &crop(0.5, 0.0, 0.5, 1.0), 1.0);
        assert!(left.center_x_m < -0.4, "left crop → box left: {left:?}");
        assert!(right.center_x_m > 0.4, "right crop → box right: {right:?}");
    }

    /// The compositor mirror is treated as a gaze-centered, symmetric view, so a
    /// frame-centered crop maps to the gaze `(0,0)` even when the eye's frustum
    /// is asymmetric — placing the box from the raw asymmetric tangents floated
    /// it off-gaze in-headset (issue #141 follow-up). Size still tracks the
    /// real per-eye spans.
    #[test]
    fn asymmetric_eye_still_centers_a_centered_crop() {
        let fov = EyeFov {
            left: -1.39,
            right: 1.24,
            top: -1.5,
            bottom: 1.46,
        };
        // A crop centered in the frame (center fraction 0.5, 0.5).
        let p = guide_placement(fov, &crop(0.1, 0.1, 0.8, 0.8), 1.0);
        assert!(
            p.center_x_m.abs() < 1e-5,
            "centered crop → centered box X: {p:?}"
        );
        assert!(
            p.center_y_m.abs() < 1e-5,
            "centered crop → centered box Y: {p:?}"
        );
        // Size still tracks the (asymmetric) per-eye spans.
        assert!((p.width_m - 0.8 * (1.24 + 1.39)).abs() < 1e-4, "{p:?}");
        assert!((p.height_m - 0.8 * (1.46 + 1.5)).abs() < 1e-4, "{p:?}");
    }

    /// Distance scales every metric extent linearly.
    #[test]
    fn distance_scales_linearly() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let near = guide_placement(fov, &crop(0.25, 0.25, 0.5, 0.5), 1.0);
        let far = guide_placement(fov, &crop(0.25, 0.25, 0.5, 0.5), 2.0);
        assert!((far.width_m - 2.0 * near.width_m).abs() < 1e-5);
        assert!((far.height_m - 2.0 * near.height_m).abs() < 1e-5);
    }

    /// `aspect()` = the crop region's metric width:height.
    #[test]
    fn aspect_reports_metric_ratio() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        // Square FOV, crop w=0.5 h=0.25 → metric 1.0 × 0.5 → aspect 2.0.
        let p = guide_placement(fov, &crop(0.25, 0.375, 0.5, 0.25), 1.0);
        assert!((p.aspect() - 2.0).abs() < 1e-4, "{p:?}");
    }

    /// A degenerate crop is sanitized, never producing NaN/zero-size.
    #[test]
    fn degenerate_crop_is_sanitized() {
        let fov = EyeFov::FALLBACK;
        let p = guide_placement(fov, &crop(2.0, -1.0, 5.0, 0.0), 1.0);
        assert!(p.width_m.is_finite() && p.width_m > 0.0);
        assert!(p.height_m.is_finite() && p.height_m > 0.0);
        assert!(p.aspect().is_finite() && p.aspect() > 0.0);
    }

    /// Span helpers are sign-robust.
    #[test]
    fn spans_are_absolute() {
        let normal = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let flipped = EyeFov {
            left: 1.0,
            right: -1.0,
            top: 1.0,
            bottom: -1.0,
        };
        assert!((normal.span_x() - flipped.span_x()).abs() < 1e-6);
        assert!((normal.span_y() - flipped.span_y()).abs() < 1e-6);
        assert!(normal.span_x() > 0.0 && normal.span_y() > 0.0);
    }

    /// Symmetric FOV: the gaze is at the frame center, so a centered crop is
    /// unchanged by gaze-centering.
    #[test]
    fn gaze_centered_crop_symmetric_is_unchanged() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let base = crop(0.34, 0.30, 0.32, 0.40); // centered at (0.5, 0.5)
        let g = gaze_centered_crop(fov, 0.0, 0.0, 1.0, &base);
        assert!(
            (g.x - base.x).abs() < 1e-5 && (g.y - base.y).abs() < 1e-5,
            "{g:?}"
        );
        assert!(
            (g.w - base.w).abs() < 1e-5 && (g.h - base.h).abs() < 1e-5,
            "{g:?}"
        );
    }

    /// The actual right-eye tangents logged in-headset (issue #141): the gaze
    /// sits left of and *above* the frame center (more outward + downward FOV),
    /// even though the runtime reports `top` more-negative than `bottom`. The
    /// min/max logic must recover `(0.38, 0.40)`, not the naive `−top/span`
    /// `(0.38, 0.60)` that put the crop too low.
    #[test]
    fn gaze_fraction_handles_inverted_top_bottom() {
        let fov = EyeFov {
            left: -0.8391,
            right: 1.3764,
            top: -1.4281,
            bottom: 0.9657,
        };
        let (u, v) = gaze_fraction(fov);
        assert!((u - 0.379).abs() < 0.005, "gaze u={u}");
        assert!(
            (v - 0.403).abs() < 0.005,
            "gaze v={v} (must be UP of center)"
        );
    }

    /// IPD parallax: the right eye (offset +0.032 m) sees the straight-ahead
    /// box shifted left, so the crop must center slightly left of the bare gaze.
    #[test]
    fn box_apparent_fraction_shifts_left_for_right_eye() {
        let fov = EyeFov {
            left: -0.8391,
            right: 1.3764,
            top: -1.4281,
            bottom: 0.9657,
        };
        let (gu, _) = gaze_fraction(fov);
        let (u, _) = box_apparent_fraction(fov, 0.032, 0.0, 1.0);
        assert!(u < gu, "right-eye parallax shifts left: {u} < {gu}");
        // shift ≈ 0.032 / (1.0 * 2.2155) = 0.0144.
        assert!((u - (gu - 0.0144)).abs() < 0.001, "u={u}");
        // No offset → no shift.
        let (u0, v0) = box_apparent_fraction(fov, 0.0, 0.0, 1.0);
        assert!((u0 - gu).abs() < 1e-6 && (v0 - gaze_fraction(fov).1).abs() < 1e-6);
    }

    /// `capture_eye` is a user setting (default Right, but Left is allowed), so
    /// the geometry must work for the LEFT eye too — as the mirror image of the
    /// right. Left-eye FOV is the right-eye's horizontal mirror, and its IPD
    /// offset is negative, so the gaze sits right of center and the parallax
    /// shifts the crop right. Everything must mirror about u=0.5 (issue #141).
    #[test]
    fn left_eye_is_mirror_of_right() {
        // Right-eye tangents (from the in-headset capture) + their left mirror.
        let right = EyeFov {
            left: -0.8391,
            right: 1.3764,
            top: -1.4281,
            bottom: 0.9657,
        };
        let left = EyeFov {
            left: -1.3764,
            right: 0.8391,
            top: -1.4281,
            bottom: 0.9657,
        };
        let (ru, rv) = gaze_fraction(right);
        let (lu, lv) = gaze_fraction(left);
        assert!((ru + lu - 1.0).abs() < 1e-4, "gaze u mirrors: {ru} / {lu}");
        assert!((rv - lv).abs() < 1e-6, "gaze v identical across eyes");
        assert!(lu > 0.5, "left-eye gaze is right of center: {lu}");
        // Parallax: right eye (+dx) shifts the crop left; left eye (−dx) right.
        let (rau, _) = box_apparent_fraction(right, 0.032, 0.0, 1.0);
        let (lau, _) = box_apparent_fraction(left, -0.032, 0.0, 1.0);
        assert!(rau < ru, "right-eye box shifts left: {rau} < {ru}");
        assert!(lau > lu, "left-eye box shifts right: {lau} > {lu}");
        assert!(
            (rau + lau - 1.0).abs() < 1e-4,
            "apparent fractions mirror about 0.5: {rau} / {lau}"
        );
        // The gaze-centered crops mirror too (same size, mirrored x).
        let base = crop(0.34, 0.30, 0.32, 0.40);
        let rc = gaze_centered_crop(right, 0.032, 0.0, 1.0, &base);
        let lc = gaze_centered_crop(left, -0.032, 0.0, 1.0, &base);
        let rcx = rc.x + rc.w * 0.5;
        let lcx = lc.x + lc.w * 0.5;
        assert!(
            (rcx + lcx - 1.0).abs() < 1e-4,
            "crop centers mirror: {rcx}/{lcx}"
        );
        assert!((rc.y - lc.y).abs() < 1e-6 && (rc.w - lc.w).abs() < 1e-6);
    }

    /// Symmetric FOV → gaze is the frame center.
    #[test]
    fn gaze_fraction_symmetric_is_center() {
        let fov = EyeFov {
            left: -1.0,
            right: 1.0,
            top: -1.0,
            bottom: 1.0,
        };
        let (u, v) = gaze_fraction(fov);
        assert!((u - 0.5).abs() < 1e-5 && (v - 0.5).abs() < 1e-5);
    }

    /// Asymmetric (real right-eye) FOV: a frame-centered crop shifts to the
    /// gaze (left and up), keeping its size.
    #[test]
    fn gaze_centered_crop_shifts_to_asymmetric_gaze() {
        let fov = EyeFov {
            left: -0.8391,
            right: 1.3764,
            top: -1.4281,
            bottom: 0.9657,
        };
        let base = crop(0.34, 0.30, 0.32, 0.40); // centered at (0.5, 0.5)
        let g = gaze_centered_crop(fov, 0.0, 0.0, 1.0, &base);
        let cu = g.x + g.w * 0.5;
        let cv = g.y + g.h * 0.5;
        assert!((cu - 0.379).abs() < 0.005, "crop center u={cu}");
        assert!((cv - 0.403).abs() < 0.005, "crop center v={cv}");
        assert!(
            cu < 0.5 && cv < 0.5,
            "shifts left and up toward the gaze: {g:?}"
        );
        assert!((g.w - base.w).abs() < 1e-5, "size preserved: {g:?}");
    }
}
