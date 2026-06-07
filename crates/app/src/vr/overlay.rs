//! Thin safe wrapper over the [`openvr`] crate: just enough surface to
//! attach to SteamVR, anchor the overlay in world space at show-time,
//! query head pose, push pixel buffers, and dispatch click input.
//!
//! This module is `cfg(target_os = "windows")` because `openvr_sys` builds
//! Valve's C++ SDK via cmake + bindgen and only does that on Windows in our
//! workspace.

use crate::assets;
use crate::settings::VrSettings;
use anyhow::{Context as _, Result};
use openvr::input::{VRActionHandle, VRActionSetHandle, VRActiveActionSet, VRInputValueHandle};
use openvr::system::EventInfo;
use openvr::{
    init, tracked_device_index, ApplicationType, Context, TrackedDeviceIndex,
    TrackingUniverseOrigin,
};

const OVERLAY_KEY: &str = "com.etienneb.ez-wishlist-overlay.main\0";
const OVERLAY_NAME: &str = "EZ Wishlist Overlay\0";

const OCR_OVERLAY_KEY: &str = "com.etienneb.ez-wishlist-overlay.ocr\0";
const OCR_OVERLAY_NAME: &str = "EZ Wishlist OCR Feedback\0";
/// Metric width of the head-locked OCR feedback card. The height in
/// metres follows from the submitted pixmap's aspect ratio, which
/// [`super::ocr_render`] grows up to ~1440 px tall — at 0.72 m wide
/// that caps the card at ~0.55 m. Bigger than the original 0.45 m
/// because users reported squinting; the explicit trade-off here is
/// that the card now occupies more of the field of view, but it's
/// still positioned ~0.18 m below the gaze centre so the upgrade
/// panel underneath remains visible while reading.
const OCR_OVERLAY_WIDTH_M: f32 = 0.72;
/// Head-locked OCR card offsets, in the HMD's local frame.
/// **OpenVR convention**: right-handed, Y-up, -Z forward, so an overlay
/// placed at z=-1.5 sits 1.5 m in front of the user. Y is slightly
/// negative to drop the card below the centre of the gaze so the user
/// can still see the upgrade panel underneath while reading it.
const OCR_OFFSET_X_M: f32 = 0.0;
const OCR_OFFSET_Y_M: f32 = -0.18;
const OCR_OFFSET_Z_M: f32 = -1.2;

const GUIDE_OVERLAY_KEY: &str = "com.etienneb.ez-wishlist-overlay.guide\0";
const GUIDE_OVERLAY_NAME: &str = "EZ Wishlist Capture Guide\0";
/// Default metric width of the head-locked capture guide box (issue #136).
/// Overridden per-frame with an FOV-derived width so the box's hole exactly
/// outlines its crop (issue #141, see [`super::fov`]); this is just the value
/// pushed at init before the loop applies it.
const GUIDE_OVERLAY_WIDTH_M: f32 = 0.9;
/// Head-locked guide-box offsets, HMD local frame. Centered on the gaze and
/// ~1 m in front so the user can line the panel/container up inside it. These
/// are the *initial* values pushed at init; while a capture mode is active the
/// loop overrides the transform per-frame with an FOV-derived placement so the
/// box exactly outlines its crop (issue #141, see [`super::fov`]).
const GUIDE_OFFSET_X_M: f32 = 0.0;
const GUIDE_OFFSET_Y_M: f32 = 0.0;
const GUIDE_OFFSET_Z_M: f32 = -1.0;

/// Empirical correction for the mirror-texture ↔ `projection_raw` correspondence
/// (issue #141). The mirror frame *should* map 1:1 to the capture eye's reported
/// FOV tangents, but supersampling / canted-display quirks can scale it
/// slightly; this multiplies the queried tangents so the box can be nudged to
/// match the crop exactly. 1.0 = trust the runtime verbatim; expect a one-time
/// in-headset tuning pass to refine it.
const MIRROR_FOV_FUDGE: f32 = 1.0;

