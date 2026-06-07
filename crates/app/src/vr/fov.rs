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
/// the half-angle from the center view axis to each clipping plane.
///
/// By OpenVR's convention `left < 0 < right` and `top < 0 < bottom` (the
/// vertical axis is reported "image-down" — the *top* clipping plane is the
/// most-negative tangent). The full mirror frame therefore spans `[left, right]`
/// horizontally and `[top, bottom]` vertically, and a frame point at normalized
/// `(u, v)` (origin top-left) sits at tangent
/// `(left + u·(right−left), top + v·(bottom−top))`.
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

/// Map a normalized crop rect (fractions of the mirror frame, origin top-left)
/// into a head-locked metric placement at `distance_m`, using the eye's FOV
/// tangents. This is the core of issue #141 — see the module docs.
///
/// The crop is sanitized first (clamped to a sane sub-rect), so callers can
/// pass a raw per-mode crop directly.
pub fn guide_placement(fov: EyeFov, crop: &CaptureCrop, distance_m: f32) -> GuidePlacement {
    let mut c = *crop;
    c.sanitize();
    let d = distance_m.max(0.01);

    // Full-frame tangent spans. Both are positive (`.abs()` guards a runtime
    // that reports a flipped sign); the *signed* spans below carry the sign
    // needed to place the crop center correctly.
    let span_x = fov.span_x();
    let span_y = fov.span_y();
    let signed_x = fov.right - fov.left;
    let signed_y = fov.bottom - fov.top;

    // Crop center in frame fractions → tangent space.
    let uc = c.x + c.w * 0.5;
    let vc = c.y + c.h * 0.5;
    let tan_x_c = fov.left + uc * signed_x;
    let tan_y_c = fov.top + vc * signed_y;

    GuidePlacement {
        width_m: c.w * span_x * d,
        height_m: c.h * span_y * d,
        center_x_m: tan_x_c * d,
        // Projection Y runs image-down (the top frame row is the most-negative
        // tangent), but the HMD frame is Y-up, so negate: a crop near the top
        // of the frame must place the box *up*.
        center_y_m: -tan_y_c * d,
        distance_m: d,
    }
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

    /// Per-eye asymmetry: an off-center frustum (e.g. the right eye, which sees
    /// further toward the nose) shifts even a frame-centered crop off the gaze
    /// axis — the correction a flat constant can't make.
    #[test]
    fn asymmetric_eye_offsets_centered_crop() {
        let fov = EyeFov {
            left: -1.39,
            right: 1.24,
            top: -1.46,
            bottom: 1.46,
        };
        // A crop centered in the frame (center fraction 0.5, 0.5).
        let p = guide_placement(fov, &crop(0.1, 0.1, 0.8, 0.8), 1.0);
        // mid_tan_x = (-1.39 + 1.24)/2 = -0.075 → small negative X offset.
        assert!(p.center_x_m < 0.0 && p.center_x_m > -0.2, "{p:?}");
        // Vertically symmetric → ~no Y offset.
        assert!(p.center_y_m.abs() < 1e-3, "{p:?}");
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
}
