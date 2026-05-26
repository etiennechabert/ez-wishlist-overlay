//! VR background thread: owns the OpenXR session, drives pose → visibility,
//! renders the overlay swapchain, reports status to the GUI.
//!
//! On Windows this thread tries to attach to an OpenXR runtime (SteamVR
//! exposes the `XR_EXTX_overlay` extension we need), retries every 5s
//! while disconnected, and surfaces transitions via a shared [`VrStatus`].
//! On other targets it parks immediately at [`VrStatus::Unsupported`].

use crate::settings::Settings;
use crate::state::AppState;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VrStatus {
    /// Compile target has no OpenXR support (e.g. macOS, Linux).
    Unsupported,
    /// No OpenXR loader / runtime is installed on this machine.
    RuntimeNotInstalled,
    /// Runtime is installed but the worker can't attach (process not
    /// running, no scene app, overlay extension absent, etc.). Retries
    /// every 5 s.
    Disconnected,
    /// Worker is creating the XR instance + session right now.
    Connecting,
    /// Session is live and the overlay composition layer is ready.
    Connected,
    /// Last attach attempt produced a hard error. Worker keeps retrying.
    Error(String),
}

impl VrStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Unsupported => "VR: unavailable on this OS".into(),
            Self::RuntimeNotInstalled => "VR: no OpenXR runtime installed".into(),
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
        // OpenXR doesn't expose a cheap "is runtime installed" probe like
        // OpenVR did; we just attempt to init and let OverlaySession::init
        // produce the diagnostic-classed error.
        *status.write() = VrStatus::Connecting;
        let initial_width = settings.read().vr.width_meters;
        match OverlaySession::init(initial_width) {
            Ok(mut session) => {
                *status.write() = VrStatus::Connected;
                tracing::info!("VR overlay initialized");
                let lost = render_loop(&mut session, &state, &settings);
                if let Err(e) = lost {
                    tracing::warn!(error = %e, "VR session lost");
                }
                drop(session); // xrDestroyInstance via Drop.
                *status.write() = VrStatus::Disconnected;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::debug!(error = %msg, "VR init failed (no runtime / no scene app / no overlay ext)");
                *status.write() = VrStatus::Disconnected;
            }
        }
        std::thread::sleep(RETRY_DELAY);
    }
}

/// ~90Hz inner loop. Returns `Err` when the OpenXR runtime appears to have
/// gone away (session lost), prompting the outer loop to retry.
#[cfg(target_os = "windows")]
fn render_loop(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    settings: &Arc<RwLock<Settings>>,
) -> anyhow::Result<()> {
    use super::pose::{Visibility, VisibilityFsm};
    use super::render::CellHit;
    use std::time::Instant;

    const TICK: Duration = Duration::from_millis(11); // ~90Hz

    let mut fsm = VisibilityFsm::new();
    let mut last_rendered: Option<(u64, u32)> = None;
    let mut last_hits: Vec<CellHit> = Vec::new();
    let mut last_canvas: (u32, u32) = (0, 0);
    let mut fade_start: Option<Instant> = None;
    let mut was_visible = false;
    // TODO(openxr): wire the OpenXR action-system click events into
    // `super::input::handle_click` so trigger-on-tile increments the
    // collected counter (the post-event logic is OpenXR-agnostic and
    // ready to go; only the event-source side needs porting).

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
                height_offset_m: vr.height_offset_m,
            },
            &mut was_visible,
            &mut fade_start,
            &mut last_rendered,
            &mut last_hits,
            &mut last_canvas,
        )?;

        // TODO(openxr): drain `xrPollEvent` for session state transitions
        // + input-action snapshots, then hit-test + dispatch like the
        // OpenVR version did.
        session.drain_events();

        if visible_now {
            let current_version = state.read().version;
            let sig = (current_version, vr.grid_cols);
            if last_rendered != Some(sig) {
                render_and_submit(
                    session,
                    state,
                    vr.grid_cols,
                    &mut last_rendered,
                    &mut last_hits,
                    &mut last_canvas,
                )?;
            }
            apply_fade_in(session, &mut fade_start, frame_start)?;
        }

        session.heartbeat()?;

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
    last_rendered: &mut Option<(u64, u32)>,
    last_hits: &mut Vec<super::render::CellHit>,
    last_canvas: &mut (u32, u32),
) -> anyhow::Result<()> {
    if t.visible_now && !*was_visible {
        if session.anchor_at_current_hmd(t.height_offset_m)? {
            // Force first render before showing so the user never sees
            // a stale or empty texture.
            render_and_submit(
                session,
                state,
                t.grid_cols,
                last_rendered,
                last_hits,
                last_canvas,
            )?;
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

// TODO(openxr): port the click → cycle_collected → haptic flow here. The
// pure post-event logic in `super::input::handle_click` is unchanged and
// already covered by unit tests; only the OpenXR side (action set, action
// state polling, haptic action firing via xrApplyHapticFeedback) needs to
// be added once the session lifecycle and graphics binding are in place.

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

#[cfg(target_os = "windows")]
fn render_and_submit(
    session: &mut super::overlay::OverlaySession,
    state: &Arc<RwLock<AppState>>,
    grid_cols: u32,
    last_rendered: &mut Option<(u64, u32)>,
    last_hits: &mut Vec<super::render::CellHit>,
    last_canvas: &mut (u32, u32),
) -> anyhow::Result<()> {
    use super::render;
    use crate::assets;

    let (items, version) = {
        let st = state.read();
        (st.active_items(), st.version)
    };
    let (pixmap, hits) = render::render(&items, grid_cols, assets::read_icon);
    session.submit_rgba(pixmap.data(), pixmap.width(), pixmap.height())?;
    *last_rendered = Some((version, grid_cols));
    *last_canvas = (pixmap.width(), pixmap.height());
    *last_hits = hits;
    Ok(())
}
