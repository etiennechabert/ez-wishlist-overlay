// The vr module is wired in across Phase 3 / Phase 4. The pose FSM and CPU
// renderer are unit-tested today but not yet called from the bin's hot path
// — `dead_code` here is expected.
#![allow(dead_code)]

//! VR overlay subsystem.
//!
//! Module map:
//!   - [`render`]   — CPU rasterizer for the icon grid (DONE; unit-tested).
//!   - [`pose`]     — pitch hysteresis state machine (DONE; unit-tested).
//!   - [`runtime`]  — background thread + status surface (Phase 3 PR 1).
//!   - `overlay`    — OpenVR init + handle (Windows-only, Phase 3 PR 1).
//!   - `input`      — overlay mouse events → grid hit-test (TODO Phase 4).
//!
//! On non-Windows targets `overlay` doesn't exist and `runtime` parks at
//! [`runtime::VrStatus::Unsupported`], so the desktop app stays fully
//! functional for UI iteration on macOS/Linux.

pub mod anchor;
pub mod input;
pub mod pose;
pub mod render;
pub mod runtime;
pub mod text;

#[cfg(target_os = "windows")]
mod overlay;

pub use runtime::Runtime;
