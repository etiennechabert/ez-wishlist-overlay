//! OpenXR overlay session.
//!
//! Connects to the active OpenXR runtime, advertises ourselves as a
//! `XR_EXTX_overlay` overlay app (so we sit on top of whatever scene app
//! is running), creates the world-anchored composition layer, and exposes
//! the lifecycle hooks the [`super::runtime`] thread drives every tick.
//!
//! This module is `cfg(target_os = "windows")` for now because we'll be
//! pairing the OpenXR session with a D3D11 device for swapchain pixel
//! pushes. Non-Windows targets keep the no-op behavior the older OpenVR
//! version had, so macOS/Linux iteration builds stay green.
//!
//! ## Status
//!
//! This is a fresh rewrite away from OpenVR. The lifecycle skeleton +
//! extension detection are in place; the actual swapchain submission,
//! input action set, and composition-layer transform need in-headset
//! validation. Search this file for `TODO(openxr)` for the gaps.

use crate::settings::VrSettings;
use anyhow::{Context as _, Result};
use openxr as xr;

/// Display name and short id that show up in SteamVR's overlay list / logs.
const OVERLAY_NAME: &str = "EZ Wishlist Overlay";

/// Owns the OpenXR instance + session + composition-layer resources for
/// the program's lifetime. Dropping runs the OpenXR cleanup in order
/// (session → instance) via the [`xr`] crate's RAII handles.
pub struct OverlaySession {
    /// The XR instance — represents this app's connection to the runtime.
    /// Drops last (after the session) so the runtime sees a clean shutdown.
    _instance: xr::Instance,
    /// Active session. Holds the swapchain + composition-layer state. The
    /// session has a finite-state lifecycle (IDLE → READY → SYNCHRONIZED →
    /// VISIBLE → FOCUSED) driven by `xrPollEvent` in [`super::runtime`].
    _session: xr::Session<xr::AnyGraphics>,
    /// Reference space the overlay's transform is expressed in. We use
    /// `LOCAL` (player-relative, gravity-aligned) so the overlay anchors
    /// where the player is *now* rather than where their game's stage
    /// origin happens to be.
    _local_space: xr::Space,
    /// Last quad width we used in the composition layer (meters). Kept so
    /// `apply_settings` can skip pushing identical values.
    last_width: f32,
    /// Last alpha applied. Skips redundant calls during fade.
    last_alpha: f32,
}

impl OverlaySession {
    pub fn init(_width_meters: f32) -> Result<Self> {
        // The openxr crate doesn't link the loader at compile time on
        // Windows — it dlopens openxr_loader.dll at runtime. The unsafe
        // call surface here is purely from "I'm loading an arbitrary DLL
        // and trusting its FFI"; we propagate failure as a normal error.
        let entry = unsafe { xr::Entry::load() }
            .context("loading openxr_loader.dll — is an OpenXR runtime installed?")?;

        // Probe what the runtime supports. We require the overlay extension
        // (otherwise we'd take over the whole compositor instead of layering
        // onto whatever game is running). Surface a clear error if it's
        // missing — the user's next move is "use a runtime that ships
        // XR_EXTX_overlay" (SteamVR, recent Quest runtimes).
        let available = entry
            .enumerate_extensions()
            .context("enumerate OpenXR extensions")?;
        if !available.extx_overlay {
            anyhow::bail!(
                "OpenXR runtime does not advertise XR_EXTX_overlay — overlay app mode \
                 is unavailable. SteamVR exposes it; some Quest runtimes do too. \
                 If you're on a different runtime, switch to SteamVR or check the \
                 vendor's extension list."
            );
        }

        let mut enabled = xr::ExtensionSet::default();
        enabled.extx_overlay = true;
        // TODO(openxr): also request the D3D11 (or Vulkan / D3D12) extension
        // matching whichever graphics device we end up creating below.

        let instance = entry
            .create_instance(
                &xr::ApplicationInfo {
                    application_name: OVERLAY_NAME,
                    application_version: env!("CARGO_PKG_VERSION")
                        .split('.')
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    engine_name: "ez-wishlist-overlay",
                    engine_version: 1,
                    api_version: xr::Version::new(1, 0, 0),
                },
                &enabled,
                &[],
            )
            .context("xrCreateInstance — is an OpenXR runtime active?")?;

        // TODO(openxr): pick a system + graphics binding. With XR_EXTX_overlay
        // the spec requires a graphics binding even though the scene app
        // owns the actual scene render. The minimal viable path is D3D11:
        //
        //   1. instance.system(FormFactor::HEAD_MOUNTED_DISPLAY)?
        //   2. create a D3D11 device on the LUID openxr returns via
        //      instance.graphics_requirements::<D3D11>(...)?
        //   3. instance.create_session::<D3D11>(...) with the overlay
        //      session-create-info chained in (xrCreateSessionOverlayEXTX
        //      via the openxr crate's overlay extension helpers).
        //
        // Until that's wired, we bail with a clear "not yet implemented"
        // so the rest of the app stays functional and the VR header chip
        // shows "Disconnected" rather than silently failing later.
        let _ = instance;
        anyhow::bail!(
            "OpenXR session creation is not yet implemented in this branch. \
             See vr/overlay.rs TODO(openxr) comments for the remaining wiring."
        );
    }