/// Owns the OpenVR runtime + a single overlay handle for the program's
/// lifetime. Drop runs `VR_Shutdown` via `Context::drop`.
pub struct OverlaySession {
    handle: openvr::overlay::OverlayHandle,
    /// Second overlay used as the head-locked OCR feedback card. Lives
    /// alongside the wishlist grid in the same OpenVR session so they
    /// share fn-table acquisition + cleanup, but is positioned and
    /// shown independently.
    ocr_handle: openvr::overlay::OverlayHandle,
    ctx: Context,
    /// Last width we pushed via SetOverlayWidthInMeters. Used by
    /// `apply_settings` to skip redundant calls.
    last_width: f32,
    /// Last alpha pushed via SetOverlayAlpha. Skips redundant calls
    /// inside the fade loop.
    last_alpha: f32,
    /// Same skip-redundant logic for the OCR overlay's alpha — its
    /// fade in/out runs in parallel with the wishlist's so we track it
    /// separately.
    last_ocr_alpha: f32,
    /// Third overlay: the head-locked capture guide box (issue #136), shown
    /// while a capture mode is active. Created in the same session as the other
    /// two so they share fn-table acquisition + cleanup.
    guide_handle: openvr::overlay::OverlayHandle,
    /// Skip-redundant alpha cache for the guide overlay's fade.
    last_guide_alpha: f32,
    /// Skip-redundant width cache for the guide overlay (set from settings).
    last_guide_width: f32,
    /// Skip-redundant transform cache for the guide overlay: `(center_x_m,
    /// center_y_m, distance_m)` of the head-locked box, so a new transform is
    /// only pushed when the FOV-derived placement actually changes (issue #141).
    last_guide_transform: (f32, f32, f32),
    /// Skip-redundant cache for the guide overlay's `SideBySide_Parallel` stereo
    /// flag — `true` while the box is rendered in only the capture eye (#143).
    last_guide_stereo: bool,
    /// IVRInput state for click detection. None if the action manifest
    /// failed to load — the rest of the overlay still works, just no
    /// trigger detection.
    input: Option<ActionInputs>,
}

/// Cached IVRInput handles for our single boolean trigger action, scoped
/// per-hand. Populated at init time by `init_action_input`; the runtime
/// calls `UpdateActionState` + `GetDigitalActionData` once per tick to
/// read the trigger state.
struct ActionInputs {
    set: VRActionSetHandle,
    trigger: VRActionHandle,
    left_hand: VRInputValueHandle,
    right_hand: VRInputValueHandle,
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

