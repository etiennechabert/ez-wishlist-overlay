//! Thin safe wrapper over the [`openvr`] crate: just enough surface to attach
//! to SteamVR, place the overlay relative to the HMD, query head pose, and
//! push pixel buffers in. Click input lands in Phase 4.
//!
//! This module is `cfg(target_os = "windows")` because `openvr_sys` builds
//! Valve's C++ SDK via cmake + bindgen and only does that on Windows in our
//! workspace.

use crate::settings::VrSettings;
use anyhow::{Context as _, Result};
use openvr::system::EventInfo;
use openvr::{
    init, tracked_device_index, ApplicationType, Context, TrackedDeviceIndex,
    TrackingUniverseOrigin,
};

const OVERLAY_KEY: &str = "com.etienneb.ez-wishlist-overlay.main\0";
const OVERLAY_NAME: &str = "EZ Wishlist Overlay\0";

/// Owns the OpenVR runtime + a single overlay handle for the program's
/// lifetime. Drop runs `VR_Shutdown` via `Context::drop`.
pub struct OverlaySession {
    handle: openvr::overlay::OverlayHandle,
    ctx: Context,
    /// Last width we pushed via SetOverlayWidthInMeters. Used by
    /// `apply_settings` to skip redundant calls.
    last_width: f32,
    /// Last alpha pushed via SetOverlayAlpha. Skips redundant calls
    /// inside the fade loop.
    last_alpha: f32,
    /// `true` once the HMD-relative transform has been pushed. The
    /// transform sticks across show/hide, so we only need to set it once
    /// per session unless tuning constants change.
    transform_applied: bool,
}

impl OverlaySession {
    pub fn init(width_meters: f32) -> Result<Self> {
        // SAFETY: VR_Init is documented as one-per-process. The runtime
        // thread is the sole caller and only re-enters after the previous
        // `Context` has been dropped (which calls VR_Shutdown).
        let ctx = unsafe { init(ApplicationType::Overlay) }
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("VR_Init failed — is SteamVR running?")?;

        let mut overlay = ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("get IVROverlay interface")?;

        let handle = overlay
            .create_overlay(OVERLAY_KEY, OVERLAY_NAME)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("CreateOverlay")?;

        overlay
            .set_width(handle, width_meters)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayWidthInMeters")?;

        // Hide by default; pose-driven show takes over.
        overlay
            .set_visibility(handle, false)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("HideOverlay")?;

        // Start fully transparent so the first fade-in goes 0 → 1.
        overlay
            .set_opacity(handle, 0.0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha")?;

        Ok(Self {
            handle,
            ctx,
            last_width: width_meters,
            last_alpha: 0.0,
            transform_applied: false,
        })
    }

    /// Cheap probe used by the runtime loop to detect that SteamVR vanished.
    /// Any operation that hits the IVROverlay fn-table works; we use the
    /// dashboard query because it has no side effects and is documented as
    /// safe to call from an overlay app.
    pub fn heartbeat(&self) -> Result<()> {
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        let _ = overlay.is_dashboard_visible();
        Ok(())
    }

    /// Push width changes the user made since last tick.
    pub fn apply_settings(&mut self, desired: &VrSettings) -> Result<()> {
        if (desired.width_meters - self.last_width).abs() > f32::EPSILON {
            let mut overlay = self
                .ctx
                .overlay()
                .map_err(|e| anyhow::anyhow!("{e:?}"))
                .context("IVROverlay interface")?;
            overlay
                .set_width(self.handle, desired.width_meters)
                .map_err(|e| anyhow::anyhow!("{e:?}"))
                .context("SetOverlayWidthInMeters")?;
            self.last_width = desired.width_meters;
            tracing::debug!(width = desired.width_meters, "applied new overlay width");
        }
        Ok(())
    }

    /// Read the HMD's current pose and extract pitch (degrees up from
    /// horizontal). Returns `None` if the HMD pose is currently invalid
    /// (e.g. tracking lost, headset off the head).
    pub fn hmd_pitch_deg(&self) -> Option<f32> {
        let system = self.ctx.system().ok()?;
        // 0.0 = "right now". Photons-ahead prediction is for compositor-side
        // submission timing; for visibility we want the current head pose.
        let poses = system.device_to_absolute_tracking_pose(TrackingUniverseOrigin::Standing, 0.0);
        let hmd = &poses[tracked_device_index::HMD.0 as usize];
        if !hmd.pose_is_valid() {
            return None;
        }
        Some(super::pose::pitch_from_hmd_matrix(
            hmd.device_to_absolute_tracking(),
        ))
    }

    /// Place the overlay relative to the HMD using the constants from
    /// [`crate::vr::anchor`]. Idempotent — only pushes the transform on the
    /// first call.
    pub fn ensure_anchor(&mut self) -> Result<()> {
        if self.transform_applied {
            return Ok(());
        }
        let m = super::anchor::hmd_relative_transform();
        let matrix = openvr::pose::Matrix3x4(m);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_transform_tracked_device_relative(self.handle, tracked_device_index::HMD, &matrix)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayTransformTrackedDeviceRelative")?;
        self.transform_applied = true;
        tracing::debug!("anchor transform applied (HMD-relative)");
        Ok(())
    }

    pub fn set_visible(&mut self, visible: bool) -> Result<()> {
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_visibility(self.handle, visible)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("Show/HideOverlay")
    }

    pub fn set_alpha(&mut self, alpha: f32) -> Result<()> {
        let a = alpha.clamp(0.0, 1.0);
        if (a - self.last_alpha).abs() < 1.0 / 256.0 {
            return Ok(());
        }
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_opacity(self.handle, a)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha")?;
        self.last_alpha = a;
        Ok(())
    }

    /// Push an RGBA8 pixel buffer to the overlay. `bytes.len()` must equal
    /// `width * height * 4`.
    pub fn submit_rgba(&mut self, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        debug_assert_eq!(bytes.len(), (width * height * 4) as usize);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_raw_data(self.handle, bytes, width as usize, height as usize, 4)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayRaw")
    }

    pub fn handle(&self) -> openvr::overlay::OverlayHandle {
        self.handle
    }

    /// Drain pending VR events into the provided buffer. Cheap — typically
    /// 0–2 events per ~11 ms tick. Caller filters by event type.
    pub fn drain_events(&self, out: &mut Vec<EventInfo>) {
        out.clear();
        let Ok(system) = self.ctx.system() else {
            return;
        };
        while let Some(ev) = system.poll_next_event() {
            out.push(ev);
            if out.len() > 64 {
                // Safety cap: if something's misbehaving and flooding us
                // we'd rather drop than spin the loop forever.
                break;
            }
        }
    }

    /// Fire a haptic pulse on a controller. `duration_us` is microseconds;
    /// OpenVR rejects values > 3999. Axis 0 is the conventional haptic
    /// axis for legacy controllers (Vive wand, Quest Touch via OpenVR).
    pub fn haptic_pulse(&self, device: TrackedDeviceIndex, duration_us: u16) {
        let Ok(system) = self.ctx.system() else {
            return;
        };
        system.trigger_haptic_pulse(device, 0, duration_us.min(3_999));
    }
}
