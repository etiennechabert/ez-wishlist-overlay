//! VR background thread: owns the OpenVR session, drives pose → visibility,
//! renders the overlay texture, reports status to the GUI.
//!
//! On Windows this thread tries to attach to SteamVR, retries every 5s while
//! disconnected, and surfaces transitions via a shared [`VrStatus`]. On other
//! targets it parks immediately at [`VrStatus::Unsupported`] — Phase 1+2 of
//! the desktop app are platform-agnostic and we don't want a broken VR layer
//! to bleed into macOS/Linux iteration builds.

use crate::settings::Settings;
use crate::state::AppState;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_secs(5);

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
    _join: std::thread::JoinHandle<()>,
}

impl Runtime {
    pub fn spawn(state: Arc<RwLock<AppState>>, settings: Arc<RwLock<Settings>>) -> Self {
        let initial = if cfg!(target_os = "windows") {
            VrStatus::Connecting
        } else {
            VrStatus::Unsupported
        };
        let status = Arc::new(RwLock::new(initial));
        let status_writer = status.clone();
        let join = std::thread::Builder::new()
            .name("ez-wishlist-vr".into())
            .spawn(move || run(state, settings, status_writer))
            .expect("spawn VR thread");
        Self {
            status,
            _join: join,
        }
    }

    pub fn status(&self) -> VrStatus {
        self.status.read().clone()
    }
}

#[cfg(not(target_os = "windows"))]
fn run(
    _state: Arc<RwLock<AppState>>,
    _settings: Arc<RwLock<Settings>>,
    status: Arc<RwLock<VrStatus>>,
) {
    // Status is already Unsupported. Nothing else to do — park the thread.
    *status.write() = VrStatus::Unsupported;
    tracing::info!("VR worker idle: no OpenVR support on this target");
}

#[cfg(target_os = "windows")]
fn run(
    state: Arc<RwLock<AppState>>,
    settings: Arc<RwLock<Settings>>,
    status: Arc<RwLock<VrStatus>>,
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
                let lost = render_loop(&mut session, &state, &settings);
                if let Err(e) = lost {
                    tracing::warn!(error = %e, "VR session lost");
                }
                drop(session); // VR_Shutdown via Context Drop.
                *status.write() = VrStatus::Disconnected;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::debug!(error = %msg, "VR init failed (SteamVR likely not running)");
                *status.write() = VrStatus::Disconnected;
            }
        }
        std::thread::sleep(RETRY_DELAY);
    }
}

/// ~90Hz inner loop. Returns `Err` when SteamVR appears to have gone away
/// (any OpenVR call returning an error), prompting the outer loop to retry.
#[cfg(target_os = "windows")]
fn render_loop(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    settings: &Arc<RwLock<Settings>>,
) -> anyhow::Result<()> {
    use super::input::{
        handle_click, texcoord_to_pixel, ClickOutcome, Debouncer, HAPTIC_INCREMENT_US,
        HAPTIC_RESET_US,
    };
    use super::pose::{Visibility, VisibilityFsm};
    use super::render::{CellHit, CANVAS_PX};
    use openvr::system::{Event, EventInfo};
    use std::time::Instant;

    const TICK: Duration = Duration::from_millis(11); // ~90Hz
    const FADE: Duration = Duration::from_millis(150);
    /// EVRMouseButton::Left in the OpenVR mouse-event button bitfield.
    const MOUSE_BUTTON_LEFT: u32 = 1;

    let mut fsm = VisibilityFsm::new();
    let mut last_rendered_version: Option<u64> = None;
    let mut last_hits: Vec<CellHit> = Vec::new();
    let mut fade_start: Option<Instant> = None;
    let mut was_visible = false;
    let mut debouncer = Debouncer::new();
    let mut event_buf: Vec<EventInfo> = Vec::with_capacity(8);

    loop {
        let frame_start = Instant::now();
        let vr = settings.read().vr.clone();
        session.apply_settings(&vr)?;

        // Pose → FSM.
        let pitch = session.hmd_pitch_deg().unwrap_or(0.0);
        let dwell = Duration::from_millis(vr.show_dwell_ms);
        let vis = fsm.tick_with(
            pitch,
            frame_start,
            vr.show_pitch_deg,
            vr.hide_pitch_deg,
            dwell,
        );

        let visible_now = matches!(vis, Visibility::Visible);

        // Transitions.
        if visible_now && !was_visible {
            if session.anchor_at_current_hmd()? {
                // Force first render before showing so the user never sees
                // a stale or empty texture.
                render_and_submit(session, state, &mut last_rendered_version, &mut last_hits)?;
                session.set_alpha(0.0)?;
                session.set_visible(true)?;
                fade_start = Some(frame_start);
                was_visible = true;
                tracing::debug!(pitch, "overlay: fade in");
            } else {
                tracing::debug!("overlay show deferred: HMD pose invalid");
            }
        } else if !visible_now && was_visible {
            session.set_visible(false)?;
            session.set_alpha(0.0)?;
            fade_start = None;
            was_visible = false;
            tracing::debug!(pitch, "overlay: hide");
        }

        // Drain input. We poll even while hidden so the queue doesn't
        // back up across hide/show cycles; clicks while hidden are
        // ignored by hit_test against an empty `last_hits`.
        session.drain_events(&mut event_buf);
        for ev in event_buf.drain(..) {
            if let Event::MouseButtonDown(mouse) = ev.event {
                if mouse.button & MOUSE_BUTTON_LEFT == 0 {
                    continue;
                }
                if !visible_now {
                    continue; // ignore clicks during fade or hidden
                }
                let (px, py) = texcoord_to_pixel(mouse.position.0, mouse.position.1, CANVAS_PX);
                match handle_click(
                    state,
                    &last_hits,
                    px,
                    py,
                    &mut debouncer,
                    frame_start,
                    vr.tentative_overlay_edits,
                ) {
                    ClickOutcome::Incremented { item_id, new_value } => {
                        session.haptic_pulse(ev.tracked_device_index, HAPTIC_INCREMENT_US);
                        tracing::debug!(%item_id, new_value, "overlay click: +1");
                    }
                    ClickOutcome::Reset { item_id } => {
                        session.haptic_pulse(ev.tracked_device_index, HAPTIC_RESET_US);
                        tracing::debug!(%item_id, "overlay click: cycle reset to 0");
                    }
                    ClickOutcome::Ignored => {}
                }
            }
        }

        if visible_now {
            // Re-render on state-change (including ones our own clicks
            // just produced).
            let current_version = state.read().version;
            if last_rendered_version != Some(current_version) {
                render_and_submit(session, state, &mut last_rendered_version, &mut last_hits)?;
            }

            // Fade-in animation.
            if let Some(t0) = fade_start {
                let elapsed = frame_start.duration_since(t0);
                let alpha = (elapsed.as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
                session.set_alpha(alpha)?;
                if alpha >= 1.0 {
                    fade_start = None;
                }
            } else {
                session.set_alpha(1.0)?;
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
fn render_and_submit(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    last_rendered_version: &mut Option<u64>,
    last_hits: &mut Vec<super::render::CellHit>,
) -> anyhow::Result<()> {
    use super::render;
    use crate::assets;

    let (items, version) = {
        let st = state.read();
        (st.active_items(), st.version)
    };
    let (pixmap, hits) = render::render(&items, assets::read_icon);
    session.submit_rgba(pixmap.data(), pixmap.width(), pixmap.height())?;
    *last_rendered_version = Some(version);
    *last_hits = hits;
    Ok(())
}
