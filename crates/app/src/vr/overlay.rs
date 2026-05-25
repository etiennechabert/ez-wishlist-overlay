//! Thin safe wrapper over the [`openvr`] crate: just enough surface to attach
//! to SteamVR and create our overlay handle. Pose polling and pixel
//! submission land in later phases.
//!
//! This module is `cfg(target_os = "windows")` because `openvr_sys` builds
//! Valve's C++ SDK via cmake + bindgen and only does that on Windows in our
//! workspace.

use crate::settings::VrSettings;
use anyhow::{Context as _, Result};
use openvr::{init, ApplicationType, Context};

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

        // Hide by default; pose-driven show comes next phase.
        overlay
            .set_visibility(handle, false)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("HideOverlay")?;

        Ok(Self {
            handle,
            ctx,
            last_width: width_meters,
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

    /// Push any settings changes the user made since last tick.
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
        // show/hide pitch + dwell aren't read yet (pose loop lands in next PR).
        Ok(())
    }

    #[allow(dead_code)] // wired in by the next phase's render loop
    pub fn handle(&self) -> openvr::overlay::OverlayHandle {
        self.handle
    }
}
