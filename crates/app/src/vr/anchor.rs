//! World-space overlay placement, captured at show-time.
//!
//! Builds the 3×4 transform fed to `IVROverlay::SetOverlayTransformAbsolute`
//! so the overlay stays put in world space once it appears — looking back
//! down with your head no longer drags it around. Yaw + position come from
//! the HMD pose at the moment of capture; pitch/roll are deliberately
//! ignored so the overlay sits in front of the user (where they can read
//! it) rather than directly above their face (where they triggered it).
//!
//! OpenVR coordinate convention (right-handed, Y-up):
//!   +X right · +Y up · -Z forward
//! Overlay surface default normal is +Z, in overlay-local space.

/// How far forward (along the user's horizontal facing direction, metres)
/// the overlay sits from the HMD.
pub const DISTANCE_M: f32 = 1.2;
/// How far above (positive Y, metres) the overlay sits from the HMD by
/// default. Combined with the default `SHOW_PITCH_DEG = 45°` this puts
/// the panel just above where your gaze lands when you tilt up to trigger
/// it, so confirming the show + reading the panel are the same motion.
pub const HEIGHT_M: f32 = 1.0;
/// Tilt around the local X axis (positive degrees rotates the overlay's
/// top edge backward, so the front face looks down toward the viewer).
pub const TILT_DEG: f32 = 35.0;

/// Compute a world-space 3×4 transform that places the overlay
/// [`HEIGHT_M`] above and [`DISTANCE_M`] in front of the HMD at the moment
/// of capture, facing the user's horizontal forward direction and tilted
/// by [`TILT_DEG`]. `hmd_pose` is the HMD's `device_to_absolute_tracking`
/// matrix in the same tracking-universe origin you'll pass to
/// `SetOverlayTransformAbsolute`.
pub fn world_anchor_from_hmd(hmd_pose: &[[f32; 4]; 3]) -> [[f32; 4]; 3] {
    world_anchor_from_hmd_with(hmd_pose, DISTANCE_M, HEIGHT_M, TILT_DEG)
}

/// Same as [`world_anchor_from_hmd`] but with all three knobs supplied —
/// used by tests and for future live tuning.
pub fn world_anchor_from_hmd_with(
    hmd_pose: &[[f32; 4]; 3],
    distance_m: f32,
    height_m: f32,
    tilt_deg: f32,
) -> [[f32; 4]; 3] {
    let (sin_y, cos_y) = hmd_yaw_sin_cos(hmd_pose);
    let t = tilt_deg.to_radians();
    let (s, c) = (t.sin(), t.cos());

    // R = Ry(yaw) * Rx(tilt), worked out element-wise.
    let r00 = cos_y;
    let r01 = sin_y * s;
    let r02 = sin_y * c;
    let r11 = c;
    let r12 = -s;
    let r20 = -sin_y;
    let r21 = cos_y * s;
    let r22 = cos_y * c;

    // Local offset (0, height_m, -distance_m) rotated by yaw only, then
    // added to the HMD's world position. Yaw-only keeps the overlay at the
    // user's eye level + HEIGHT_M regardless of how far they craned their
    // neck to trigger it.
    let dx = -sin_y * distance_m;
    let dz = -cos_y * distance_m;
    let tx = hmd_pose[0][3] + dx;
    let ty = hmd_pose[1][3] + height_m;
    let tz = hmd_pose[2][3] + dz;

    [
        [r00, r01, r02, tx],
        [0.0, r11, r12, ty],
        [r20, r21, r22, tz],
    ]
}