        // Second overlay for the OCR feedback card. Head-locked to the
        // HMD via SetOverlayTransformTrackedDeviceRelative so it tracks
        // the user's gaze rather than sitting in world space — the user
        // glances around the room while OCR runs and the card needs to
        // stay readable wherever they look.
        let ocr_handle = overlay
            .create_overlay(OCR_OVERLAY_KEY, OCR_OVERLAY_NAME)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("CreateOverlay(ocr)")?;
        overlay
            .set_width(ocr_handle, OCR_OVERLAY_WIDTH_M)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayWidthInMeters(ocr)")?;
        overlay
            .set_visibility(ocr_handle, false)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("HideOverlay(ocr)")?;
        overlay
            .set_opacity(ocr_handle, 0.0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha(ocr)")?;
        // Head-lock the OCR overlay to the HMD's local frame at a
        // fixed offset (slightly below + 1.2 m in front). Identity
        // rotation keeps the card facing the user. Same trick the
        // wishlist uses, but tracked-device-relative instead of
        // world-absolute.
        let hmd_relative = openvr::pose::Matrix3x4([
            [1.0, 0.0, 0.0, OCR_OFFSET_X_M],
            [0.0, 1.0, 0.0, OCR_OFFSET_Y_M],
            [0.0, 0.0, 1.0, OCR_OFFSET_Z_M],
        ]);
        overlay
            .set_transform_tracked_device_relative(
                ocr_handle,
                tracked_device_index::HMD,
                &hmd_relative,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayTransformTrackedDeviceRelative(ocr → HMD)")?;

        // Third overlay: the capture guide box. Head-locked like the OCR card
        // but centered on the gaze (so the user lines the panel up inside it).
        // Hidden + transparent until a capture mode arms it.
        let guide_handle = overlay
            .create_overlay(GUIDE_OVERLAY_KEY, GUIDE_OVERLAY_NAME)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("CreateOverlay(guide)")?;
        overlay
            .set_width(guide_handle, GUIDE_OVERLAY_WIDTH_M)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayWidthInMeters(guide)")?;
        overlay
            .set_visibility(guide_handle, false)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("HideOverlay(guide)")?;
        overlay
            .set_opacity(guide_handle, 0.0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha(guide)")?;
        let guide_relative = openvr::pose::Matrix3x4([
            [1.0, 0.0, 0.0, GUIDE_OFFSET_X_M],
            [0.0, 1.0, 0.0, GUIDE_OFFSET_Y_M],
            [0.0, 0.0, 1.0, GUIDE_OFFSET_Z_M],
        ]);
        overlay
            .set_transform_tracked_device_relative(
                guide_handle,
                tracked_device_index::HMD,
                &guide_relative,
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayTransformTrackedDeviceRelative(guide → HMD)")?;

        // Mark the overlay interactive so the controller laser intersects it
        // and SteamVR emits MouseButtonDown events on trigger pulls. The
        // `openvr` 0.9.0 safe wrapper omits `SetOverlayInputMethod` and
        // `SetOverlayFlag`, so we drop down to openvr_sys for these two
        // calls. Without them PR #22's click pipeline never receives a
        // single event — the laser passes right through the overlay.
        unsafe { enable_overlay_interaction(handle) }
            .context("make overlay interactive (input method + visibility flag)")?;

        // Wire up the action system. Legacy `GetControllerState` returns
        // stale zeros for modern controllers (Quest Touch via Link, Index
        // knuckles), so this is the only reliable trigger path.
        let input = match init_action_input(&ctx) {
            Ok(inputs) => {
                tracing::info!("IVRInput action manifest loaded");
                Some(inputs)
            }
            Err(e) => {
                tracing::warn!(error = %e, "IVRInput setup failed; trigger clicks disabled");
                None
            }
        };

        Ok(Self {
            handle,
            ocr_handle,
            ctx,
            last_width: width_meters,
            last_alpha: 0.0,
            last_ocr_alpha: 0.0,
            guide_handle,
            last_guide_alpha: 0.0,
            last_guide_width: GUIDE_OVERLAY_WIDTH_M,
            last_guide_transform: (GUIDE_OFFSET_X_M, GUIDE_OFFSET_Y_M, -GUIDE_OFFSET_Z_M),
            last_guide_stereo: false,
            input,
        })
    }

    /// Push an RGBA8 frame to the OCR feedback overlay.
    pub fn submit_ocr_rgba(&mut self, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        debug_assert_eq!(bytes.len(), (width * height * 4) as usize);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_raw_data(self.ocr_handle, bytes, width as usize, height as usize, 4)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayRaw(ocr)")
    }

    /// Show/hide the OCR feedback overlay independently of the wishlist.
    pub fn set_ocr_visible(&mut self, visible: bool) -> Result<()> {
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_visibility(self.ocr_handle, visible)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("Show/HideOverlay(ocr)")
    }

    pub fn set_ocr_alpha(&mut self, alpha: f32) -> Result<()> {
        let a = alpha.clamp(0.0, 1.0);
        if (a - self.last_ocr_alpha).abs() < 1.0 / 256.0 {
            return Ok(());
        }
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_opacity(self.ocr_handle, a)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha(ocr)")?;
        self.last_ocr_alpha = a;
        Ok(())
    }

    /// Push an RGBA8 frame to the capture guide-box overlay.
    pub fn submit_guide_rgba(&mut self, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        debug_assert_eq!(bytes.len(), (width * height * 4) as usize);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_raw_data(self.guide_handle, bytes, width as usize, height as usize, 4)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayRaw(guide)")
    }

    /// Show/hide the capture guide-box overlay.
    pub fn set_guide_visible(&mut self, visible: bool) -> Result<()> {
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_visibility(self.guide_handle, visible)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("Show/HideOverlay(guide)")
    }

    pub fn set_guide_alpha(&mut self, alpha: f32) -> Result<()> {
        let a = alpha.clamp(0.0, 1.0);
        if (a - self.last_guide_alpha).abs() < 1.0 / 256.0 {
            return Ok(());
        }
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_opacity(self.guide_handle, a)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayAlpha(guide)")?;
        self.last_guide_alpha = a;
        Ok(())
    }

    /// Set the guide box's metric width (derived per-frame from the active
    /// mode's crop width). Skips the call when unchanged.
    pub fn set_guide_width(&mut self, width_meters: f32) -> Result<()> {
        if (width_meters - self.last_guide_width).abs() < f32::EPSILON {
            return Ok(());
        }
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_width(self.guide_handle, width_meters)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayWidthInMeters(guide)")?;
        self.last_guide_width = width_meters;
        Ok(())
    }

    /// Set the guide box's head-locked transform — translation only (identity
    /// rotation), `(center_x, center_y, -distance)` in the HMD's local frame
    /// (issue #141). Driven per-frame from the FOV-derived crop placement so the
    /// box sits exactly over its crop (including the per-eye asymmetry a fixed
    /// `(0,0,-1)` offset couldn't). Skips the call when unchanged.
    pub fn set_guide_transform(
        &mut self,
        center_x_m: f32,
        center_y_m: f32,
        distance_m: f32,
    ) -> Result<()> {
        let (lx, ly, ld) = self.last_guide_transform;
        if (center_x_m - lx).abs() < f32::EPSILON
            && (center_y_m - ly).abs() < f32::EPSILON
            && (distance_m - ld).abs() < f32::EPSILON
        {
            return Ok(());
        }
        let m = openvr::pose::Matrix3x4([
            [1.0, 0.0, 0.0, center_x_m],
            [0.0, 1.0, 0.0, center_y_m],
            [0.0, 0.0, 1.0, -distance_m],
        ]);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_transform_tracked_device_relative(self.guide_handle, tracked_device_index::HMD, &m)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayTransformTrackedDeviceRelative(guide)")?;
        self.last_guide_transform = (center_x_m, center_y_m, distance_m);
        Ok(())
    }

    /// Query the capture eye's projection frustum tangents via
    /// `IVRSystem::GetProjectionRaw` (issue #141) — the tangents of the
    /// half-angles to the four clipping planes, which the mirror frame maps onto
    /// linearly. Returns `None` if the `IVRSystem` interface can't be acquired
    /// this tick (caller falls back to [`super::fov::EyeFov::FALLBACK`]). The
    /// reported tangents are scaled by [`MIRROR_FOV_FUDGE`].
    pub fn eye_fov(&self, eye: crate::settings::CaptureEye) -> Option<super::fov::EyeFov> {
        let system = self.ctx.system().ok()?;
        let oeye = match eye {
            crate::settings::CaptureEye::Left => openvr::Eye::Left,
            crate::settings::CaptureEye::Right => openvr::Eye::Right,
        };
        let p = system.projection_raw(oeye);
        Some(super::fov::EyeFov {
            left: p.left * MIRROR_FOV_FUDGE,
            right: p.right * MIRROR_FOV_FUDGE,
            top: p.top * MIRROR_FOV_FUDGE,
            bottom: p.bottom * MIRROR_FOV_FUDGE,
        })
    }

    /// The capture eye's translation off the HMD (head) origin, in metres —
    /// `(dx, dy, dz)` from `IVRSystem::GetEyeToHeadTransform` (dx ≈ ±IPD/2).
    /// Used to parallax-correct the capture crop so it lines up with the
    /// head-locked guide box as seen by *this* eye (issue #141). Returns `None`
    /// if the `IVRSystem` interface can't be acquired this tick.
    pub fn eye_offset(&self, eye: crate::settings::CaptureEye) -> Option<(f32, f32, f32)> {
        let system = self.ctx.system().ok()?;
        let oeye = match eye {
            crate::settings::CaptureEye::Left => openvr::Eye::Left,
            crate::settings::CaptureEye::Right => openvr::Eye::Right,
        };
        let m = system.eye_to_head_transform(oeye);
        Some((m[0][3], m[1][3], m[2][3]))
    }

    /// Toggle the guide overlay's `SideBySide_Parallel` stereo flag (issue #143).
    /// When `on`, the overlay texture is treated as two half-width images
    /// (left half → left eye, right half → right eye); paired with a texture
    /// that has content in only the capture eye's half (see
    /// [`super::guide::side_by_side`]), this renders the box in just that eye,
    /// killing the binocular "double box". When `off`, the overlay renders
    /// normally to both eyes. Skips the call when unchanged.
    pub fn set_guide_stereo(&mut self, on: bool) -> Result<()> {
        use openvr_sys as sys;
        if on == self.last_guide_stereo {
            return Ok(());
        }
        // SAFETY: VR_Init has run (this session owns it). Re-acquire the
        // IVROverlay C fn-table the same way `enable_overlay_interaction` does,
        // since the safe wrapper omits `SetOverlayFlag`.
        unsafe {
            let mut init_err = sys::EVRInitError_VRInitError_None;
            let version = c"FnTable:IVROverlay_028".as_ptr();
            let table_ptr = sys::VR_GetGenericInterface(version.cast(), &mut init_err)
                as *const sys::VR_IVROverlay_FnTable;
            if init_err != sys::EVRInitError_VRInitError_None || table_ptr.is_null() {
                anyhow::bail!(
                    "VR_GetGenericInterface(FnTable:IVROverlay_028) failed: err={init_err}"
                );
            }
            let table = &*table_ptr;
            let set_flag = table
                .SetOverlayFlag
                .context("IVROverlay::SetOverlayFlag missing from fn table")?;
            let e = set_flag(
                self.guide_handle.0,
                sys::VROverlayFlags_SideBySide_Parallel,
                on,
            );
            if e != sys::EVROverlayError_VROverlayError_None {
                anyhow::bail!(
                    "SetOverlayFlag(SideBySide_Parallel, {on}) returned EVROverlayError={e}"
                );
            }
        }
        self.last_guide_stereo = on;
        tracing::debug!(on, "guide overlay: SideBySide stereo flag set");
        Ok(())
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

    /// Capture the HMD's current world pose and pin the overlay there as
    /// an absolute (world-space) transform. Yaw + position only — pitch and
    /// roll are dropped so the overlay sits in front of the user at the
    /// moment of trigger, not above their face where their gaze was
    /// pointing. `height_offset_m` controls how far above the HMD the panel
    /// floats (see [`super::anchor::HEIGHT_M`] for the historical default).
    /// Returns `Ok(false)` if the HMD pose is invalid this frame (caller
    /// should retry on the next tick).
    pub fn anchor_at_current_hmd(&mut self, height_offset_m: f32) -> Result<bool> {
        let system = self
            .ctx
            .system()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVRSystem interface")?;
        let poses = system.device_to_absolute_tracking_pose(TrackingUniverseOrigin::Standing, 0.0);
        let hmd = &poses[tracked_device_index::HMD.0 as usize];
        if !hmd.pose_is_valid() {
            return Ok(false);
        }
        let m = super::anchor::world_anchor_from_hmd_with(
            hmd.device_to_absolute_tracking(),
            super::anchor::DISTANCE_M,
            height_offset_m,
            super::anchor::TILT_DEG,
        );
        let matrix = openvr::pose::Matrix3x4(m);
        let mut overlay = self
            .ctx
            .overlay()
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("IVROverlay interface")?;
        overlay
            .set_transform_absolute(self.handle, TrackingUniverseOrigin::Standing, &matrix)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("SetOverlayTransformAbsolute")?;
        tracing::debug!("anchor transform applied (world-space, yaw-only)");
        Ok(true)
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

    /// Step the IVRInput action state forward and read the trigger digital
    /// action for both hands. Returns a `(left_event, right_event)` tuple
    /// where each is `Some(TrackedDeviceIndex)` if the trigger just
    /// transitioned to pressed on that hand this tick, else `None`.
    ///
    /// Returns `(None, None)` if the action system wasn't set up at init.
    pub fn poll_trigger_actions(
        &mut self,
    ) -> (Option<TrackedDeviceIndex>, Option<TrackedDeviceIndex>) {
        let Some(inputs) = self.input.as_ref() else {
            return (None, None);
        };
        let set = inputs.set;
        let trigger = inputs.trigger;
        let left_hand = inputs.left_hand;
        let right_hand = inputs.right_hand;

        let (left_idx, right_idx) = self.controller_indices();

        let Ok(mut input) = self.ctx.input() else {
            return (None, None);
        };

        let mut active = [VRActiveActionSet(openvr_sys::VRActiveActionSet_t {
            ulActionSet: set.0,
            ulRestrictedToDevice: 0, // k_ulInvalidInputValueHandle = all
            ulSecondaryActionSet: 0,
            unPadding: 0,
            nPriority: 0,
        })];
        if let Err(e) = input.update_actions(&mut active) {
            tracing::debug!(error = ?e, "UpdateActionState failed");
            return (None, None);
        }

        // bChanged && bState = rising edge this tick, per OpenVR semantics.
        let left_fired = matches!(
            input.get_digital_action_data(trigger, left_hand),
            Ok(d) if d.0.bActive && d.0.bChanged && d.0.bState
        );
        let right_fired = matches!(
            input.get_digital_action_data(trigger, right_hand),
            Ok(d) if d.0.bActive && d.0.bChanged && d.0.bState
        );

        // Some runtimes don't isolate a shared, "single"-usage boolean action
        // per hand: `restrictToDevice` leaks the global state, so BOTH hands
        // report fired on a single trigger pull (which made the Left/Right
        // capture-trigger setting look like it had no effect). When both fire,
        // attribute the press to the controller actually driving the action
        // this tick, via the action's active origin.
        if left_fired && right_fired {
            if let Ok(g) = input.get_digital_action_data(trigger, VRInputValueHandle(0)) {
                if let Ok(info) =
                    input.get_origin_tracked_device_info(VRInputValueHandle(g.0.activeOrigin))
                {
                    let dev = TrackedDeviceIndex(info.0.trackedDeviceIndex);
                    if Some(dev) == left_idx {
                        return (left_idx, None);
                    }
                    if Some(dev) == right_idx {
                        return (None, right_idx);
                    }
                }
            }
        }

        let left = if left_fired { left_idx } else { None };
        let right = if right_fired { right_idx } else { None };
        (left, right)
    }

    /// Resolve the left/right controller role into tracked device indices.
    fn controller_indices(&self) -> (Option<TrackedDeviceIndex>, Option<TrackedDeviceIndex>) {
        let Ok(system) = self.ctx.system() else {
            return (None, None);
        };
        let left = system
            .tracked_device_index_for_controller_role(openvr::TrackedControllerRole::LeftHand);
        let right = system
            .tracked_device_index_for_controller_role(openvr::TrackedControllerRole::RightHand);
        (left, right)
    }

    /// Pull the compositor's mirror texture (native render-target
    /// resolution, lossless) into an in-memory bitmap. Lossless input
    /// is what makes the OCR pipeline tractable; the chunky pixel-art
    /// digit font is destroyed by Steam's F12 JPEG. Returning the
    /// bitmap rather than a written PNG lets the runtime hand it
    /// straight to the OCR worker over the channel, skipping the
    /// ~5 s round-trip through disk in the default fast path. Method
    /// lives on `OverlaySession` purely so the type system enforces
    /// "VR_Init has run on this thread".
    pub fn capture_screenshot(
        &self,
        eye: super::capture::CaptureEye,
        trace: bool,
    ) -> Result<image::DynamicImage> {
        super::capture::capture_compositor_mirror_image(eye, trace)
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

    /// Ray-cast from a controller's current pose against this overlay.
    /// Returns the `(u, v)` texcoord of the hit if the ray intersects the
    /// overlay; `None` if it misses or the device pose is invalid.
    ///
    /// Used as a fallback for the click pipeline when SteamVR doesn't
    /// auto-route trigger pulls to our overlay as `MouseButtonDown`
    /// events — we get raw `ButtonPress(button=33)` events and have to
    /// figure out the hit ourselves.
    pub fn intersect_from_device(&self, device: TrackedDeviceIndex) -> Option<(f32, f32)> {
        // Quest Touch's grip pose -Z is tilted *up* relative to where the
        // user visually aims. Tilt the ray down ~30° around the controller's
        // local X to compensate. A proper fix is the IVRInput action-system
        // "aim" pose; this approximation gets us close enough.
        const AIM_PITCH_CORRECTION_DEG: f32 = -30.0;

        let system = self.ctx.system().ok()?;
        let poses = system.device_to_absolute_tracking_pose(TrackingUniverseOrigin::Standing, 0.0);
        let pose = poses.get(device.0 as usize)?;
        if !pose.pose_is_valid() {
            return None;
        }
        let m = pose.device_to_absolute_tracking();
        let src = openvr_sys::HmdVector3_t {
            v: [m[0][3], m[1][3], m[2][3]],
        };
        // Local (0,0,-1) rotated by θ around X = (0, sin θ, -cos θ); then
        // transformed to world via col0=X, col1=Y, col2=Z of the controller
        // rotation submatrix.
        let pitch = AIM_PITCH_CORRECTION_DEG.to_radians();
        let (sp, cp) = (pitch.sin(), pitch.cos());
        let dir = openvr_sys::HmdVector3_t {
            v: [
                m[0][1] * sp + m[0][2] * (-cp),
                m[1][1] * sp + m[1][2] * (-cp),
                m[2][1] * sp + m[2][2] * (-cp),
            ],
        };
        unsafe { compute_overlay_intersection(self.handle.0, src, dir) }
    }
}

/// Set `VROverlayInputMethod_Mouse` and turn on
/// `VROverlayFlags_MakeOverlaysInteractiveIfVisible` on the given handle —
/// the two pieces that make SteamVR route controller-laser intersections
/// to our overlay as mouse events.
///
/// ## Why we re-acquire the fn-table
///
/// The `openvr` 0.9.0 safe wrapper queries `VR_GetGenericInterface` with
/// the bare interface version `"IVROverlay_028"`. That actually returns a
/// **C++ COM object pointer**, not a fn-table — its private member layout
/// happens to overlap with valid function pointers for a handful of
/// methods (ShowOverlay, SetOverlayWidthInMeters, SetOverlayAlpha, etc.),
/// which is why the safe wrapper appears to work. For the methods we need
/// here that overlap simply isn't there — `SetOverlayFlag`'s would-be
/// position reads as null garbage.
///
/// The fix is to ask for the C fn-table directly via the `"FnTable:"`
/// prefix, which is what the C API was designed around. We then call
/// through that fn-table at the offsets `openvr_sys` 2.1.4's bindgen
/// expects — both match this way.
///
/// # Safety
/// Must be called after a successful `VR_Init`. The fn-table pointer is
/// valid for the lifetime of the current OpenVR session, which the
/// caller (`OverlaySession`) owns.
unsafe fn enable_overlay_interaction(handle: openvr::overlay::OverlayHandle) -> Result<()> {
    use openvr_sys as sys;

    let mut init_err: sys::EVRInitError = sys::EVRInitError_VRInitError_None;
    let version = c"FnTable:IVROverlay_028".as_ptr();
    let table_ptr = sys::VR_GetGenericInterface(version.cast(), &mut init_err)
        as *const sys::VR_IVROverlay_FnTable;
    if init_err != sys::EVRInitError_VRInitError_None || table_ptr.is_null() {
        anyhow::bail!(
            "VR_GetGenericInterface(FnTable:IVROverlay_028) failed: err={init_err}, ptr={table_ptr:?}"
        );
    }
    let table = &*table_ptr;
    let h: sys::VROverlayHandle_t = handle.0;

    let set_flag = table
        .SetOverlayFlag
        .context("IVROverlay::SetOverlayFlag missing from fn table")?;
    let e = set_flag(
        h,
        sys::VROverlayFlags_MakeOverlaysInteractiveIfVisible,
        true,
    );
    if e != sys::EVROverlayError_VROverlayError_None {
        anyhow::bail!(
            "SetOverlayFlag(MakeOverlaysInteractiveIfVisible) returned EVROverlayError={e}"
        );
    }
    tracing::info!("overlay flag set (MakeOverlaysInteractiveIfVisible)");

    // Input method is intentionally **None**, not Mouse. Mouse mode hands
    // the controller trigger to SteamVR's overlay-mouse pipeline, which
    // drops it on the floor for non-dashboard overlays — meaning our
    // IVRInput action set never sees the press. None lets the trigger
    // flow through to the action system; the `MakeOverlaysInteractiveIfVisible`
    // flag (set above) is still what governs the laser intersecting the
    // overlay visually.
    let set_input = table
        .SetOverlayInputMethod
        .context("IVROverlay::SetOverlayInputMethod missing from fn table")?;
    let e = set_input(h, sys::VROverlayInputMethod_None);
    if e != sys::EVROverlayError_VROverlayError_None {
        anyhow::bail!("SetOverlayInputMethod(None) returned EVROverlayError={e}");
    }
    tracing::info!("overlay input method set (None)");

    Ok(())
}

/// FFI: `IVROverlay::ComputeOverlayIntersection`. Asks the runtime to
/// project a ray (`src` + `dir`) against the overlay handle and report
/// the texcoord hit point. Returns `None` on miss.
///
/// # Safety
/// Must be called after a successful `VR_Init`. Re-acquires the fn-table
/// via `VR_GetGenericInterface("FnTable:IVROverlay_028")` — same path
/// `enable_overlay_interaction` uses for its calls.
unsafe fn compute_overlay_intersection(
    handle: openvr_sys::VROverlayHandle_t,
    src: openvr_sys::HmdVector3_t,
    dir: openvr_sys::HmdVector3_t,
) -> Option<(f32, f32)> {
    use openvr_sys as sys;
    let mut init_err = sys::EVRInitError_VRInitError_None;
    let version = c"FnTable:IVROverlay_028".as_ptr();
    let table_ptr = sys::VR_GetGenericInterface(version.cast(), &mut init_err)
        as *const sys::VR_IVROverlay_FnTable;
    if init_err != sys::EVRInitError_VRInitError_None || table_ptr.is_null() {
        return None;
    }
    let table = &*table_ptr;
    let compute = table.ComputeOverlayIntersection?;

    let mut params = sys::VROverlayIntersectionParams_t {
        vSource: src,
        vDirection: dir,
        eOrigin: sys::ETrackingUniverseOrigin_TrackingUniverseStanding,
    };
    let mut results = sys::VROverlayIntersectionResults_t::default();
    let hit = compute(handle, &mut params, &mut results);
    if !hit {
        return None;
    }
    Some((results.vUVs.v[0], results.vUVs.v[1]))
}

/// Extract the bundled action manifest + bindings to a temp dir, hand it to
/// `IVRInput::SetActionManifestPath`, and resolve our action/handle pairs.
///
/// The manifest path is unique per process so multiple instances on the
/// same machine don't collide.
fn init_action_input(ctx: &Context) -> Result<ActionInputs> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("ez-wishlist-overlay-vr-{}", std::process::id()));
    let manifest = assets::extract_vr_actions(&dir).context("write VR action manifest")?;

    let mut input = ctx
        .input()
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("get IVRInput interface")?;
    input
        .set_action_manifest(&manifest)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .with_context(|| format!("SetActionManifestPath({})", manifest.display()))?;
    let set = input
        .get_action_set_handle("/actions/main")
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("get_action_set_handle(/actions/main)")?;
    let trigger = input
        .get_action_handle("/actions/main/in/trigger")
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("get_action_handle(trigger)")?;
    let left_hand = input
        .get_input_source_handle("/user/hand/left")
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("get_input_source_handle(left)")?;
    let right_hand = input
        .get_input_source_handle("/user/hand/right")
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("get_input_source_handle(right)")?;
    Ok(ActionInputs {
        set,
        trigger,
        left_hand,
        right_hand,
    })
}
