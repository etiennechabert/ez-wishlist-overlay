//! Pitch-driven visibility state machine.
//!
//! Shows the overlay the instant pitch crosses `SHOW_PITCH_DEG`, hides
//! immediately when pitch drops below `HIDE_PITCH_DEG`. The 25° gap
//! between the two thresholds is the hysteresis dead-zone that prevents
//! the overlay from flickering at the boundary — no extra dwell timer is
//! needed (an earlier iteration had one; user feedback said the lag from
//! that debounce just felt sluggish). See SPEC.md §7.2.
//!
//! This module is intentionally OpenVR-agnostic — Phase 3 just feeds it the
//! pitch it reads from `WaitGetPoses` each frame.

/// Show threshold in degrees of pitch up from horizontal.
pub const SHOW_PITCH_DEG: f32 = 45.0;
/// Hide threshold in degrees of pitch up from horizontal.
pub const HIDE_PITCH_DEG: f32 = 20.0;

/// Extract HMD pitch (degrees up from horizontal) from an OpenVR
/// `device_to_absolute_tracking` 3×4 matrix.
///
/// OpenVR uses a right-handed Y-up coordinate system; the HMD's local
/// forward axis is `-Z`. Transforming local forward `(0, 0, -1)` by the
/// 3×3 rotation part of the matrix yields the forward direction in world
/// space; its `y` component is `sin(pitch)`.
///
/// Returns `0.0` if the matrix is degenerate (all-zero, as supplied for
/// disconnected devices).
pub fn pitch_from_hmd_matrix(matrix: &[[f32; 4]; 3]) -> f32 {
    // Local forward (0,0,-1) → world = -col2 of the rotation 3×3.
    let fy = -matrix[1][2];
    if !fy.is_finite() {
        return 0.0;
    }
    fy.clamp(-1.0, 1.0).asin().to_degrees()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Hidden,
    Visible,
}

pub struct VisibilityFsm {
    state: Visibility,
}

impl Default for VisibilityFsm {
    fn default() -> Self {
        Self::new()
    }
}

impl VisibilityFsm {
    pub fn new() -> Self {
        Self {
            state: Visibility::Hidden,
        }
    }

    pub fn state(&self) -> Visibility {
        self.state
    }

    /// Feed one pitch sample using the canonical SHOW/HIDE thresholds.
    /// Returns the new state (which may equal the old).
    #[allow(dead_code)] // Used by tests; runtime uses `tick_with` for live settings.
    pub fn tick(&mut self, pitch_deg: f32) -> Visibility {
        self.tick_with(pitch_deg, SHOW_PITCH_DEG, HIDE_PITCH_DEG)
    }

    /// Variant that lets the caller supply the show/hide thresholds —
    /// used by the runtime so live settings edits take effect on the
    /// very next tick.
    pub fn tick_with(&mut self, pitch_deg: f32, show_deg: f32, hide_deg: f32) -> Visibility {
        self.state = match self.state {
            Visibility::Hidden if pitch_deg >= show_deg => Visibility::Visible,
            Visibility::Visible if pitch_deg < hide_deg => Visibility::Hidden,
            // In the hysteresis band (or no threshold cross): hold.
            s => s,
        };
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_instantly_when_above_show_threshold() {
        let mut fsm = VisibilityFsm::new();
        assert_eq!(fsm.tick(70.0), Visibility::Visible);
    }

    #[test]
    fn hides_instantly_when_below_hide_threshold() {
        let mut fsm = VisibilityFsm::new();
        fsm.tick(70.0);
        assert_eq!(fsm.state(), Visibility::Visible);

        // Look down clearly below HIDE_PITCH_DEG (20°): instant hide.
        assert_eq!(fsm.tick(10.0), Visibility::Hidden);
    }

    #[test]
    fn hysteresis_band_holds_state() {
        let mut fsm = VisibilityFsm::new();
        fsm.tick(70.0);
        assert_eq!(fsm.state(), Visibility::Visible);

        // 30° is in the band (HIDE 20° < 30° < SHOW 45°) — should NOT hide.
        assert_eq!(fsm.tick(30.0), Visibility::Visible);

        // And while hidden, the same in-band value shouldn't pop us visible either.
        let mut fsm = VisibilityFsm::new();
        assert_eq!(fsm.tick(30.0), Visibility::Hidden);
    }

    /// Build a 3×4 OpenVR-style rotation matrix for a head pitched up by
    /// `pitch_deg`. Rotation around X: forward (0,0,-1) → (0, sin(p), -cos(p)).
    fn pitched_hmd_matrix(pitch_deg: f32) -> [[f32; 4]; 3] {
        let p = pitch_deg.to_radians();
        let (s, c) = (p.sin(), p.cos());
        // Row-major rotation around X by +pitch_deg:
        //   [1   0       0   ]
        //   [0  cos(p) -sin(p)]
        //   [0  sin(p)  cos(p)]
        // With +pitch rotating local forward (0,0,-1) → (0, sin(p), -cos(p)).
        [[1.0, 0.0, 0.0, 0.0], [0.0, c, -s, 0.0], [0.0, s, c, 0.0]]
    }

    #[test]
    fn pitch_from_matrix_recovers_input_angle() {
        for deg in [-80.0_f32, -30.0, 0.0, 15.0, 45.0, 60.0, 80.0] {
            let m = pitched_hmd_matrix(deg);
            let got = pitch_from_hmd_matrix(&m);
            assert!(
                (got - deg).abs() < 0.05,
                "expected {deg}°, got {got}° from matrix",
            );
        }
    }

    #[test]
    fn pitch_handles_degenerate_matrix() {
        let zero = [[0.0_f32; 4]; 3];
        assert_eq!(pitch_from_hmd_matrix(&zero), 0.0);
    }
}
