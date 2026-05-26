// The vr module is wired in across Phase 3 / Phase 4. The pose FSM and CPU
// renderer are unit-tested today but not yet called from the bin's hot path
// — `dead_code` here is expected.
#![allow(dead_code)]

//! VR overlay subsystem.
//!
//! Module map:
//!   - [`render`]   — CPU rasterizer for the icon grid (unit-tested).
//!   - [`pose`]     — pitch hysteresis state machine (unit-tested).
//!   - [`runtime`]  — background thread + status surface.
//!   - `overlay`    — OpenXR session + composition layer (Windows-only).
//!   - [`input`]    — click hit-test + cycle logic (OpenXR-agnostic; the
//!                    event-source plumbing still needs porting in
//!                    `runtime.rs` — see `TODO(openxr)`).
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
