//! Capture-session types shared by the runtime loop, the GUI, and the
//! guide-box renderer.
//!
//! These are deliberately **platform-independent** (no OpenVR) so the
//! mode→[`JobKind`] routing and the capture-state machine are unit-tested on
//! every target — the OpenVR glue that drives them lives in the Windows-only
//! `overlay`/`runtime` code, which CI can't compile.
//!
//! ## Capture model (see issue #136)
//! Capture is **trigger-driven**, not timed: the app puts the overlay into a
//! [`CaptureMode`] (hideout / box / stash), and in-game each controller
//! trigger pull takes one screenshot. While a mode is active the top wishlist
//! overlay is suppressed so the trigger isn't double-bound, and a guide box
//! shows where to aim + the current [`CaptureState`].

use crate::ocr::{JobKind, ScanTarget};

/// What the headset trigger captures right now.
///
/// `Off` is the normal state: the wishlist overlay is shown and the trigger
/// does cell clicks. Any other variant routes the trigger to a screenshot,
/// suppresses the wishlist overlay, and shows the guide box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    /// Not capturing — normal wishlist overlay + cell-click trigger.
    #[default]
    Off,
    /// Facility-upgrade panel (the hideout flow). Tagged [`JobKind::UpgradePanel`].
    Hideout,
    /// A world container / stash scan. Tagged [`JobKind::BoxScan`]; the inner
    /// [`ScanTarget`] tells the worker which store to write on finish.
    Box(ScanTarget),
}

impl CaptureMode {
    /// `true` for any active capture mode (i.e. not [`CaptureMode::Off`]).
    pub fn is_active(&self) -> bool {
        !matches!(self, CaptureMode::Off)
    }

    /// The OCR job kind a capture taken in this mode should carry, or `None`
    /// when no mode is active (no capture should be taken).
    pub fn job_kind(&self) -> Option<JobKind> {
        match self {
            CaptureMode::Off => None,
            CaptureMode::Hideout => Some(JobKind::UpgradePanel),
            CaptureMode::Box(_) => Some(JobKind::BoxScan),
        }
    }

    /// The box/stash scan target, when this is a [`CaptureMode::Box`].
    pub fn scan_target(&self) -> Option<&ScanTarget> {
        match self {
            CaptureMode::Box(t) => Some(t),
            _ => None,
        }
    }

    /// Short caption for the guide box.
    pub fn guide_caption(&self) -> &'static str {
        match self {
            CaptureMode::Off => "",
            CaptureMode::Hideout => "Aim at the upgrade panel",
            CaptureMode::Box(ScanTarget::Stash) => "Aim at the stash terminal",
            CaptureMode::Box(ScanTarget::Container(_)) => "Aim at the container",
        }
    }
}

/// Where the trigger-driven capture loop is right now, surfaced on the guide
/// box so the user knows whether to hold still or pull the trigger again.
///
/// There is no timed "delay" phase any more (the auto-capture timer was
/// removed): between captures the loop simply sits at [`CaptureState::Ready`]
/// waiting for the next trigger pull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureState {
    /// Idle — waiting for the user to pull the trigger.
    #[default]
    Ready,
    /// Grabbing the compositor-mirror frame this tick.
    Capturing,
    /// The frame was handed to the OCR worker, which is processing it.
    RunningOcr,
}

impl CaptureState {
    /// Compact label for the guide-box status chip.
    pub fn label(&self) -> &'static str {
        match self {
            CaptureState::Ready => "Ready — pull trigger",
            CaptureState::Capturing => "Capturing…",
            CaptureState::RunningOcr => "Reading…",
        }
    }

    /// Status-chip color (RGB), matching the overlay palette conventions:
    /// green = ready, amber = capturing, teal = OCR running.
    pub fn rgb(&self) -> (u8, u8, u8) {
        match self {
            CaptureState::Ready => (80, 180, 100),
            CaptureState::Capturing => (200, 180, 80),
            CaptureState::RunningOcr => (89, 190, 175),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::ScanTarget;

    #[test]
    fn job_kind_routes_per_mode() {
        assert_eq!(CaptureMode::Off.job_kind(), None);
        assert_eq!(CaptureMode::Hideout.job_kind(), Some(JobKind::UpgradePanel));
        assert_eq!(
            CaptureMode::Box(ScanTarget::Stash).job_kind(),
            Some(JobKind::BoxScan)
        );
        assert_eq!(
            CaptureMode::Box(ScanTarget::Container("crate_a".into())).job_kind(),
            Some(JobKind::BoxScan)
        );
    }

    #[test]
    fn is_active_only_when_not_off() {
        assert!(!CaptureMode::Off.is_active());
        assert!(CaptureMode::Hideout.is_active());
        assert!(CaptureMode::Box(ScanTarget::Stash).is_active());
    }

    #[test]
    fn scan_target_only_for_box() {
        assert_eq!(CaptureMode::Off.scan_target(), None);
        assert_eq!(CaptureMode::Hideout.scan_target(), None);
        assert_eq!(
            CaptureMode::Box(ScanTarget::Stash).scan_target(),
            Some(&ScanTarget::Stash)
        );
    }

    #[test]
    fn captions_and_labels_present_for_active_states() {
        assert!(CaptureMode::Off.guide_caption().is_empty());
        assert!(!CaptureMode::Hideout.guide_caption().is_empty());
        assert!(!CaptureMode::Box(ScanTarget::Stash)
            .guide_caption()
            .is_empty());
        for s in [
            CaptureState::Ready,
            CaptureState::Capturing,
            CaptureState::RunningOcr,
        ] {
            assert!(!s.label().is_empty());
        }
    }

    #[test]
    fn default_is_off_and_ready() {
        assert_eq!(CaptureMode::default(), CaptureMode::Off);
        assert_eq!(CaptureState::default(), CaptureState::Ready);
    }
}
