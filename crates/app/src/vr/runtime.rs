//! VR background thread: owns the OpenVR session, drives pose → visibility,
//! renders the overlay texture, reports status to the GUI.
//!
//! On Windows this thread tries to attach to SteamVR, retries every 5s while
//! disconnected, and surfaces transitions via a shared [`VrStatus`]. On other
//! targets it parks immediately at [`VrStatus::Unsupported`] — Phase 1+2 of
//! the desktop app are platform-agnostic and we don't want a broken VR layer
//! to bleed into macOS/Linux iteration builds.

use crate::ocr::OcrJob;
use crate::persist::PersistPaths;
use crate::settings::Settings;
#[cfg(target_os = "windows")]
use crate::settings::{CaptureCrop, CaptureEye, CaptureHand};
use crate::state::AppState;
use crate::vr::capture_session::{CaptureMode, CaptureState};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Subdirectory under `PersistPaths::debug_dir` where captured mirror-texture
/// WebP crops (and their OCR sidecars) are written. Lives inside the per-session
/// `debug/` bundle that's flushed at startup, so captures never accumulate
/// across sessions. The OCR test bed under `ocr_data/` is happy to consume
/// from wherever.
const SCREENSHOT_SUBDIR: &str = "vr_screenshots";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VrStatus {
    /// Compile target has no OpenVR support (e.g. macOS, Linux).
    Unsupported,
    /// `VR_IsRuntimeInstalled()` returned false — SteamVR isn't installed
    /// on this machine. Distinct from `Disconnected` because the user's
    /// next step is "install Steam → install SteamVR" rather than "launch
    /// SteamVR".
    RuntimeNotInstalled,
    /// SteamVR is installed but the worker can't attach (process not
    /// running, dashboard not up, etc.). Retries every 5 s.
    Disconnected,
    /// Worker is calling `VR_Init` right now.
    Connecting,
    /// Session is live and the overlay handle is created.
    Connected,
    /// Last attach attempt produced a hard error. Worker keeps retrying.
    Error(String),
}

impl VrStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Unsupported => "VR: unavailable on this OS".into(),
            Self::RuntimeNotInstalled => "VR: SteamVR not installed (install via Steam)".into(),
            Self::Disconnected => "VR: not running".into(),
            Self::Connecting => "VR: connecting…".into(),
            Self::Connected => "VR: connected".into(),
            Self::Error(msg) => format!("VR error: {msg}"),
        }
    }

    pub fn color(&self) -> egui::Color32 {
        match self {
            Self::Connected => egui::Color32::from_rgb(80, 180, 100),
            Self::Connecting => egui::Color32::from_rgb(200, 180, 80),
            Self::Error(_) => egui::Color32::from_rgb(220, 100, 90),
            Self::Disconnected | Self::Unsupported | Self::RuntimeNotInstalled => {
                egui::Color32::GRAY
            }
        }
    }
}

/// Spawn the VR worker. The returned handle is the GUI's read view onto the
/// session state.
pub struct Runtime {
    status: Arc<RwLock<VrStatus>>,
    /// One-shot trigger for "save the next mirror texture as PNG". Bounded
    /// so the GUI can't queue thousands of pending captures if the user
    /// mashes the button while SteamVR is disconnected.
    capture_tx: Sender<()>,
    /// Most recent capture result the worker thread reported. Cleared when
    /// the GUI consumes it via [`Runtime::take_last_capture`].
    last_capture: Arc<RwLock<Option<CaptureResult>>>,
    /// Which capture mode the headset trigger is bound to (issue #136).
    /// Ephemeral — always starts [`CaptureMode::Off`] so a capture mode can't
    /// be left armed into a raid. While active the wishlist overlay is
    /// suppressed and each trigger pull takes one screenshot. The GUI sets it
    /// via [`Runtime::set_capture_mode`] / [`Runtime::exit_capture_mode`].
    capture_mode: Arc<RwLock<CaptureMode>>,
    /// Where the trigger-driven capture loop is right now (Ready / Capturing /
    /// reading). Written by the VR loop, read by the GUI for status display.
    capture_state: Arc<RwLock<CaptureState>>,
    _join: std::thread::JoinHandle<()>,
}

#[derive(Clone, Debug)]
pub enum CaptureResult {
    /// Capture succeeded and the cropped WebP was written to disk at this path.
    /// Only produced when `settings.ocr_debug` is on, because the runtime
    /// otherwise skips the disk write entirely (see [`Runtime::spawn`]).
    Ok(PathBuf),
    /// Capture succeeded but nothing was written to disk — the default fast
    /// path, where the bitmap goes straight to the OCR worker over
    /// the channel and never touches disk. The GUI surfaces this as
    /// a generic "captured" toast since there's no file to show.
    Ephemeral,
    Err(String),
}

impl Runtime {
    /// Spawn the VR worker thread.
    ///
    /// `ocr_tx` receives one [`OcrJob`] per successful capture — the
    /// already-decoded mirror-texture bitmap, plus the on-disk WebP
    /// path when `settings.ocr_debug` is on (otherwise `None`, and
    /// nothing is written at all). The downstream OCR worker thread
    /// (in `main.rs`) reads from the matching receiver and reflects
    /// parsed counts back into `AppState`. Bounded + `try_send` so a
    /// busy OCR worker can't backpressure the VR render loop;
    /// excess captures are silently dropped on the OCR side.
    pub fn spawn(
        state: Arc<RwLock<AppState>>,
        settings: Arc<RwLock<Settings>>,
        paths: Arc<PersistPaths>,
        ocr_tx: Sender<OcrJob>,
        ocr_feedback_rx: Receiver<crate::gui::OcrFeedback>,
    ) -> Self {
        let initial = if cfg!(target_os = "windows") {
            VrStatus::Connecting
        } else {
            VrStatus::Unsupported
        };
        let status = Arc::new(RwLock::new(initial));
        let status_writer = status.clone();
        let last_capture = Arc::new(RwLock::new(None));
        let last_capture_writer = last_capture.clone();
        // Bound at 4 so a mashed button doesn't queue work the worker can't
        // service. `try_send` from the GUI side is non-blocking; excess
        // presses are silently dropped, which is fine semantically — a
        // screenshot is a one-shot ask.
        let (capture_tx, capture_rx) = crossbeam_channel::bounded::<()>(4);
        let capture_mode = Arc::new(RwLock::new(CaptureMode::Off));
        let capture_mode_worker = capture_mode.clone();
        let capture_state = Arc::new(RwLock::new(CaptureState::Ready));
        let capture_state_worker = capture_state.clone();
        let join = std::thread::Builder::new()
            .name("ez-wishlist-vr".into())
            .spawn(move || {
                run(
                    state,
                    settings,
                    paths,
                    status_writer,
                    capture_rx,
                    last_capture_writer,
                    ocr_tx,
                    ocr_feedback_rx,
                    capture_mode_worker,
                    capture_state_worker,
                )
            })
            .expect("spawn VR thread");
        Self {
            status,
            capture_tx,
            last_capture,
            capture_mode,
            capture_state,
            _join: join,
        }
    }