    /// Cheap probe used by the runtime loop to detect that the OpenXR
    /// runtime vanished (e.g. SteamVR shut down). TODO(openxr): currently
    /// a no-op; once we have a real session, this should poll session
    /// state via `xrPollEvent` or call an idempotent query.
    pub fn heartbeat(&self) -> Result<()> {
        Ok(())
    }

    /// Push width changes the user made since last tick. TODO(openxr):
    /// update the composition-layer `XrCompositionLayerQuad::size` next
    /// frame.
    pub fn apply_settings(&mut self, desired: &VrSettings) -> Result<()> {
        if (desired.width_meters - self.last_width).abs() > f32::EPSILON {
            self.last_width = desired.width_meters;
            tracing::debug!(width = desired.width_meters, "queued new overlay width");
        }
        Ok(())
    }

    /// HMD pitch in degrees above horizontal. TODO(openxr): query via
    /// `locate_views(LOCAL)` or `xrLocateSpace(VIEW, LOCAL)`.
    pub fn hmd_pitch_deg(&self) -> Option<f32> {
        None
    }

    /// Capture HMD world pose and pin the overlay there. TODO(openxr):
    /// compute the anchor transform via [`super::anchor::world_anchor_from_hmd_with`]
    /// (the math is OpenVR-agnostic) and stash an `XrPosef` for the next
    /// composition-layer submission.
    pub fn anchor_at_current_hmd(&mut self, _height_offset_m: f32) -> Result<bool> {
        Ok(false)
    }

    pub fn set_visible(&mut self, _visible: bool) -> Result<()> {
        // TODO(openxr): toggle whether we include the composition layer in
        // `xrEndFrame` submissions next tick. There's no "show overlay"
        // call in OpenXR; visibility is a per-frame layer-list choice.
        Ok(())
    }

    pub fn set_alpha(&mut self, alpha: f32) -> Result<()> {
        let a = alpha.clamp(0.0, 1.0);
        if (a - self.last_alpha).abs() < 1.0 / 256.0 {
            return Ok(());
        }
        // TODO(openxr): apply via composition-layer flags + tint.
        self.last_alpha = a;
        Ok(())
    }

    /// Push an RGBA8 pixel buffer to the overlay swapchain. TODO(openxr):
    /// acquire/wait/release a swapchain image and copy `bytes` into it
    /// via D3D11 (or whichever graphics backend we end up on).
    pub fn submit_rgba(&mut self, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        debug_assert_eq!(bytes.len(), (width * height * 4) as usize);
        Ok(())
    }

    /// Drain pending XR events. TODO(openxr): use `xrPollEvent` to surface
    /// session-state transitions + input-action events. The runtime loop
    /// will consume click events from here once input is wired.
    pub fn drain_events(&self) {}
}
