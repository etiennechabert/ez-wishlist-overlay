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
#[allow(dead_code)] // Wired in by the Phase 3 OpenVR runtime loop.
pub const DWELL_MS: u64 = 350;

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
        match self.state {
            Visibility::Hidden => {
                if pitch_deg >= SHOW_PITCH_DEG {
                    self.pending_since = Some(now);
                    self.state = Visibility::PendingShow;
                }
            }
            Visibility::PendingShow => {
                if pitch_deg < HIDE_PITCH_DEG {
                    self.pending_since = None;
                    self.state = Visibility::Hidden;
                } else if pitch_deg >= SHOW_PITCH_DEG {
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
                if pitch_deg < HIDE_PITCH_DEG {
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
