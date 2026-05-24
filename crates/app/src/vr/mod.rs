// The vr module is wired in by Phase 3 / Phase 4. The pose FSM and CPU
// renderer are unit-tested today but not yet called from the bin's hot path
// — `dead_code` here is expected.
#![allow(dead_code)]

//! VR overlay subsystem (Phase 3 stub).
//!
//! The desktop app is fully usable without VR — this module compiles to
//! no-ops until an OpenVR binding is wired in. The structure mirrors the
//! sections in SPEC.md §7 so Phase 3 just fills in the actual FFI calls.
//!
//! Module map:
//!   - [`render`]   — CPU rasterizer for the icon grid (DONE; unit-tested).
//!   - [`pose`]     — pitch hysteresis state machine (DONE; unit-tested).
//!   - `overlay`    — OpenVR init + texture submission        (TODO Phase 3).
//!   - `input`      — overlay mouse events → grid hit-test    (TODO Phase 4).
//!
//! Picking an OpenVR binding is deferred to Phase 3 — the candidates as of
//! this writing are `ovr_overlay` (very sparse on crates.io), the older
//! `openvr` crate, or raw bindings via `openvr-sys` / `bindgen`. The eventual
//! wrapper should expose just:
//!
//! ```text
//! init() -> Result<VrSession>
//! VrSession::poll_pose() -> HmdPose
//! VrSession::submit_texture(rgba: &[u8], w: u32, h: u32)
//! VrSession::poll_events() -> Vec<OverlayEvent>
//! VrSession::trigger_haptic(controller: ControllerId, pattern: HapticPattern)
//! VrSession::set_alpha(alpha: f32)
//! ```

pub mod pose;
pub mod render;