    /// Build a runtime with **no** live VR worker, for headless GUI tests that
    /// must hand a `Runtime` to a pane (e.g. the Containers tab's box-scan
    /// controls) but never drive a real capture. Parks at
    /// [`VrStatus::Unsupported`]; the flags and channels are real, so the
    /// `set_*` / `*_enabled` accessors behave normally — nothing reads the far
    /// end. The background thread exits immediately.
    #[cfg(test)]
    pub fn disconnected_for_test() -> Self {
        let (capture_tx, _capture_rx) = crossbeam_channel::bounded::<()>(4);
        Self {
            status: Arc::new(RwLock::new(VrStatus::Unsupported)),
            capture_tx,
            last_capture: Arc::new(RwLock::new(None)),
            capture_mode: Arc::new(RwLock::new(CaptureMode::Off)),
            capture_state: Arc::new(RwLock::new(CaptureState::Ready)),
            _join: std::thread::Builder::new()
                .name("ez-wishlist-vr-test".into())
                .spawn(|| {})
                .expect("spawn dummy VR thread"),
        }
    }

    pub fn status(&self) -> VrStatus {
        self.status.read().clone()
    }

    /// Ask the VR worker to take one mirror-texture screenshot the next time
    /// it ticks. Non-blocking; if the queue is full (4 pending captures) the
    /// extra press is dropped silently.
    pub fn request_screenshot(&self) {
        let _ = self.capture_tx.try_send(());
    }

    /// Returns the most recent capture result and clears it, so the GUI
    /// shows a status line once per capture instead of permanently.
    pub fn take_last_capture(&self) -> Option<CaptureResult> {
        self.last_capture.write().take()
    }

    /// Arm a capture mode (hideout / box / stash). Ephemeral — never persisted,
    /// so it always starts [`CaptureMode::Off`] on launch and can't be left
    /// armed into a raid. While a mode is active the VR loop suppresses the
    /// wishlist overlay, shows the guide box, and routes each controller
    /// trigger pull to a screenshot. Resets the displayed state to
    /// [`CaptureState::Ready`].
    pub fn set_capture_mode(&self, mode: CaptureMode) {
        *self.capture_state.write() = CaptureState::Ready;
        *self.capture_mode.write() = mode;
    }

    /// Leave capture mode — restores the normal wishlist overlay + cell-click
    /// trigger. Shorthand for `set_capture_mode(CaptureMode::Off)`.
    pub fn exit_capture_mode(&self) {
        self.set_capture_mode(CaptureMode::Off);
    }

    /// The currently armed capture mode (clone — it's cheap).
    pub fn capture_mode(&self) -> CaptureMode {
        self.capture_mode.read().clone()
    }

    /// Whether any capture mode is active.
    pub fn capture_active(&self) -> bool {
        self.capture_mode.read().is_active()
    }