/// Extract (sin, cos) of the HMD's yaw — i.e., the rotation around world
/// +Y that takes overlay-local forward (0,0,-1) to the horizontal
/// projection of HMD forward. Falls back to the horizontal projection of
/// the HMD's local -Y axis (the "chin-forward" direction) when the user is
/// craned ~straight up and forward goes singular.
fn hmd_yaw_sin_cos(hmd_pose: &[[f32; 4]; 3]) -> (f32, f32) {
    let fx = -hmd_pose[0][2];
    let fz = -hmd_pose[2][2];
    let h = (fx * fx + fz * fz).sqrt();
    if h > 1e-3 {
        // Ry(y) * (0,0,-1) = (-sin(y), 0, -cos(y)); match (fx/h, _, fz/h).
        return (-fx / h, -fz / h);
    }
    let cx = -hmd_pose[0][1];
    let cz = -hmd_pose[2][1];
    let ch = (cx * cx + cz * cz).sqrt().max(1e-6);
    (-cx / ch, -cz / ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HMD pose for a user at `pos` facing yaw `yaw_deg` (around +Y) with
    /// pitch `pitch_deg`. Roll is zero.
    fn hmd_pose(pos: [f32; 3], yaw_deg: f32, pitch_deg: f32) -> [[f32; 4]; 3] {
        let y = yaw_deg.to_radians();
        let p = pitch_deg.to_radians();
        let (sy, cy) = (y.sin(), y.cos());
        let (sp, cp) = (p.sin(), p.cos());
        // R = Ry(yaw) * Rx(pitch). Same composition the anchor uses; here
        // we feed pitch instead of tilt.
        [
            [cy, sy * sp, sy * cp, pos[0]],
            [0.0, cp, -sp, pos[1]],
            [-sy, cy * sp, cy * cp, pos[2]],
        ]
    }

    #[test]
    fn pitch_does_not_change_overlay_position() {
        // Same standing pose, different pitch: anchor should be identical.
        let neutral = hmd_pose([0.0, 1.7, 0.0], 0.0, 0.0);
        let craned = hmd_pose([0.0, 1.7, 0.0], 0.0, 75.0);
        let a = world_anchor_from_hmd(&neutral);
        let b = world_anchor_from_hmd(&craned);
        for r in 0..3 {
            for c in 0..4 {
                assert!(
                    (a[r][c] - b[r][c]).abs() < 1e-5,
                    "row {r} col {c}: {} vs {}",
                    a[r][c],
                    b[r][c],
                );
            }
        }
    }

    #[test]
    fn overlay_sits_in_front_of_user_in_yaw_direction() {
        // User at (5, 1.7, -2), facing +X (yaw=-90° in OpenVR's Ry sense
        // — see comment in hmd_pose).
        let pose = hmd_pose([5.0, 1.7, -2.0], -90.0, 30.0);
        let m = world_anchor_from_hmd(&pose);
        let pos = [m[0][3], m[1][3], m[2][3]];
        // User facing +X → overlay should be offset in +X by DISTANCE_M and
        // up by HEIGHT_M. Z stays equal to the HMD's Z.
        assert!((pos[0] - (5.0 + DISTANCE_M)).abs() < 1e-4, "x: {}", pos[0]);
        assert!((pos[1] - (1.7 + HEIGHT_M)).abs() < 1e-4, "y: {}", pos[1]);
        assert!((pos[2] - (-2.0)).abs() < 1e-4, "z: {}", pos[2]);
    }

    #[test]
    fn surface_normal_faces_back_toward_user() {
        // Yaw 0 (facing -Z). Overlay normal in world = R * (0,0,1) = col2.
        let pose = hmd_pose([0.0, 1.6, 0.0], 0.0, 0.0);
        let m = world_anchor_from_hmd(&pose);
        let n = [m[0][2], m[1][2], m[2][2]];
        // User looking -Z, overlay is at -Z. Normal should point toward
        // user → +Z component, and down → -Y component (because of tilt).
        assert!(n[1] < 0.0, "normal Y should be down: {n:?}");
        assert!(n[2] > 0.0, "normal Z should point back at user: {n:?}");
    }

    #[test]
    fn straight_up_fallback_does_not_nan() {
        // Pitch 90° — horizontal forward goes to zero length. Make sure
        // the fallback path produces a finite, sensible transform.
        let pose = hmd_pose([0.0, 1.7, 0.0], 0.0, 90.0);
        let m = world_anchor_from_hmd(&pose);
        for row in &m {
            for v in row {
                assert!(v.is_finite(), "non-finite component in {m:?}");
            }
        }
    }
}
