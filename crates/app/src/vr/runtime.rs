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
    use super::input::Debouncer;
    use super::pose::{Visibility, VisibilityFsm};
    use super::render::CellHit;
    use openvr::system::EventInfo;
    use std::time::Instant;

    const TICK: Duration = Duration::from_millis(11); // ~90Hz

    let mut fsm = VisibilityFsm::new();
    // Re-render whenever either the wishlist (state.version) or the grid
    // shape (vr.grid_cols) changes — without the second component, dragging
    // the items-per-row slider would only take effect on the next click.
    let mut last_rendered: Option<(u64, u32)> = None;
    let mut last_hits: Vec<CellHit> = Vec::new();
    // Width/height of the most recently submitted pixmap, so the mouse
    // event mapping in `texcoord_to_pixel` reflects whatever shape we
    // actually drew (the canvas is no longer fixed at 1024x1024).
    let mut last_canvas: (u32, u32) = (0, 0);
    let mut fade_start: Option<Instant> = None;
    let mut was_visible = false;
    let mut debouncer = Debouncer::new();
    let mut event_buf: Vec<EventInfo> = Vec::with_capacity(8);

    loop {
        let frame_start = Instant::now();
        let vr = settings.read().vr.clone();
        session.apply_settings(&vr)?;

        let pitch = session.hmd_pitch_deg().unwrap_or(0.0);
        let dwell = Duration::from_millis(vr.show_dwell_ms);
        let visible_now = matches!(
            fsm.tick_with(
                pitch,
                frame_start,
                vr.show_pitch_deg,
                vr.hide_pitch_deg,
                dwell
            ),
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
            },
            &mut was_visible,
            &mut fade_start,
            &mut last_rendered,
            &mut last_hits,
            &mut last_canvas,
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
        if session.anchor_at_current_hmd()? {
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
    for ev in event_buf.drain(..) {
        let Event::MouseButtonDown(mouse) = ev.event else {
            continue;
        };
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
    match handle_click(state, last_hits, px, py, debouncer, frame_start) {
        ClickOutcome::Incremented { item_id, new_value } => {
            session.haptic_pulse(device_index, HAPTIC_INCREMENT_US);
            tracing::debug!(%item_id, new_value, "overlay click: +1");
        }
        ClickOutcome::Reset { item_id } => {
            session.haptic_pulse(device_index, HAPTIC_RESET_US);
            tracing::debug!(%item_id, "overlay click: cycle reset to 0");
        }
        ClickOutcome::Ignored => {}
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