    /// Where the trigger-driven capture loop is right now, for GUI status.
    pub fn capture_state(&self) -> CaptureState {
        *self.capture_state.read()
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
fn run(
    _state: Arc<RwLock<AppState>>,
    _settings: Arc<RwLock<Settings>>,
    _paths: Arc<PersistPaths>,
    status: Arc<RwLock<VrStatus>>,
    _capture_rx: Receiver<()>,
    _last_capture: Arc<RwLock<Option<CaptureResult>>>,
    _ocr_tx: Sender<OcrJob>,
    _ocr_feedback_rx: Receiver<crate::gui::OcrFeedback>,
    _capture_mode: Arc<RwLock<CaptureMode>>,
    _capture_state: Arc<RwLock<CaptureState>>,
) {
    // Status is already Unsupported. Nothing else to do — park the thread.
    *status.write() = VrStatus::Unsupported;
    tracing::info!("VR worker idle: no OpenVR support on this target");
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn run(
    state: Arc<RwLock<AppState>>,
    settings: Arc<RwLock<Settings>>,
    paths: Arc<PersistPaths>,
    status: Arc<RwLock<VrStatus>>,
    capture_rx: Receiver<()>,
    last_capture: Arc<RwLock<Option<CaptureResult>>>,
    ocr_tx: Sender<OcrJob>,
    ocr_feedback_rx: Receiver<crate::gui::OcrFeedback>,
    capture_mode: Arc<RwLock<CaptureMode>>,
    capture_state: Arc<RwLock<CaptureState>>,
) {
    use super::overlay::OverlaySession;

    loop {
        // Cheap probe before we attempt VR_Init: if the OpenVR runtime
        // isn't installed at all, surface that distinctly so the user
        // knows the fix is "install SteamVR via Steam" rather than
        // "launch the SteamVR you already have". `is_runtime_installed`
        // is a top-level fn that doesn't need a Context.
        if !openvr::is_runtime_installed() {
            *status.write() = VrStatus::RuntimeNotInstalled;
            std::thread::sleep(RETRY_DELAY);
            continue;
        }
        *status.write() = VrStatus::Connecting;
        let initial_width = settings.read().vr.width_meters;
        match OverlaySession::init(initial_width) {
            Ok(mut session) => {
                *status.write() = VrStatus::Connected;
                tracing::info!("VR overlay initialized");
                // Anchor is captured on each show transition, not at init —
                // see render_loop / OverlaySession::anchor_at_current_hmd.
                let lost = render_loop(
                    &mut session,
                    &state,
                    &settings,
                    &paths,
                    &capture_rx,
                    &last_capture,
                    &ocr_tx,
                    &ocr_feedback_rx,
                    &capture_mode,
                    &capture_state,
                );
                if let Err(e) = lost {
                    tracing::warn!(error = %e, "VR session lost");
                }
                drop(session); // VR_Shutdown via Context Drop.
                *status.write() = VrStatus::Disconnected;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(error = %msg, "VR init failed");
                *status.write() = VrStatus::Disconnected;
            }
        }
        std::thread::sleep(RETRY_DELAY);
    }
}

/// ~90Hz inner loop. Returns `Err` when SteamVR appears to have gone away
/// (any OpenVR call returning an error), prompting the outer loop to retry.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn render_loop(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    settings: &Arc<RwLock<Settings>>,
    paths: &Arc<PersistPaths>,
    capture_rx: &Receiver<()>,
    last_capture: &Arc<RwLock<Option<CaptureResult>>>,
    ocr_tx: &Sender<OcrJob>,
    ocr_feedback_rx: &Receiver<crate::gui::OcrFeedback>,
    capture_mode: &Arc<RwLock<CaptureMode>>,
    capture_state: &Arc<RwLock<CaptureState>>,
) -> anyhow::Result<()> {
    use super::input::Debouncer;
    use super::pose::{Visibility, VisibilityFsm};
    use super::render::CellHit;
    use openvr::system::EventInfo;
    use std::time::Instant;

    const TICK: Duration = Duration::from_millis(11); // ~90Hz

    let mut fsm = VisibilityFsm::new();
    // The data-rendered pixmap is cached so we don't re-decode all icons on
    // every frame — only re-render when version/grid_cols/max_items changes.
    // Hover border is a cheap copy+stroke applied per frame on top of the cache.
    let mut clean_pixmap: Option<tiny_skia::Pixmap> = None;
    let mut clean_sig: Option<(u64, u32, u32)> = None;
    let mut last_hits: Vec<CellHit> = Vec::new();
    let mut last_canvas: (u32, u32) = (0, 0);
    let mut fade_start: Option<Instant> = None;
    let mut was_visible = false;
    let mut debouncer = Debouncer::new();
    let mut event_buf: Vec<EventInfo> = Vec::with_capacity(8);
    let mut current_hover: Option<String> = None;
    // Lifecycle of the head-locked OCR feedback card. The worker
    // produces an `OcrFeedback` for every state transition; this loop
    // owns the on-screen lifetime (auto-fade in release, hold-until-
    // replaced in debug). `ocr_state` is `None` when nothing is
    // showing, `Some(_)` while a card is up.
    let mut ocr_state: Option<OcrOverlayState> = None;

    // Capture bookkeeping. Capture is now trigger-driven (issue #136), so
    // there's no timed pacing — `capture_in_flight` is just a single-flight
    // latch so a rapid second trigger pull can't dispatch a second grab while
    // the worker is still reading the first. Set when a capture is dispatched,
    // cleared when the worker reports a terminal result. `dispatched_at` backs
    // a watchdog that frees the latch if a dispatched job never reports back.
    let mut capture_in_flight = false;
    let mut dispatched_at: Option<Instant> = None;
    // Guide-box render cache: re-render only when the chip it shows changes —
    // keyed on (mode, chip label, trigger, eye-only, capture-eye) so it updates
    // when the capture phase, the post-capture confirmation, the trigger, or the
    // single-eye / capture-eye settings change, not every 90 Hz frame. `None` =
    // not shown.
    let mut guide_shown: Option<(CaptureMode, String, CaptureHand, bool, CaptureEye)> = None;
    // Post-capture OCR confirmation shown on the guide chip (over "Ready — pull
    // trigger") until `GUIDE_CONFIRM_DURATION` after `shown_at`. Issue #136.
    let mut guide_confirm: Option<(String, (u8, u8, u8), Instant)> = None;

    loop {
        let frame_start = Instant::now();
        let (vr, capture_eye) = {
            let s = settings.read();
            (s.vr.clone(), s.capture_eye)
        };
        session.apply_settings(&vr)?;

        let mode = capture_mode.read().clone();
        let cap_active = mode.is_active();
        // Per-mode capture crop (issue #136): the hideout panel, container, and
        // stash screens have known, different shapes, so each uses its own fixed
        // crop rect (and thus guide-box aspect). `Off` maps to the hideout crop —
        // unused, since capture is inactive then.
        let crop = CaptureCrop::for_mode(&mode);
        // FOV-derived guide-box placement needs the capture eye's frustum
        // tangents so the box exactly outlines its crop (issue #141). Query once
        // per tick while a mode is active; fall back to a generic FOV if
        // IVRSystem is momentarily unavailable this frame.
        let eye_fov = if cap_active {
            session
                .eye_fov(capture_eye)
                .unwrap_or(super::fov::EyeFov::FALLBACK)
        } else {
            super::fov::EyeFov::FALLBACK
        };

        let pitch = session.hmd_pitch_deg().unwrap_or(0.0);
        // The wishlist overlay is suppressed while a capture mode is active so
        // the controller trigger isn't double-bound — in capture mode it takes
        // a screenshot, not a cell click. Forcing `visible_now = false` lets the
        // existing visibility-transition path hide + fade it out cleanly.
        let visible_now = !cap_active
            && matches!(
                fsm.tick_with(pitch, vr.show_pitch_deg, vr.hide_pitch_deg),
                Visibility::Visible
            );

        handle_visibility_transition(
            session,
            state,
            VisibilityTransition {
                visible_now,
                pitch,
                frame_start,
                grid_cols: vr.grid_cols,
                max_items: vr.max_items,
                height_offset_m: vr.height_offset_m,
            },
            &mut was_visible,
            &mut fade_start,
            &mut clean_pixmap,
            &mut clean_sig,
            &mut last_hits,
            &mut last_canvas,
            &mut current_hover,
        )?;

        // Input. `session.poll_trigger_actions()` steps the IVRInput action set
        // and must be called exactly once per tick, so route it by mode:
        //   - capture mode: a trigger rising edge requests a screenshot; mouse /
        //     cell-click events are ignored.
        //   - otherwise: the existing wishlist cell-click handlers run.
        session.drain_events(&mut event_buf);
        let mut capture_requested = false;
        if cap_active {
            event_buf.clear();
            let (lt, rt) = session.poll_trigger_actions();
            // Only the configured hand's trigger captures (issue #136); the
            // other hand is left alone so it stays free for in-game menu
            // navigation. `(fired, ignored)` splits the poll by that choice.
            let (fired, ignored) = match vr.capture_trigger {
                CaptureHand::Left => (lt, rt),
                CaptureHand::Right => (rt, lt),
            };
            if let Some(device) = fired {
                capture_requested = true;
                tracing::info!(
                    ?mode,
                    hand = vr.capture_trigger.label(),
                    device = device.0,
                    "capture trigger → screenshot"
                );
            }
            if let Some(device) = ignored {
                tracing::debug!(
                    device = device.0,
                    "capture mode: other-hand trigger ignored (free for in-game menu)"
                );
            }
        } else {
            handle_overlay_events(
                session,
                state,
                &mut event_buf,
                &last_hits,
                visible_now,
                last_canvas,
                &mut debouncer,
                frame_start,
            );
            poll_trigger_actions(
                session,
                state,
                &last_hits,
                visible_now,
                last_canvas,
                &mut debouncer,
                frame_start,
            );
        }

        // SPACE hotkey also captures (desktop dev + manual fallback). Coalesce
        // multiple presses within one ~11 ms frame.
        let mut space_requested = false;
        while capture_rx.try_recv().is_ok() {
            space_requested = true;
        }
        // The desktop SPACE key / Debug "Capture now" button is the only
        // non-controller capture source (desktop testing + manual fallback).
        if space_requested {
            tracing::info!(cap_active, ?mode, "capture via SPACE / desktop request");
        }

        // Take one capture this tick. In a capture mode the trigger (or SPACE)
        // drives it, single-flighted so a rapid second pull can't overlap the
        // worker. SPACE outside any mode keeps the legacy one-shot upgrade-panel
        // grab so desktop testing works without arming a mode.
        if cap_active && (capture_requested || space_requested) {
            let ocr_enabled = settings.read().ocr_enabled;
            if let Some(kind) = mode.job_kind() {
                if ocr_enabled && !capture_in_flight {
                    *capture_state.write() = CaptureState::Capturing;
                    // A new capture supersedes any lingering confirmation chip.
                    guide_confirm = None;
                    // Push the "Capturing" chip to the overlay *before* the
                    // (blocking) mirror grab so the user gets instant feedback
                    // that the trigger registered.
                    ensure_guide(
                        session,
                        &mode,
                        CaptureState::Capturing,
                        vr.capture_trigger,
                        None,
                        eye_fov,
                        vr.guide_eye_only,
                        capture_eye,
                        &mut guide_shown,
                    );
                    let dispatched = capture_and_forward(
                        session,
                        settings,
                        paths,
                        ocr_tx,
                        last_capture,
                        &mut ocr_state,
                        true,
                        kind,
                        crop,
                        eye_fov,
                    );
                    if dispatched {
                        capture_in_flight = true;
                        dispatched_at = Some(frame_start);
                        *capture_state.write() = CaptureState::RunningOcr;
                        // Flip to "Reading" the moment OCR is dispatched, so the
                        // user knows the shot is taken and can scroll onward.
                        ensure_guide(
                            session,
                            &mode,
                            CaptureState::RunningOcr,
                            vr.capture_trigger,
                            None,
                            eye_fov,
                            vr.guide_eye_only,
                            capture_eye,
                            &mut guide_shown,
                        );
                    } else {
                        *capture_state.write() = CaptureState::Ready;
                    }
                }
            }
        } else if space_requested && !cap_active {
            capture_and_forward(
                session,
                settings,
                paths,
                ocr_tx,
                last_capture,
                &mut ocr_state,
                true,
                crate::ocr::JobKind::UpgradePanel,
                crop,
                eye_fov,
            );
        }

        // Guide box: while a capture mode is active, keep the head-locked aiming
        // reticle + status chip current. The chip shows the post-capture OCR
        // confirmation for a few seconds (over "Ready — pull trigger"), then the
        // ready prompt. `ensure_guide` re-renders only when the chip changes;
        // it's also pushed eagerly the instant the capture state flips (above).
        if cap_active {
            let st = *capture_state.read();
            let confirm = guide_confirm
                .as_ref()
                .filter(|(_, _, shown_at)| {
                    frame_start.duration_since(*shown_at) < GUIDE_CONFIRM_DURATION
                })
                .map(|(label, rgb, _)| (label.clone(), *rgb));
            ensure_guide(
                session,
                &mode,
                st,
                vr.capture_trigger,
                confirm.as_ref(),
                eye_fov,
                vr.guide_eye_only,
                capture_eye,
                &mut guide_shown,
            );
        } else {
            // Not capturing: drop any pending confirmation and hide the box.
            guide_confirm = None;
            if guide_shown.is_some() {
                let _ = session.set_guide_alpha(0.0);
                let _ = session.set_guide_visible(false);
                guide_shown = None;
            }
        }

        // Single-flight watchdog: a dispatched read normally clears
        // `capture_in_flight` when the worker reports back (see
        // `drive_ocr_overlay` below). If it never does (e.g. OCR disabled
        // mid-flight so the worker skipped the job), free the latch after 30 s
        // so the next trigger pull isn't wedged.
        if let Some(t) = dispatched_at {
            if capture_in_flight && frame_start.duration_since(t) > Duration::from_secs(30) {
                tracing::warn!("capture: no OCR result within 30 s — clearing in-flight latch");
                capture_in_flight = false;
                dispatched_at = None;
                *capture_state.write() = CaptureState::Ready;
            }
        }

        if visible_now {
            let current_version = state.read().version;
            let need_data_render = clean_sig != Some((current_version, vr.grid_cols, vr.max_items));
            if need_data_render {
                render_data(
                    state,
                    vr.grid_cols,
                    vr.max_items,
                    &mut clean_pixmap,
                    &mut clean_sig,
                    &mut last_hits,
                    &mut last_canvas,
                )?;
            }

            let new_hover = compute_hover(session, &last_hits, last_canvas);
            if new_hover != current_hover {
                match (&current_hover, &new_hover) {
                    (None, Some(id)) => tracing::info!(item = %id, "hover ON"),
                    (Some(prev), None) => tracing::info!(was = %prev, "hover OFF"),
                    (Some(prev), Some(next)) => {
                        tracing::info!(from = %prev, to = %next, "hover SWITCH")
                    }
                    (None, None) => {}
                }
            }
            if need_data_render || new_hover != current_hover {
                submit_with_hover(
                    session,
                    clean_pixmap.as_ref(),
                    &last_hits,
                    new_hover.as_deref(),
                )?;
                current_hover = new_hover;
            }
            apply_fade_in(session, &mut fade_start, frame_start)?;
        }

        {
            let (ocr_debug, dismiss, feedback_grid) = {
                let s = settings.read();
                (
                    s.ocr_debug,
                    Duration::from_secs(s.ocr_dismiss_seconds as u64),
                    s.ocr_feedback_grid,
                )
            };
            let terminal = drive_ocr_overlay(
                session,
                ocr_feedback_rx,
                &mut ocr_state,
                frame_start,
                ocr_debug,
                matches!(mode, CaptureMode::Box(_)),
                dismiss,
                feedback_grid,
            );
            if terminal {
                // OCR finished — release the single-flight latch and return the
                // capture state to Ready (waiting for the next trigger pull).
                capture_in_flight = false;
                dispatched_at = None;
                if cap_active {
                    *capture_state.write() = CaptureState::Ready;
                    // Surface the result on the guide chip for a few seconds
                    // (over "Ready — pull trigger"), so the capture confirmation
                    // lands right where the user is aiming (issue #136).
                    if let Some(c) = ocr_state.as_ref().and_then(|s| s.feedback.chip_confirm()) {
                        guide_confirm = Some((c.0, c.1, frame_start));
                    }
                }
            }
        }

        // Liveness probe — fails when SteamVR disappears.
        session.heartbeat()?;

        // Pace the loop.
        let spent = frame_start.elapsed();
        if spent < TICK {
            std::thread::sleep(TICK - spent);
        }
    }
}

#[cfg(target_os = "windows")]
struct VisibilityTransition {
    visible_now: bool,
    pitch: f32,
    frame_start: std::time::Instant,
    grid_cols: u32,
    max_items: u32,
    height_offset_m: f32,
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn handle_visibility_transition(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    t: VisibilityTransition,
    was_visible: &mut bool,
    fade_start: &mut Option<std::time::Instant>,
    clean_pixmap: &mut Option<tiny_skia::Pixmap>,
    clean_sig: &mut Option<(u64, u32, u32)>,
    last_hits: &mut Vec<super::render::CellHit>,
    last_canvas: &mut (u32, u32),
    current_hover: &mut Option<String>,
) -> anyhow::Result<()> {
    if t.visible_now && !*was_visible {
        if session.anchor_at_current_hmd(t.height_offset_m)? {
            render_data(
                state,
                t.grid_cols,
                t.max_items,
                clean_pixmap,
                clean_sig,
                last_hits,
                last_canvas,
            )?;
            submit_with_hover(session, clean_pixmap.as_ref(), last_hits, None)?;
            *current_hover = None;
            session.set_alpha(0.0)?;
            session.set_visible(true)?;
            *fade_start = Some(t.frame_start);
            *was_visible = true;
            tracing::debug!(pitch = t.pitch, "overlay: fade in");
        } else {
            tracing::debug!("overlay show deferred: HMD pose invalid");
        }
    } else if !t.visible_now && *was_visible {
        session.set_visible(false)?;
        session.set_alpha(0.0)?;
        *fade_start = None;
        *was_visible = false;
        tracing::debug!(pitch = t.pitch, "overlay: hide");
    }
    Ok(())
}

/// Drain queued mouse events. We poll even while hidden so the queue doesn't
/// back up across hide/show cycles; clicks while hidden are dropped here.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn handle_overlay_events(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    event_buf: &mut Vec<openvr::system::EventInfo>,
    last_hits: &[super::render::CellHit],
    visible_now: bool,
    last_canvas: (u32, u32),
    debouncer: &mut super::input::Debouncer,
    frame_start: std::time::Instant,
) {
    use openvr::system::Event;
    // OpenVR `EButton_SteamVR_Trigger`. In modern SteamVR, non-dashboard
    // overlays don't get auto-routed `MouseButtonDown` events — the trigger
    // comes through as a raw `ButtonPress` and we ray-cast ourselves.
    const TRIGGER_BUTTON_ID: u32 = 33;

    for ev in event_buf.drain(..) {
        match ev.event {
            Event::MouseButtonDown(mouse) => {
                dispatch_mouse_down(
                    session,
                    state,
                    ev.tracked_device_index,
                    mouse,
                    last_hits,
                    visible_now,
                    last_canvas,
                    debouncer,
                    frame_start,
                );
            }
            Event::ButtonPress(c) if c.button == TRIGGER_BUTTON_ID => {
                dispatch_trigger_press(
                    session,
                    state,
                    ev.tracked_device_index,
                    last_hits,
                    visible_now,
                    last_canvas,
                    debouncer,
                    frame_start,
                );
            }
            _ => {}
        }
    }
}

/// Distance (metres) the head-locked guide box floats in front of the gaze.
/// The FOV→metric mapping ([`super::fov`]) scales linearly with this, so the box
/// outlines its crop at any distance; 1 m matches the historical guide offset.
/// Issue #141.
#[cfg(target_os = "windows")]
const GUIDE_DISTANCE_M: f32 = 1.0;

/// Base pixel width of the guide texture's transparent hole. The height follows
/// from the crop's metric aspect; the margins (caption / chip / frame) are added
/// around it by [`super::guide::layout_for_hole`]. Issue #141.
#[cfg(target_os = "windows")]
const GUIDE_HOLE_W: u32 = 1024;

/// How long the post-capture OCR confirmation sits on the guide-box status chip
/// (over "Ready — pull trigger") before reverting to the ready prompt. #136.
#[cfg(target_os = "windows")]
const GUIDE_CONFIRM_DURATION: Duration = Duration::from_secs(4);

/// Render + submit the capture guide box, sized + positioned so its transparent
/// hole **exactly outlines its OCR crop** via the eye FOV (issue #141).
///
/// The crop's edges map through `eye_fov` to head-locked metric extents at
/// [`GUIDE_DISTANCE_M`] (see [`super::fov::guide_placement`]); the hole is
/// rendered at that metric aspect, the texture margins carry the caption / chip
/// / frame *outside* the hole, and the overlay width is scaled up by
/// `tex_w / hole_w` so the hole still lands on the crop. The transform +
/// width are skip-cached in the session, so recomputing them every call is
/// cheap; the (heavier) pixmap is only re-rendered when the chip changes
/// (tracked via `guide_shown`).
///
/// The status chip shows `confirm` (the post-capture OCR result — text + fill
/// color) when `Some`, otherwise the live capture phase `st`. Called from the
/// steady-state guide block and eagerly the instant the capture state flips, so
/// the chip never lags.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn ensure_guide(
    session: &mut super::overlay::OverlaySession,
    mode: &CaptureMode,
    st: CaptureState,
    trigger: CaptureHand,
    confirm: Option<&(String, (u8, u8, u8))>,
    eye_fov: super::fov::EyeFov,
    guide_eye_only: bool,
    capture_eye: CaptureEye,
    guide_shown: &mut Option<(CaptureMode, String, CaptureHand, bool, CaptureEye)>,
) {
    // Geometry: map the per-mode crop through the eye FOV to a head-locked
    // metric placement. Recomputed every call (cheap math) so a capture-eye /
    // FOV change is reflected even when the chip label is unchanged; the
    // transform + width + stereo setters skip redundant OpenVR calls internally.
    let crop = CaptureCrop::for_mode(mode);
    let placement = super::fov::guide_placement(eye_fov, &crop, GUIDE_DISTANCE_M);
    let hole_h = ((GUIDE_HOLE_W as f32) / placement.aspect())
        .round()
        .clamp(64.0, 4096.0) as u32;
    let layout = super::guide::layout_for_hole(GUIDE_HOLE_W, hole_h);
    // The hole maps to placement.width_m; the full texture is wider by the
    // margins, so scale the overlay width up to keep the hole on the crop.
    let overlay_w = placement.width_m * (layout.tex_w as f32 / layout.hole_w.max(1) as f32);
    let _ = session.set_guide_transform(
        placement.center_x_m,
        placement.center_y_m,
        placement.distance_m,
    );
    let _ = session.set_guide_width(overlay_w);
    // Single-eye box (issue #143): SideBySide stereo flag + a texture that has
    // content in only the capture eye's half (built below). Cached internally.
    let _ = session.set_guide_stereo(guide_eye_only);

    let (label, rgb) = match confirm {
        Some((text, rgb)) => (text.as_str(), *rgb),
        None => (st.label(), st.rgb()),
    };
    let key = (
        mode.clone(),
        label.to_string(),
        trigger,
        guide_eye_only,
        capture_eye,
    );
    if guide_shown.as_ref() == Some(&key) {
        return;
    }
    let content = super::guide::render(mode, label, rgb, trigger.label(), &layout);
    // When single-eye, pack the content into the capture eye's half of a
    // double-wide side-by-side texture; the other eye then sees nothing.
    let pix = if guide_eye_only {
        super::guide::side_by_side(&content, matches!(capture_eye, CaptureEye::Left))
    } else {
        content
    };
    if let Err(e) = session.submit_guide_rgba(pix.data(), pix.width(), pix.height()) {
        tracing::warn!(error = %e, "guide overlay: submit failed");
    }
    let _ = session.set_guide_visible(true);
    let _ = session.set_guide_alpha(1.0);
    *guide_shown = Some(key);
}

/// Step the IVRInput action set and fire `dispatch_trigger_press` for
/// each hand whose trigger transitioned to pressed this tick. This is the
/// supported click path: legacy `GetControllerState` returns dead zeros
/// for Quest Touch via Link / Index knuckles via SteamVR's modern driver
/// stack, while the action system stays alive.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn poll_trigger_actions(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    last_hits: &[super::render::CellHit],
    visible_now: bool,
    last_canvas: (u32, u32),
    debouncer: &mut super::input::Debouncer,
    frame_start: std::time::Instant,
) {
    let (left, right) = session.poll_trigger_actions();
    for (label, device) in [("left", left), ("right", right)] {
        let Some(device) = device else { continue };
        tracing::info!(hand = label, device = device.0, "trigger action fired");
        dispatch_trigger_press(
            session,
            state,
            device,
            last_hits,
            visible_now,
            last_canvas,
            debouncer,
            frame_start,
        );
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn dispatch_trigger_press(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    device_index: openvr::TrackedDeviceIndex,
    last_hits: &[super::render::CellHit],
    visible_now: bool,
    last_canvas: (u32, u32),
    debouncer: &mut super::input::Debouncer,
    frame_start: std::time::Instant,
) {
    use super::input::{
        handle_click, texcoord_to_pixel, ClickOutcome, HAPTIC_INCREMENT_US, HAPTIC_RESET_US,
    };

    if !visible_now {
        tracing::info!("trigger ignored: overlay not visible (look up to summon)");
        return;
    }
    let (cw, ch) = last_canvas;
    if cw == 0 || ch == 0 {
        tracing::info!("trigger ignored: nothing rendered yet (canvas 0x0)");
        return;
    }
    let Some((u, v)) = session.intersect_from_device(device_index) else {
        tracing::info!(?device_index, "trigger fired but ray missed overlay");
        return;
    };
    let (px, py) = texcoord_to_pixel(u, v, cw, ch);
    tracing::info!(u, v, px, py, "trigger-ray hit overlay");
    match handle_click(state, last_hits, px, py, debouncer, frame_start) {
        ClickOutcome::Incremented { item_id, new_value } => {
            session.haptic_pulse(device_index, HAPTIC_INCREMENT_US);
            tracing::info!(%item_id, new_value, "trigger-ray click: +1");
        }
        ClickOutcome::Reset { item_id } => {
            session.haptic_pulse(device_index, HAPTIC_RESET_US);
            tracing::info!(%item_id, "trigger-ray click: cycle reset to 0");
        }
        ClickOutcome::Ignored => {
            tracing::info!("trigger-ray click: ignored (no hit / debounced)");
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn dispatch_mouse_down(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    device_index: openvr::TrackedDeviceIndex,
    mouse: openvr::system::event::Mouse,
    last_hits: &[super::render::CellHit],
    visible_now: bool,
    last_canvas: (u32, u32),
    debouncer: &mut super::input::Debouncer,
    frame_start: std::time::Instant,
) {
    use super::input::{
        handle_click, texcoord_to_pixel, ClickOutcome, HAPTIC_INCREMENT_US, HAPTIC_RESET_US,
    };

    /// EVRMouseButton::Left in the OpenVR mouse-event button bitfield.
    const MOUSE_BUTTON_LEFT: u32 = 1;

    if mouse.button & MOUSE_BUTTON_LEFT == 0 || !visible_now {
        return;
    }
    let (cw, ch) = last_canvas;
    if cw == 0 || ch == 0 {
        return; // first frame after spawn — nothing rendered yet
    }
    let (px, py) = texcoord_to_pixel(mouse.position.0, mouse.position.1, cw, ch);
    tracing::info!(
        tex_x = mouse.position.0,
        tex_y = mouse.position.1,
        px,
        py,
        canvas_w = cw,
        canvas_h = ch,
        hits = last_hits.len(),
        "overlay mouse-down received"
    );
    match handle_click(state, last_hits, px, py, debouncer, frame_start) {
        ClickOutcome::Incremented { item_id, new_value } => {
            session.haptic_pulse(device_index, HAPTIC_INCREMENT_US);
            tracing::info!(%item_id, new_value, "overlay click: +1");
        }
        ClickOutcome::Reset { item_id } => {
            session.haptic_pulse(device_index, HAPTIC_RESET_US);
            tracing::info!(%item_id, "overlay click: cycle reset to 0");
        }
        ClickOutcome::Ignored => {
            tracing::info!("overlay click: ignored (no hit / debounced)");
        }
    }
}

#[cfg(target_os = "windows")]
fn apply_fade_in(
    session: &mut super::overlay::OverlaySession,
    fade_start: &mut Option<std::time::Instant>,
    frame_start: std::time::Instant,
) -> anyhow::Result<()> {
    const FADE: Duration = Duration::from_millis(150);
    if let Some(t0) = *fade_start {
        let elapsed = frame_start.duration_since(t0);
        let alpha = (elapsed.as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
        session.set_alpha(alpha)?;
        if alpha >= 1.0 {
            *fade_start = None;
        }
    } else {
        session.set_alpha(1.0)?;
    }
    Ok(())
}

/// Build the path for the next screenshot. Format matches the in-game F12
/// naming convention (`YYYYMMDDhhmmss_<nanos>.webp`) so sorting it next to
/// Steam's screenshots in a file browser feels natural. The capture is written
/// directly as WebP q95 (issue #141) — the same format the committed fixtures
/// use, so a debug capture can be promoted to a fixture without re-encoding.
#[cfg(target_os = "windows")]
fn next_screenshot_path(paths: &PersistPaths) -> PathBuf {
    use time::macros::format_description;
    use time::OffsetDateTime;

    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let stamp = now
        .format(format_description!(
            "[year][month][day][hour][minute][second]"
        ))
        .unwrap_or_else(|_| "00000000000000".into());
    // Nanosecond suffix disambiguates rapid presses within the same second.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    paths
        .debug_dir
        .join(SCREENSHOT_SUBDIR)
        .join(format!("{stamp}_{nanos:09}.webp"))
}

/// Encode `img` as lossy WebP at quality `q` (0–100) via libwebp (the `webp`
/// crate). The `image` crate's built-in WebP encoder is lossless-only, so the
/// debug capture (issue #141) goes through libwebp directly to land at q95 —
/// matching the committed fixture format with no out-of-band re-encode step.
#[cfg(target_os = "windows")]
fn encode_webp(img: &image::DynamicImage, q: f32) -> Vec<u8> {
    let rgb = img.to_rgb8();
    let encoder = webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height());
    encoder.encode(q).to_vec()
}

/// Grab one compositor-mirror frame and hand it to the OCR worker.
/// Shared by the manual SPACE path and the auto-capture loop.
///
/// Retires any visible OCR card first: the card is a real SteamVR
/// overlay composited into the eye buffers, so leaving it up would bake
/// it into the screenshot and the next OCR pass would read itself over
/// the panel. The debug image-on-disk (WebP q95, issue #141) is gated on
/// `settings.ocr_debug` (off by default → no disk round-trip, ~2-3 s OCR
/// instead of ~6-7 s). Capture errors are surfaced to the GUI via
/// `last_capture` rather than aborting the render loop.
///
/// `beep` plays the system done/error sound — `true` for manual
/// captures (audible confirmation in-headset), `false` for the auto
/// loop (a ding every few seconds would be maddening). Returns `true`
/// when a bitmap was actually dispatched to the worker, `false` on
/// capture error — the auto loop uses this to decide whether to arm its
/// single-flight latch.
///
/// `box_scan` tags the job [`crate::ocr::JobKind::BoxScan`] and switches the
/// dispatch to a *blocking* send, because every scroll capture is load-bearing
/// — dropping one could leave a row out of the merged list.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn capture_and_forward(
    session: &mut super::overlay::OverlaySession,
    settings: &Arc<RwLock<Settings>>,
    paths: &Arc<PersistPaths>,
    ocr_tx: &Sender<OcrJob>,
    last_capture: &Arc<RwLock<Option<CaptureResult>>>,
    ocr_state: &mut Option<OcrOverlayState>,
    beep: bool,
    kind: crate::ocr::JobKind,
    crop: crate::settings::CaptureCrop,
    eye_fov: super::fov::EyeFov,
) -> bool {
    use image::GenericImageView as _;
    let box_scan = matches!(kind, crate::ocr::JobKind::BoxScan);
    if ocr_state.is_some() {
        if let Err(e) = session.set_ocr_alpha(0.0) {
            tracing::warn!(error = %e, "OCR overlay: pre-capture clear-alpha failed");
        }
        if let Err(e) = session.set_ocr_visible(false) {
            tracing::warn!(error = %e, "OCR overlay: pre-capture hide failed");
        }
        *ocr_state = None;
    }
    let (capture_eye_setting, trace, ocr_debug) = {
        let s = settings.read();
        (s.capture_eye, s.ocr_capture_trace, s.ocr_debug)
    };
    let capture_eye: super::capture::CaptureEye = capture_eye_setting.into();
    // Capture eye's head-space offset (≈ ±IPD/2), for the crop's parallax
    // correction so the captured rect lines up with the head-locked box as seen
    // by this eye (issue #141).
    let eye_off = session
        .eye_offset(capture_eye_setting)
        .unwrap_or((0.0, 0.0, 0.0));
    let debug_path = ocr_debug.then(|| next_screenshot_path(paths));
    // Capture the mirror frame into memory; the debug image we keep (when
    // enabled) is the *cropped* OCR input, written below — not the full frame.
    let capture_result = session.capture_screenshot(capture_eye, trace);
    let (result, dispatched) = match capture_result {
        Ok(img) => {
            // Aggressive crop to the guide-box region (issue #136) — cuts the
            // surrounding game UI text out of the frame before OCR. The crop is
            // re-centered on the eye's forward (gaze) axis (issue #141): the
            // mirror is the raw asymmetric eye render, so the straight-ahead
            // guide box lands at the gaze fraction, *not* the frame center.
            // Downstream (anchor / box read) works in the cropped image's own
            // pixel space, so nothing assumes full-frame coordinates.
            let (iw, ih) = img.dimensions();
            let mirror_crop = super::fov::gaze_centered_crop(
                eye_fov,
                eye_off.0,
                eye_off.1,
                GUIDE_DISTANCE_M,
                &crop,
            );
            let (cx, cy, cw, ch) = mirror_crop.px_rect(iw, ih);
            let img = img.crop_imm(cx, cy, cw, ch);
            // Debug artifact (when `ocr_debug` is on): the exact cropped region
            // OCR reads — so you can verify the crop + the read off-headset.
            // Written directly as WebP q95 (issue #141), no PNG round-trip.
            if let Some(path) = &debug_path {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(path, encode_webp(&img, 95.0)) {
                    Ok(()) => {
                        tracing::info!(path = %path.display(), "wrote cropped OCR debug WebP")
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to write cropped OCR debug WebP")
                    }
                }
                // Guide-box ↔ crop alignment diagnostic (issue #141 in-headset
                // tuning): the FOV, the straight-ahead box placement, the gaze
                // fraction, and the gaze-centered crop actually captured. Read
                // this next to a capture to confirm the box outlines its crop.
                let placement = super::fov::guide_placement(eye_fov, &crop, GUIDE_DISTANCE_M);
                let (sx, sy) = (eye_fov.span_x(), eye_fov.span_y());
                let (gaze_u, gaze_v) = super::fov::gaze_fraction(eye_fov);
                let (app_u, app_v) = super::fov::box_apparent_fraction(
                    eye_fov,
                    eye_off.0,
                    eye_off.1,
                    GUIDE_DISTANCE_M,
                );
                let diag = format!(
                    "guide-box vs crop diagnostic (issue #141)\n\
                     eye_fov tangents : left={:.4} right={:.4} top={:.4} bottom={:.4}\n\
                     spans            : x={:.4} y={:.4}  (frustum mid: x={:.4} y={:.4})\n\
                     gaze fraction    : u={:.4} v={:.4}  (frame center is 0.5,0.5)\n\
                     eye offset (m)   : dx={:.4} dy={:.4} dz={:.4}\n\
                     box apparent     : u={:.4} v={:.4}  (gaze + IPD parallax → crop center)\n\
                     mirror frame     : {}x{} px\n\
                     base crop        : x={:.3} y={:.3} w={:.3} h={:.3}  (center {:.3},{:.3})\n\
                     gaze crop        : x={:.3} y={:.3} w={:.3} h={:.3}  (center {:.3},{:.3})\n\
                     gaze crop pixels : x={} y={} w={} h={}\n\
                     box placement (D={:.2}m): center=({:.4},{:.4})m  size=({:.4} x {:.4})m\n",
                    eye_fov.left,
                    eye_fov.right,
                    eye_fov.top,
                    eye_fov.bottom,
                    sx,
                    sy,
                    (eye_fov.left + eye_fov.right) * 0.5,
                    (eye_fov.top + eye_fov.bottom) * 0.5,
                    gaze_u,
                    gaze_v,
                    eye_off.0,
                    eye_off.1,
                    eye_off.2,
                    app_u,
                    app_v,
                    iw,
                    ih,
                    crop.x,
                    crop.y,
                    crop.w,
                    crop.h,
                    crop.x + crop.w * 0.5,
                    crop.y + crop.h * 0.5,
                    mirror_crop.x,
                    mirror_crop.y,
                    mirror_crop.w,
                    mirror_crop.h,
                    mirror_crop.x + mirror_crop.w * 0.5,
                    mirror_crop.y + mirror_crop.h * 0.5,
                    cx,
                    cy,
                    cw,
                    ch,
                    GUIDE_DISTANCE_M,
                    placement.center_x_m,
                    placement.center_y_m,
                    placement.width_m,
                    placement.height_m,
                );
                let _ = std::fs::write(path.with_extension("guide.txt"), diag);
            }
            let job = OcrJob {
                image: img,
                source_path: debug_path.clone(),
                kind,
            };
            // Upgrade captures: best-effort `try_send` — if the worker is busy
            // we drop this shot (it only keeps the latest anyway) so the render
            // loop never blocks. Box-scan captures: blocking `send`, because a
            // dropped scroll position could drop a row from the merged list. Safe
            // to block here — box captures are deliberate and seconds apart.
            if box_scan {
                let _ = ocr_tx.send(job);
            } else {
                let _ = ocr_tx.try_send(job);
            }
            if beep {
                super::capture::play_capture_done_beep(true);
            }
            let r = match debug_path {
                Some(path) => CaptureResult::Ok(path),
                None => CaptureResult::Ephemeral,
            };
            (r, true)
        }
        Err(e) => {
            tracing::warn!(error = %e, "compositor mirror capture failed");
            if beep {
                super::capture::play_capture_done_beep(false);
            }
            (CaptureResult::Err(format!("{e:#}")), false)
        }
    };
    *last_capture.write() = Some(result);
    dispatched
}

/// Re-render the wishlist grid into `clean_pixmap` (no hover overlay).
/// Slow path: decodes icons, lays out cells. Called only when the data
/// changes (wishlist version bump, grid-cols change).
#[cfg(target_os = "windows")]
fn render_data(
    state: &Arc<RwLock<AppState>>,
    grid_cols: u32,
    max_items: u32,
    clean_pixmap: &mut Option<tiny_skia::Pixmap>,
    clean_sig: &mut Option<(u64, u32, u32)>,
    last_hits: &mut Vec<super::render::CellHit>,
    last_canvas: &mut (u32, u32),
) -> anyhow::Result<()> {
    use super::render;
    use crate::assets;

    let (mut items, version) = {
        let st = state.read();
        (st.active_items(), st.version)
    };
    // Trim to the user's top-N priority cap (0 = no cap) before layout, so the
    // overlay shows just the leaders and the click hit-table only covers cells
    // we actually draw.
    render::cap_wishlist(&mut items, max_items);
    let (pixmap, hits) = render::render(&items, grid_cols, assets::read_icon);
    *last_canvas = (pixmap.width(), pixmap.height());
    *last_hits = hits;
    *clean_pixmap = Some(pixmap);
    *clean_sig = Some((version, grid_cols, max_items));
    Ok(())
}

/// Clone the cached clean pixmap, paint the hover-highlight border on top
/// (if any), submit. Cheap path: per-frame when the user sweeps the laser.
#[cfg(target_os = "windows")]
fn submit_with_hover(
    session: &mut super::overlay::OverlaySession,
    clean: Option<&tiny_skia::Pixmap>,
    hits: &[super::render::CellHit],
    hover_id: Option<&str>,
) -> anyhow::Result<()> {
    let Some(clean) = clean else {
        return Ok(()); // Nothing to submit yet — first render hasn't run.
    };
    let mut frame = clean.clone();
    if let Some(id) = hover_id {
        super::render::apply_hover_highlight(&mut frame, hits, id);
    }
    session.submit_rgba(frame.data(), frame.width(), frame.height())?;
    Ok(())
}

/// Render state for the head-locked OCR feedback overlay. Carries the
/// most recently published [`crate::gui::OcrFeedback`] plus the moment
/// the card became visible — together they drive auto-fade in release
/// builds and replace-on-new in debug builds.
#[cfg(target_os = "windows")]
struct OcrOverlayState {
    feedback: crate::gui::OcrFeedback,
    /// When this particular feedback card showed up on the overlay.
    /// Refreshed every time the worker publishes a new feedback so
    /// fade-out timing resets correctly across rapid successive runs.
    visible_since: std::time::Instant,
    /// True when the texture has been pushed to the overlay at least
    /// once for this feedback. We render → submit exactly once per
    /// state transition; per-frame fades go through `set_ocr_alpha`
    /// only. Re-submitting an unchanged pixmap at 90 Hz was both
    /// wasteful and (more importantly) visibly flickered the card.
    submitted: bool,
    /// Current alpha last pushed to SteamVR. Tracked alongside the
    /// overlay session's own cache so we can compare against the
    /// computed fade target without rounding noise.
    last_alpha: f32,
}

/// Drain pending feedback messages and drive the OCR overlay's
/// show / hide / fade lifecycle.
///
/// Replace semantics: any new feedback supersedes the current one
/// (timer resets, pixmap re-renders). Auto-dismiss only fires in
/// release builds — debug builds keep the latest card on screen until
/// the next OCR run replaces it, so the developer has time to read
/// every per-item line.
///
/// We render the pixmap **exactly once** per state transition, then
/// only touch `set_ocr_alpha` per frame. Re-submitting the same
/// texture at 90 Hz caused visible flicker (SteamVR seemed to swap
/// in partial uploads) and was wasted work besides.
///
/// Overlay-submit / visibility / alpha failures are logged but never
/// propagated up: the OCR overlay is a developer-aid surface, not
/// load-bearing, and one bad `SetOverlayRaw` should not kill the
/// VR session and tear down the wishlist grid the user is actively
/// looking at.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn drive_ocr_overlay(
    session: &mut super::overlay::OverlaySession,
    feedback_rx: &Receiver<crate::gui::OcrFeedback>,
    state: &mut Option<OcrOverlayState>,
    frame_start: std::time::Instant,
    ocr_debug: bool,
    box_scan_active: bool,
    auto_dismiss: Duration,
    feedback_grid: bool,
) -> bool {
    use crate::gui::OcrFeedbackKind;

    // Drain everything queued so we don't lag if multiple transitions
    // arrived between frames. Only the most recent feedback matters for
    // the display. A non-`Processing` feedback means the pipeline
    // finished this tick — report that back (return value) so the auto
    // loop can release its single-flight latch and start the interval.
    let mut latest = None;
    while let Ok(fb) = feedback_rx.try_recv() {
        latest = Some(fb);
    }
    let mut terminal_consumed = false;
    if let Some(fb) = latest {
        terminal_consumed = !matches!(fb.kind, OcrFeedbackKind::Processing);
        *state = Some(OcrOverlayState {
            feedback: fb,
            visible_since: frame_start,
            submitted: false,
            last_alpha: 0.0,
        });
    }

    let Some(current) = state.as_mut() else {
        return terminal_consumed;
    };

    // `ocr_debug` and an active box-scan both pin the card: it sticks until the
    // next capture replaces it (debug: inspect alongside on-disk artifacts;
    // box scan: captures are deliberate and seconds apart, so the "series so
    // far" should stay up between trigger pulls and only resume fading once
    // the desktop Finish/Cancel leaves capture mode). Otherwise terminal kinds
    // fade after `auto_dismiss`; Processing kinds never auto-fade (replaced
    // when OCR finishes).
    let manual_dismiss = ocr_debug || box_scan_active;
    let processing = matches!(current.feedback.kind, OcrFeedbackKind::Processing);
    let age = frame_start.duration_since(current.visible_since);

    // First submit for this feedback: render once and push. (No auto-capture
    // banner — the guide box now shows capture state.)
    if !current.submitted {
        let pixmap = super::ocr_render::render(&current.feedback, false, feedback_grid);
        if let Err(e) = session.submit_ocr_rgba(pixmap.data(), pixmap.width(), pixmap.height()) {
            tracing::warn!(error = %e, "OCR overlay: submit failed (continuing)");
        }
        if let Err(e) = session.set_ocr_visible(true) {
            tracing::warn!(error = %e, "OCR overlay: set_visible failed");
        }
        current.submitted = true;
    }

    // Terminal kinds fade out after `auto_dismiss` unless debug mode
    // wants the card to stick.
    if !processing && !manual_dismiss && age >= auto_dismiss {
        if let Err(e) = session.set_ocr_alpha(0.0) {
            tracing::warn!(error = %e, "OCR overlay: set_alpha(0) failed");
        }
        if let Err(e) = session.set_ocr_visible(false) {
            tracing::warn!(error = %e, "OCR overlay: hide failed");
        }
        *state = None;
        return terminal_consumed;
    }

    // Per-frame alpha. Processing kinds and debug-mode terminals
    // stay at 1.0; non-debug terminals fade through the last
    // ~600 ms of their visible time.
    let target_alpha = if processing || manual_dismiss {
        1.0
    } else {
        let fade_tail = Duration::from_millis(600);
        let remaining = auto_dismiss.saturating_sub(age);
        if remaining < fade_tail {
            (remaining.as_secs_f32() / fade_tail.as_secs_f32()).clamp(0.0, 1.0)
        } else {
            1.0
        }
    };
    if (target_alpha - current.last_alpha).abs() > 1.0 / 256.0 {
        if let Err(e) = session.set_ocr_alpha(target_alpha) {
            tracing::warn!(error = %e, "OCR overlay: set_alpha failed");
        }
        current.last_alpha = target_alpha;
    }
    terminal_consumed
}

/// Per-tick hover detection: ray-cast from each controller, find whichever
/// cell the laser is currently pointing at (if any).
#[cfg(target_os = "windows")]
fn compute_hover(
    session: &super::overlay::OverlaySession,
    last_hits: &[super::render::CellHit],
    last_canvas: (u32, u32),
) -> Option<String> {
    use super::input::{hit_test, texcoord_to_pixel};
    let (cw, ch) = last_canvas;
    if cw == 0 || ch == 0 || last_hits.is_empty() {
        return None;
    }
    // Controllers typically live at indices 1+ (0 is the HMD). Scan a
    // generous range to handle multi-controller / index-shifted setups.
    for idx in 1..=8u32 {
        let device = openvr::TrackedDeviceIndex(idx);
        let Some((u, v)) = session.intersect_from_device(device) else {
            continue;
        };
        let (px, py) = texcoord_to_pixel(u, v, cw, ch);
        let hit = hit_test(last_hits, px, py);
        // Per-tick spam: downgraded to debug. The `hover ON/OFF/SWITCH` info
        // logs below already tell the story at transition granularity.
        tracing::debug!(
            device = idx,
            u,
            v,
            px,
            py,
            hit = ?hit.map(|h| h.item_id.as_str()),
            "ray intersect"
        );
        if let Some(hit) = hit {
            return Some(hit.item_id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_mode_starts_off_and_round_trips() {
        use crate::ocr::ScanTarget;

        let rt = Runtime::disconnected_for_test();
        // Always starts disarmed so a mode can't leak into a raid.
        assert_eq!(rt.capture_mode(), CaptureMode::Off);
        assert!(!rt.capture_active());

        // Arming a box/stash scan flips the mode and reports active.
        rt.set_capture_mode(CaptureMode::Box(ScanTarget::Stash));
        assert_eq!(rt.capture_mode(), CaptureMode::Box(ScanTarget::Stash));
        assert!(rt.capture_active());
        // Arming resets the displayed state to Ready.
        assert_eq!(rt.capture_state(), CaptureState::Ready);

        // Switching to hideout swaps cleanly.
        rt.set_capture_mode(CaptureMode::Hideout);
        assert_eq!(rt.capture_mode(), CaptureMode::Hideout);

        // Exiting restores the normal (Off) overlay.
        rt.exit_capture_mode();
        assert_eq!(rt.capture_mode(), CaptureMode::Off);
        assert!(!rt.capture_active());
    }
}
