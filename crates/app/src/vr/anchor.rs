//! HMD-relative overlay placement.
//!
//! Builds the 3×4 transform fed to `IVROverlay::SetOverlayTransformTrackedDeviceRelative`
//! with the HMD as the reference device. The overlay floats above and in
//! front of the user, tilted to face them when they look up. Constants
//! here are deliberately tweakable — the exact pose only feels right after
//! trying it in a real headset (see OPEN_QUESTIONS.md, "Anchor position").
//!
//! OpenVR coordinate convention (right-handed, Y-up):
//!   +X right · +Y up · -Z forward
//! Overlay surface default normal is +Z, in overlay-local space.

/// How far forward (negative Z, metres) the overlay sits from the HMD.
pub const DISTANCE_M: f32 = 1.2;
/// How far above (positive Y, metres) the overlay sits from the HMD.
pub const HEIGHT_M: f32 = 0.6;
/// Tilt around the X axis (positive degrees rotates the overlay's top edge
/// backward, so the front face looks down toward the viewer).
pub const TILT_DEG: f32 = 35.0;

/// Row-major 3×4 column-augmented matrix (rotation + translation) that
/// places an overlay [`HEIGHT_M`] above and [`DISTANCE_M`] forward of the
/// HMD, tilted by [`TILT_DEG`] so the surface normal points down-and-back
/// toward the user. Returned in OpenVR's `HmdMatrix34_t` layout —
/// `[[f32; 4]; 3]`, with each row laid out `[r0, r1, r2, t]`.
pub fn hmd_relative_transform() -> [[f32; 4]; 3] {
    build(DISTANCE_M, HEIGHT_M, TILT_DEG)
}

/// Same as [`hmd_relative_transform`] but with all three parameters
/// supplied — useful for tests and for live-tuning later.
pub fn build(distance_m: f32, height_m: f32, tilt_deg: f32) -> [[f32; 4]; 3] {
    let t = tilt_deg.to_radians();
    let (s, c) = (t.sin(), t.cos());
    // Rx(t) =  1  0   0
    //          0  c  -s
    //          0  s   c
    // Translation column places the overlay at (0, height, -distance) in
    // HMD-local space.
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, c, -s, height_m],
        [0.0, s, c, -distance_m],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_column_matches_inputs() {
        let m = build(1.2, 0.5, 30.0);
        assert!((m[0][3] - 0.0).abs() < 1e-6);
        assert!((m[1][3] - 0.5).abs() < 1e-6);
        assert!((m[2][3] - -1.2).abs() < 1e-6);
    }

    #[test]
    fn tilted_normal_points_down_and_back() {
        // Overlay-local +Z (its forward normal) transformed by the rotation
        // should point down (-Y) and back (+Z in HMD space, since OpenVR
        // forward is -Z).
        let m = build(1.2, 0.5, 45.0);
        // Column 2 of the rotation = R * (0,0,1).
        let n = [m[0][2], m[1][2], m[2][2]];
        assert!(n[1] < 0.0, "normal Y should be negative (down): {n:?}");
        assert!(n[2] > 0.0, "normal Z should be positive (back): {n:?}");
    }

    #[test]
    fn zero_tilt_is_identity_rotation() {
        let m = build(0.0, 0.0, 0.0);
        assert!((m[0][0] - 1.0).abs() < 1e-6);
        assert!((m[1][1] - 1.0).abs() < 1e-6);
        assert!((m[2][2] - 1.0).abs() < 1e-6);
    }
}
