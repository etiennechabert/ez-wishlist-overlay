//! Pitch-driven visibility state machine.
//!
//! Shows the overlay when the user holds pitch ≥ `SHOW_PITCH_DEG` for
//! `DWELL_MS` milliseconds; hides immediately when pitch drops below
//! `HIDE_PITCH_DEG`. See SPEC.md §7.2.
//!
//! This module is intentionally OpenVR-agnostic — Phase 3 just feeds it the
//! pitch it reads from `WaitGetPoses` each frame.

use std::time::{Duration, Instant};

/// Show threshold in degrees of pitch up from horizontal.
pub const SHOW_PITCH_DEG: f32 = 60.0;
/// Hide threshold in degrees of pitch up from horizontal.
pub const HIDE_PITCH_DEG: f32 = 45.0;
/// Required dwell above the show threshold before fade-in.
pub const DWELL_MS: u64 = 350;

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
    /// Above SHOW threshold but still within the dwell window.
    PendingShow,
    Visible,
}

pub struct VisibilityFsm {
    state: Visibility,
    /// When pitch first crossed the SHOW threshold during the current
    /// pending-show window.
    pending_since: Option<Instant>,
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
            pending_since: None,
        }
    }

    pub fn state(&self) -> Visibility {
        self.state
    }

    /// Feed one pitch sample. Returns the new state (which may equal the old).
    #[allow(dead_code)] // Wired in by the Phase 3 OpenVR runtime loop.
    pub fn tick(&mut self, pitch_deg: f32, now: Instant) -> Visibility {
        self.tick_with_dwell(pitch_deg, now, Duration::from_millis(DWELL_MS))
    }

    /// Variant that lets tests override the dwell duration.
    pub fn tick_with_dwell(&mut self, pitch_deg: f32, now: Instant, dwell: Duration) -> Visibility {
        self.tick_with(pitch_deg, now, SHOW_PITCH_DEG, HIDE_PITCH_DEG, dwell)
    }

    /// General variant: caller supplies the show/hide thresholds and dwell.
    /// Used by the runtime so live settings edits take effect immediately.
    pub fn tick_with(
        &mut self,
        pitch_deg: f32,
        now: Instant,
        show_deg: f32,
        hide_deg: f32,
        dwell: Duration,
    ) -> Visibility {
        match self.state {
            Visibility::Hidden => {
                if pitch_deg >= show_deg {
                    self.pending_since = Some(now);
                    self.state = Visibility::PendingShow;
                }
            }
            Visibility::PendingShow => {
                if pitch_deg < hide_deg {
                    self.pending_since = None;
                    self.state = Visibility::Hidden;
                } else if pitch_deg >= show_deg {
                    if let Some(t0) = self.pending_since {
                        if now.duration_since(t0) >= dwell {
                            self.state = Visibility::Visible;
                            self.pending_since = None;
                        }
                    }
                } else {
                    // In the hysteresis band — hold.
                }
            }
            Visibility::Visible => {
                if pitch_deg < hide_deg {
                    self.state = Visibility::Hidden;
                    self.pending_since = None;
                }
            }
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_until_dwell_elapses() {
        let mut fsm = VisibilityFsm::new();
        let t0 = Instant::now();
        let dwell = Duration::from_millis(350);

        // Look up immediately: pending, not visible.
        assert_eq!(
            fsm.tick_with_dwell(70.0, t0, dwell),
            Visibility::PendingShow
        );
        // 200ms later still pending.
        assert_eq!(
            fsm.tick_with_dwell(70.0, t0 + Duration::from_millis(200), dwell),
            Visibility::PendingShow
        );
        // 400ms in: visible.
        assert_eq!(
            fsm.tick_with_dwell(70.0, t0 + Duration::from_millis(400), dwell),
            Visibility::Visible
        );
    }

    #[test]
    fn hides_instantly_when_below_hide_threshold() {
        let mut fsm = VisibilityFsm::new();
        let t0 = Instant::now();
        let dwell = Duration::from_millis(350);
        fsm.tick_with_dwell(70.0, t0, dwell);
        fsm.tick_with_dwell(70.0, t0 + Duration::from_millis(400), dwell);
        assert_eq!(fsm.state(), Visibility::Visible);

        // Look down: instant hide.
        assert_eq!(
            fsm.tick_with_dwell(20.0, t0 + Duration::from_millis(450), dwell),
            Visibility::Hidden
        );
    }

    #[test]
    fn hysteresis_band_holds_state() {
        let mut fsm = VisibilityFsm::new();
        let t0 = Instant::now();
        let dwell = Duration::from_millis(100);
        fsm.tick_with_dwell(70.0, t0, dwell);
        fsm.tick_with_dwell(70.0, t0 + Duration::from_millis(150), dwell);
        assert_eq!(fsm.state(), Visibility::Visible);

        // 50° is in the band — should NOT hide.
        assert_eq!(
            fsm.tick_with_dwell(50.0, t0 + Duration::from_millis(200), dwell),
            Visibility::Visible
        );
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

    #[test]
    fn brief_glance_does_not_show() {
        let mut fsm = VisibilityFsm::new();
        let t0 = Instant::now();
        let dwell = Duration::from_millis(350);
        // Look up...
        fsm.tick_with_dwell(70.0, t0, dwell);
        // ...look down before dwell elapses.
        assert_eq!(
            fsm.tick_with_dwell(20.0, t0 + Duration::from_millis(100), dwell),
            Visibility::Hidden
        );
    }
}
