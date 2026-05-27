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
    // The data-rendered pixmap is cached so we don't re-decode all icons on
    // every frame — only re-render when version/grid_cols changes. Hover
    // border is a cheap copy+stroke applied per frame on top of the cache.
    let mut clean_pixmap: Option<tiny_skia::Pixmap> = None;
    let mut clean_sig: Option<(u64, u32)> = None;
    let mut last_hits: Vec<CellHit> = Vec::new();
    let mut last_canvas: (u32, u32) = (0, 0);
    let mut fade_start: Option<Instant> = None;
    let mut was_visible = false;
    let mut debouncer = Debouncer::new();
    let mut event_buf: Vec<EventInfo> = Vec::with_capacity(8);
    let mut current_hover: Option<String> = None;

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

        if visible_now {
            let current_version = state.read().version;
            let need_data_render = clean_sig != Some((current_version, vr.grid_cols));
            if need_data_render {
                render_data(
                    state,
                    vr.grid_cols,
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
    clean_sig: &mut Option<(u64, u32)>,
    last_hits: &mut Vec<super::render::CellHit>,
    last_canvas: &mut (u32, u32),
    current_hover: &mut Option<String>,
) -> anyhow::Result<()> {
    if t.visible_now && !*was_visible {
        if session.anchor_at_current_hmd(t.height_offset_m)? {
            render_data(
                state,
                t.grid_cols,
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

/// Re-render the wishlist grid into `clean_pixmap` (no hover overlay).
/// Slow path: decodes icons, lays out cells. Called only when the data
/// changes (wishlist version bump, grid-cols change).
#[cfg(target_os = "windows")]
fn render_data(
    state: &Arc<RwLock<AppState>>,
    grid_cols: u32,
    clean_pixmap: &mut Option<tiny_skia::Pixmap>,
    clean_sig: &mut Option<(u64, u32)>,
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
    *last_canvas = (pixmap.width(), pixmap.height());
    *last_hits = hits;
    *clean_pixmap = Some(pixmap);
    *clean_sig = Some((version, grid_cols));
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
