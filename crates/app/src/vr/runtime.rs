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
use crate::state::AppState;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Subdirectory under `PersistPaths::debug_dir` where captured mirror-texture
/// PNGs (and their OCR sidecars) are written. Lives inside the per-session
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
    /// Ephemeral auto-capture loop switch. Not persisted — always starts
    /// `false` so the looping OCR can't be left running into a raid. The
    /// VR render loop polls this each tick; the GUI flips it via
    /// [`Runtime::set_auto_capture`].
    auto_capture: Arc<AtomicBool>,
    /// Ephemeral box-scan mode switch. While `true`, SPACE captures are tagged
    /// [`crate::ocr::JobKind::BoxScan`] and the auto-capture loop is suppressed
    /// (the two modes are mutually exclusive). Not persisted — always starts
    /// `false`. The GUI flips it via [`Runtime::set_box_scan_mode`].
    box_scan_mode: Arc<AtomicBool>,
    _join: std::thread::JoinHandle<()>,
}

#[derive(Clone, Debug)]
pub enum CaptureResult {
    /// Capture succeeded and was written to disk at this path. Only
    /// produced when `settings.ocr_debug` is on, because the runtime
    /// otherwise skips the PNG write entirely (see [`Runtime::spawn`]).
    Ok(PathBuf),
    /// Capture succeeded but no PNG was written — the default fast
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
    /// already-decoded mirror-texture bitmap, plus the on-disk PNG
    /// path when `settings.ocr_debug` is on (otherwise `None`, and
    /// no PNG is written at all). The downstream OCR worker thread
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
        let auto_capture = Arc::new(AtomicBool::new(false));
        let auto_capture_worker = auto_capture.clone();
        let box_scan_mode = Arc::new(AtomicBool::new(false));
        let box_scan_mode_worker = box_scan_mode.clone();
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
                    auto_capture_worker,
                    box_scan_mode_worker,
                )
            })
            .expect("spawn VR thread");
        Self {
            status,
            capture_tx,
            last_capture,
            auto_capture,
            box_scan_mode,
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
            auto_capture: Arc::new(AtomicBool::new(false)),
            box_scan_mode: Arc::new(AtomicBool::new(false)),
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

    /// Enable/disable the auto-capture loop. Ephemeral — never persisted,
    /// so it always starts OFF on launch. The VR render loop polls this
    /// each tick: flipping it on fires a capture right away, off stops
    /// the loop after any in-flight read finishes.
    pub fn set_auto_capture(&self, on: bool) {
        self.auto_capture.store(on, Ordering::Relaxed);
    }

    pub fn auto_capture_enabled(&self) -> bool {
        self.auto_capture.load(Ordering::Relaxed)
    }

    /// Enter/leave box-scan mode. Ephemeral — never persisted. While on, SPACE
    /// captures feed the box-scan session and the auto-capture loop is
    /// suppressed. The Containers tab flips this around a "Scan box" session.
    pub fn set_box_scan_mode(&self, on: bool) {
        self.box_scan_mode.store(on, Ordering::Relaxed);
    }

    pub fn box_scan_enabled(&self) -> bool {
        self.box_scan_mode.load(Ordering::Relaxed)
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
    _auto_capture: Arc<AtomicBool>,
    _box_scan_mode: Arc<AtomicBool>,
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
    auto_capture: Arc<AtomicBool>,
    box_scan_mode: Arc<AtomicBool>,
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
                    &auto_capture,
                    &box_scan_mode,
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
    auto_capture: &Arc<AtomicBool>,
    box_scan_mode: &Arc<AtomicBool>,
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

    // Auto-capture loop bookkeeping. `capture_in_flight` is the
    // single-flight latch — set when the auto loop dispatches a capture,
    // cleared when the OCR worker reports a terminal result. Pacing is by
    // comparing `frame_start` against `last_auto_done` (no sleeps, so the
    // overlay never freezes). `auto_dispatched_at` backs a watchdog that
    // frees the latch if a dispatched job never reports back.
    let mut capture_in_flight = false;
    let mut last_auto_done: Option<Instant> = None;
    let mut auto_dispatched_at: Option<Instant> = None;
    let mut prev_auto_on = false;

    loop {
        let frame_start = Instant::now();
        let vr = settings.read().vr.clone();
        session.apply_settings(&vr)?;

        let pitch = session.hmd_pitch_deg().unwrap_or(0.0);
        let visible_now = matches!(
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

        session.drain_events(&mut event_buf);
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

        // Manual capture requests (SPACE hotkey). Drain the queue and grab
        // once — multiple presses within a single ~11 ms frame can only be
        // accidental, so coalescing them is correct even in box-scan mode
        // (distinct scroll positions are always whole frames apart). `beep =
        // true` gives audible in-headset confirmation. The capture work itself
        // lives in `capture_and_forward` (shared with the auto loop below).
        let mut manual_requested = false;
        while capture_rx.try_recv().is_ok() {
            manual_requested = true;
        }
        if manual_requested {
            capture_and_forward(
                session,
                settings,
                paths,
                ocr_tx,
                last_capture,
                &mut ocr_state,
                true,
                box_scan_mode.load(Ordering::Relaxed),
            );
        }

        // Auto-capture loop. Entirely non-blocking: paced by `Instant`
        // comparisons in this ~90 Hz loop, single-flight via
        // `capture_in_flight`, and silent (no per-loop beep). The mode
        // flag is ephemeral, so it always starts off (see
        // `Runtime::set_auto_capture`). Suppressed entirely while a box scan is
        // active — the two capture modes are mutually exclusive.
        let auto_on =
            auto_capture.load(Ordering::Relaxed) && !box_scan_mode.load(Ordering::Relaxed);
        if auto_on && !prev_auto_on {
            // Rising edge: make the first capture eligible immediately.
            last_auto_done = None;
        }
        if !auto_on && prev_auto_on {
            // Falling edge: drop any still-running placeholder card so a
            // "Reading panel…" card doesn't hang on screen after the loop
            // stops (Processing never auto-dismisses on its own).
            if matches!(
                ocr_state.as_ref().map(|s| &s.feedback.kind),
                Some(crate::gui::OcrFeedbackKind::Processing)
            ) {
                let _ = session.set_ocr_alpha(0.0);
                let _ = session.set_ocr_visible(false);
                ocr_state = None;
            }
        }
        prev_auto_on = auto_on;
        if auto_on {
            let (ocr_enabled, interval) = {
                let s = settings.read();
                (
                    s.ocr_enabled,
                    Duration::from_secs(s.auto_capture_interval_secs as u64),
                )
            };
            let due = match last_auto_done {
                Some(t) => frame_start.duration_since(t) >= interval,
                None => true,
            };
            if ocr_enabled && !capture_in_flight && due {
                if capture_and_forward(
                    session,
                    settings,
                    paths,
                    ocr_tx,
                    last_capture,
                    &mut ocr_state,
                    false,
                    false, // auto-capture is upgrade-panel only
                ) {
                    capture_in_flight = true;
                    auto_dispatched_at = Some(frame_start);
                } else {
                    // Capture failed (no job dispatched) — wait one
                    // interval before retrying instead of spinning.
                    last_auto_done = Some(frame_start);
                }
            }
            // Watchdog: if a dispatched read never produced terminal
            // feedback (e.g. OCR was disabled mid-flight so the worker
            // skipped it), free the latch so the loop can't wedge.
            if let Some(t) = auto_dispatched_at {
                if capture_in_flight && frame_start.duration_since(t) > Duration::from_secs(30) {
                    tracing::warn!(
                        "auto-capture: no OCR result within 30 s — clearing in-flight latch"
                    );
                    capture_in_flight = false;
                    auto_dispatched_at = None;
                    last_auto_done = Some(frame_start);
                }
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
            let (ocr_debug, dismiss) = {
                let s = settings.read();
                (
                    s.ocr_debug,
                    Duration::from_secs(s.ocr_dismiss_seconds as u64),
                )
            };
            let terminal = drive_ocr_overlay(
                session,
                ocr_feedback_rx,
                &mut ocr_state,
                frame_start,
                ocr_debug,
                auto_on,
                dismiss,
            );
            if terminal {
                // OCR finished — release the single-flight latch and start
                // the inter-read interval countdown.
                capture_in_flight = false;
                auto_dispatched_at = None;
                last_auto_done = Some(frame_start);
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
/// naming convention (`YYYYMMDDhhmmss_<nanos>.png`) so sorting it next to
/// Steam's JPEGs in a file browser feels natural.
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
        .join(format!("{stamp}_{nanos:09}.png"))
}

/// Grab one compositor-mirror frame and hand it to the OCR worker.
/// Shared by the manual SPACE path and the auto-capture loop.
///
/// Retires any visible OCR card first: the card is a real SteamVR
/// overlay composited into the eye buffers, so leaving it up would bake
/// it into the screenshot and the next OCR pass would read itself over
/// the panel. PNG-on-disk is gated on `settings.ocr_debug` (off by
/// default → no disk round-trip, ~2-3 s OCR instead of ~6-7 s). Capture
/// errors are surfaced to the GUI via `last_capture` rather than
/// aborting the render loop.
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
/// — dropping one would leave a gap in the stitched list.
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
    box_scan: bool,
) -> bool {
    if ocr_state.is_some() {
        if let Err(e) = session.set_ocr_alpha(0.0) {
            tracing::warn!(error = %e, "OCR overlay: pre-capture clear-alpha failed");
        }
        if let Err(e) = session.set_ocr_visible(false) {
            tracing::warn!(error = %e, "OCR overlay: pre-capture hide failed");
        }
        *ocr_state = None;
    }
    let (capture_eye, trace, ocr_debug) = {
        let s = settings.read();
        (s.capture_eye.into(), s.ocr_capture_trace, s.ocr_debug)
    };
    let png_path = ocr_debug.then(|| next_screenshot_path(paths));
    let capture_result = match &png_path {
        Some(path) => session.capture_screenshot_to_png(path, capture_eye, trace),
        None => session.capture_screenshot(capture_eye, trace),
    };
    let (result, dispatched) = match capture_result {
        Ok(img) => {
            let job = OcrJob {
                image: img,
                source_path: png_path.clone(),
                kind: if box_scan {
                    crate::ocr::JobKind::BoxScan
                } else {
                    crate::ocr::JobKind::UpgradePanel
                },
            };
            // Upgrade captures: best-effort `try_send` — if the worker is busy
            // we drop this shot (it only keeps the latest anyway) so the render
            // loop never blocks. Box-scan captures: blocking `send`, because a
            // dropped scroll position would gap the stitched list. Safe to
            // block here — box captures are deliberate and seconds apart.
            if box_scan {
                let _ = ocr_tx.send(job);
            } else {
                let _ = ocr_tx.try_send(job);
            }
            if beep {
                super::capture::play_capture_done_beep(true);
            }
            let r = match png_path {
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
fn drive_ocr_overlay(
    session: &mut super::overlay::OverlaySession,
    feedback_rx: &Receiver<crate::gui::OcrFeedback>,
    state: &mut Option<OcrOverlayState>,
    frame_start: std::time::Instant,
    ocr_debug: bool,
    auto_on: bool,
    auto_dismiss: Duration,
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

    // While the auto-capture loop is on, keep a card up at all times so
    // the head-locked "AUTO-CAPTURE ON" banner is an un-ignorable
    // reminder. Seed a placeholder whenever nothing else is showing
    // (just toggled on, or a prior card was cleared); the next real
    // feedback replaces it.
    if state.is_none() && auto_on {
        *state = Some(OcrOverlayState {
            feedback: crate::gui::OcrFeedback::processing(),
            visible_since: frame_start,
            submitted: false,
            last_alpha: 0.0,
        });
    }

    let Some(current) = state.as_mut() else {
        return terminal_consumed;
    };

    // `ocr_debug` and auto-capture both pin the card: it sticks until the
    // next capture replaces it (debug: inspect alongside on-disk
    // artifacts; auto: the banner must stay up the whole session).
    // Otherwise terminal kinds fade after `auto_dismiss`. Processing
    // kinds never auto-fade — they're replaced when OCR finishes.
    let manual_dismiss = ocr_debug || auto_on;
    let processing = matches!(current.feedback.kind, OcrFeedbackKind::Processing);
    let age = frame_start.duration_since(current.visible_since);

    // First submit for this feedback: render once and push.
    if !current.submitted {
        let pixmap = super::ocr_render::render(&current.feedback, auto_on);
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
