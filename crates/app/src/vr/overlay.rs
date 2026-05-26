//! OpenXR overlay session: instance + D3D11 device + swapchain + composition
//! layer + action set, wrapped in a single owner that the runtime thread
//! drives per-tick.
//!
//! High-level flow per frame (driven by [`super::runtime`]):
//!
//!   1. `poll_events` — drain `xrPollEvent`; advance the session-state
//!      machine (IDLE → READY → SYNCHRONIZED → VISIBLE → FOCUSED). Skip
//!      everything else until we're at least SYNCHRONIZED.
//!   2. `wait_frame` — blocks for the runtime's frame pacing tick.
//!   3. `submit_rgba` (when overlay is visible) — copies our CPU pixel
//!      buffer into the next swapchain image via a staging texture.
//!   4. `end_frame_with_layer` — emits the world-anchored composition
//!      layer quad if visible, or an empty layer list if hidden.
//!   5. `poll_click` — samples the trigger action; if newly pressed,
//!      locates the hand pose, ray-casts against the quad, returns the
//!      texture-space (u, v) of the hit so the runtime can dispatch via
//!      the existing [`super::input::handle_click`] pipeline.
//!
//! Untested in-headset as of this commit. Search `TODO(openxr-headset)`
//! for the spots most likely to need iteration on a real runtime.

#![allow(clippy::too_many_arguments)]

use crate::settings::VrSettings;
use anyhow::{Context as _, Result};
use openxr as xr;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_FLAG,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DYNAMIC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};

const OVERLAY_NAME: &str = "EZ Wishlist Overlay";
/// `XR_MIN_COMPOSITION_LAYERS_SUPPORTED` is 16; we only ever submit one.
const MAX_LAYERS: usize = 1;
/// Suggested-bindings interaction profile paths. We bind to the generic
/// `khr/simple_controller` profile, which all runtimes implement and
/// re-map to the user's actual controllers (Index, Touch, etc.).
const INTERACTION_PROFILE: &str = "/interaction_profiles/khr/simple_controller";

pub struct OverlaySession {
    instance: xr::Instance,
    system: xr::SystemId,
    session: xr::Session<xr::D3D11>,
    /// Reference space for our world-anchored quad. `LOCAL` is gravity-
    /// aligned and origin-at-headset-start, which matches what
    /// `super::anchor::world_anchor_from_hmd_with` already computes against.
    space: xr::Space,
    frame_waiter: xr::FrameWaiter,
    frame_stream: xr::FrameStream<xr::D3D11>,
    swapchain: xr::Swapchain<xr::D3D11>,
    /// `xr::d3d::D3D11::SwapchainImage = *mut sys::platform::ID3D11Texture2D`
    /// which is opaque (`*mut c_void`). We cast to the strongly-typed
    /// `windows::ID3D11Texture2D` only at the call site that needs it.
    swapchain_images: Vec<*mut std::ffi::c_void>,
    swapchain_extent: (u32, u32),
    /// CPU-writable scratch texture we copy our RGBA buffer into before
    /// `CopyResource`ing into the runtime-owned swapchain image.
    staging: ID3D11Texture2D,
    d3d11_device: ID3D11Device,
    d3d11_context: ID3D11DeviceContext,
    action_set: xr::ActionSet,
    click_action: xr::Action<bool>,
    pose_action: xr::Action<xr::Posef>,
    haptic_action: xr::Action<xr::Haptic>,
    left_hand_space: xr::Space,
    right_hand_space: xr::Space,
    /// Where the overlay currently sits in `space`'s coordinate frame.
    /// Updated by `anchor_at_current_hmd`; consumed each `end_frame` when
    /// we build the composition layer.
    current_anchor: Option<xr::Posef>,
    /// Composition-layer quad size in meters (width, height). Height is
    /// derived from width × aspect every render frame so the runtime
    /// sees the layer match whatever the renderer produced.
    quad_size: (f32, f32),
    last_width_setting: f32,
    last_alpha: f32,
    visible: bool,
    /// Current OpenXR session state — advanced by `poll_events`. Frames
    /// are only submitted when we're at SYNCHRONIZED or above.
    session_state: xr::SessionState,
    /// Previous trigger sample; an edge-trigger detector compares against
    /// this to fire one click per pull rather than ~90 per second.
    prev_trigger_left: bool,
    prev_trigger_right: bool,
}

/// Hit information produced by [`OverlaySession::poll_click`] when a
/// trigger pull's ray intersects the quad.
pub struct ClickHit {
    /// Texture-space coordinates (origin top-left, unit square).
    pub u: f32,
    pub v: f32,
    /// Which hand sent the click — runtime uses this to buzz the right controller.
    pub hand: Hand,
}

#[derive(Clone, Copy, Debug)]
pub enum Hand {
    Left,
    Right,
}

impl OverlaySession {
    pub fn init(width_meters: f32) -> Result<Self> {
        // 1) Load the OpenXR loader DLL + check for our required extensions.
        let entry = load_openxr_entry()
            .context("could not load openxr_loader.dll from any known location")?;
        let available = entry
            .enumerate_extensions()
            .context("enumerate OpenXR extensions")?;
        if !available.extx_overlay {
            let active = read_active_openxr_runtime().unwrap_or_else(|| "(unknown)".into());
            anyhow::bail!(
                "Active OpenXR runtime ({active}) does not advertise XR_EXTX_overlay, so we \
                 can't run as an overlay app. Fix: launch SteamVR, open Settings → Developer \
                 (enable Advanced if needed), click \"Set SteamVR as OpenXR Runtime\", then \
                 relaunch this app."
            );
        }
        if !available.khr_d3d11_enable {
            anyhow::bail!("OpenXR runtime does not advertise XR_KHR_D3D11_enable");
        }
        let mut enabled = xr::ExtensionSet::default();
        enabled.extx_overlay = true;
        enabled.khr_d3d11_enable = true;

        let instance = entry
            .create_instance(
                &xr::ApplicationInfo {
                    application_name: OVERLAY_NAME,
                    application_version: 0,
                    engine_name: "ez-wishlist-overlay",
                    engine_version: 1,
                    api_version: xr::Version::new(1, 0, 0),
                },
                &enabled,
                &[],
            )
            .context("xrCreateInstance — is an OpenXR runtime active?")?;

        // 2) Pick the HMD system + the D3D11 adapter the runtime requires.
        let system = instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .context("xrGetSystem — no HMD form factor available")?;
        let reqs = <xr::D3D11 as xr::Graphics>::requirements(&instance, system)
            .context("get D3D11 graphics requirements")?;

        // 3) Create a D3D11 device on the adapter matching the runtime's LUID.
        // The openxr-sys LUID is layout-compatible with the Win32 one but a
        // distinct type — copy the fields across.
        let target_luid = LUID {
            LowPart: reqs.adapter_luid.LowPart,
            HighPart: reqs.adapter_luid.HighPart,
        };
        let (d3d11_device, d3d11_context) = create_d3d11_device(target_luid)
            .context("create D3D11 device on runtime-required adapter")?;

        // 4) Create the session, advertising overlay-app mode.
        let create_info = xr::d3d::SessionCreateInfoD3D11 {
            device: d3d11_device.as_raw() as *mut _,
        };
        let (session, frame_waiter, frame_stream) = unsafe {
            instance
                .create_session::<xr::D3D11>(system, &create_info)
                .context("xrCreateSession (D3D11 + overlay)")?
        };

        // 5) Reference space for the world-anchored quad.
        let space = session
            .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            .context("create LOCAL reference space")?;

        // 6) Swapchain that holds our CPU-rendered pixmap. Width/height are
        //    placeholders — submit_rgba recreates the swapchain whenever
        //    the renderer produces a different canvas size (the override
        //    editor + wishlist size can change it).
        let swapchain_extent = (1024_u32, 1024_u32);
        let (swapchain, swapchain_images, staging) = make_swapchain(
            &session,
            &d3d11_device,
            swapchain_extent.0,
            swapchain_extent.1,
        )
        .context("create initial swapchain")?;

        // 7) Action set for trigger + hand pose + haptic.
        let action_set = instance
            .create_action_set("ez_wishlist_overlay", "EZ Wishlist Overlay", 0)
            .context("create action set")?;
        let click_action = action_set
            .create_action::<bool>("trigger_click", "Trigger Click", &[])
            .context("create click action")?;
        let pose_action = action_set
            .create_action::<xr::Posef>("hand_pose", "Hand Pose", &[])
            .context("create hand pose action")?;
        let haptic_action = action_set
            .create_action::<xr::Haptic>("haptic", "Haptic", &[])
            .context("create haptic action")?;

        suggest_simple_controller_bindings(&instance, &click_action, &pose_action, &haptic_action)
            .context("suggest interaction-profile bindings")?;

        session
            .attach_action_sets(&[&action_set])
            .context("attach action sets")?;

        let left_hand_space = pose_action
            .create_space(&session, xr::Path::NULL, xr::Posef::IDENTITY)
            .context("create left hand pose space")?;
        let right_hand_space = pose_action
            .create_space(&session, xr::Path::NULL, xr::Posef::IDENTITY)
            .context("create right hand pose space")?;

        Ok(Self {
            instance,
            system,
            session,
            space,
            frame_waiter,
            frame_stream,
            swapchain,
            swapchain_images,
            swapchain_extent,
            staging,
            d3d11_device,
            d3d11_context,
            action_set,
            click_action,
            pose_action,
            haptic_action,
            left_hand_space,
            right_hand_space,
            current_anchor: None,
            quad_size: (width_meters, width_meters),
            last_width_setting: width_meters,
            last_alpha: 1.0,
            visible: false,
            session_state: xr::SessionState::IDLE,
            prev_trigger_left: false,
            prev_trigger_right: false,
        })
    }

    /// Drain `xrPollEvent` and advance session state. Returns `Err` if the
    /// runtime signaled loss-pending or the instance was lost — the runtime
    /// thread treats either as "session lost, retry".
    pub fn poll_events(&mut self) -> Result<()> {
        let mut storage = xr::EventDataBuffer::new();
        while let Some(event) = self
            .instance
            .poll_event(&mut storage)
            .context("xrPollEvent")?
        {
            if let xr::Event::SessionStateChanged(e) = event {
                let new_state = e.state();
                tracing::debug!(?new_state, "openxr session state");
                self.session_state = new_state;
                match new_state {
                    xr::SessionState::READY => {
                        self.session
                            .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                            .context("xrBeginSession")?;
                    }
                    xr::SessionState::STOPPING => {
                        self.session.end().context("xrEndSession")?;
                    }
                    xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                        anyhow::bail!("OpenXR session exiting / loss pending");
                    }
                    _ => {}
                }
            }
            // We don't care about other event types yet.
        }
        Ok(())
    }

    /// True iff the session is in a state where frames must be submitted.
    pub fn is_running(&self) -> bool {
        matches!(
            self.session_state,
            xr::SessionState::SYNCHRONIZED | xr::SessionState::VISIBLE | xr::SessionState::FOCUSED
        )
    }

    /// `xrWaitFrame` — block for the runtime's compositor tick. Returns the
    /// predicted display time the next `end_frame` should be tagged with.
    pub fn wait_frame(&mut self) -> Result<xr::Time> {
        let state = self.frame_waiter.wait().context("xrWaitFrame")?;
        Ok(state.predicted_display_time)
    }

    /// `xrBeginFrame` — paired 1:1 with `end_frame`.
    pub fn begin_frame(&mut self) -> Result<()> {
        self.frame_stream.begin().context("xrBeginFrame")?;
        Ok(())
    }

    /// Cheap probe; the real liveness check now happens via `poll_events`.
    /// Kept on the API for symmetry with the runtime's call site.
    pub fn heartbeat(&self) -> Result<()> {
        Ok(())
    }

    pub fn apply_settings(&mut self, desired: &VrSettings) -> Result<()> {
        if (desired.width_meters - self.last_width_setting).abs() > f32::EPSILON {
            // Height tracks width × current aspect — gets corrected on next
            // submit_rgba which knows the real canvas dimensions.
            let aspect = self.quad_size.1 / self.quad_size.0.max(1e-3);
            self.quad_size = (desired.width_meters, desired.width_meters * aspect);
            self.last_width_setting = desired.width_meters;
        }
        Ok(())
    }

    /// Read HMD pitch via the VIEW reference space localized in LOCAL. None
    /// when the runtime can't yet locate the view (first few frames).
    pub fn hmd_pitch_deg(&mut self) -> Option<f32> {
        // TODO(openxr-headset): once we have a VIEW reference space cached,
        // use xrLocateSpace to read the head pose and feed into
        // `super::pose::pitch_from_hmd_matrix`. For now park at 0 — the
        // visibility FSM will treat that as "below show threshold" so the
        // overlay stays hidden until pitch wiring lands.
        None
    }

    /// Sample the runtime to learn where the headset is right now and pin
    /// our composition-layer pose there, yaw + position only.
    pub fn anchor_at_current_hmd(&mut self, _height_offset_m: f32) -> Result<bool> {
        // TODO(openxr-headset): once VIEW-space pose locating is wired,
        // call super::anchor::world_anchor_from_hmd_with and convert the
        // result into an xr::Posef. For now, anchor at +1m forward of the
        // origin so something is at least visible during the first
        // session.
        self.current_anchor = Some(xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: xr::Vector3f {
                x: 0.0,
                y: 1.5,
                z: -1.5,
            },
        });
        Ok(true)
    }

    pub fn set_visible(&mut self, visible: bool) -> Result<()> {
        self.visible = visible;
        Ok(())
    }

    pub fn set_alpha(&mut self, alpha: f32) -> Result<()> {
        self.last_alpha = alpha.clamp(0.0, 1.0);
        Ok(())
    }

    /// Copy `bytes` (RGBA8, top-left origin) into the next swapchain image.
    pub fn submit_rgba(&mut self, bytes: &[u8], width: u32, height: u32) -> Result<()> {
        debug_assert_eq!(bytes.len(), (width * height * 4) as usize);

        if (width, height) != self.swapchain_extent {
            tracing::debug!(width, height, "openxr: rebuilding swapchain for new canvas");
            let (sw, imgs, staging) =
                make_swapchain(&self.session, &self.d3d11_device, width, height)
                    .context("rebuild swapchain")?;
            self.swapchain = sw;
            self.swapchain_images = imgs;
            self.staging = staging;
            self.swapchain_extent = (width, height);
            // Width stays, height becomes width × (h/w aspect of canvas).
            let aspect = height as f32 / width.max(1) as f32;
            self.quad_size = (self.last_width_setting, self.last_width_setting * aspect);
        }

        let image_index = self.swapchain.acquire_image().context("acquire image")?;
        self.swapchain
            .wait_image(xr::Duration::INFINITE)
            .context("wait image")?;

        unsafe {
            // Map the staging texture, copy RGBA in, unmap.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.d3d11_context
                .Map(
                    &self.staging,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .context("D3D11 Map staging")?;
            let row_bytes = (width * 4) as usize;
            let dst = std::slice::from_raw_parts_mut(
                mapped.pData as *mut u8,
                mapped.RowPitch as usize * height as usize,
            );
            for y in 0..height as usize {
                let src_off = y * row_bytes;
                let dst_off = y * mapped.RowPitch as usize;
                dst[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&bytes[src_off..src_off + row_bytes]);
            }
            self.d3d11_context.Unmap(&self.staging, 0);

            // Copy staging → the swapchain image the runtime just handed us.
            // The OpenXR-returned pointer is opaque `*mut c_void`; the runtime
            // contract guarantees it's an ID3D11Texture2D, so the cast is safe.
            let dst_tex: &ID3D11Texture2D =
                &*(self.swapchain_images[image_index as usize] as *const ID3D11Texture2D);
            let box_ = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            };
            self.d3d11_context.CopySubresourceRegion(
                dst_tex,
                0,
                0,
                0,
                0,
                &self.staging,
                0,
                Some(&box_),
            );
        }

        self.swapchain.release_image().context("release image")?;
        Ok(())
    }

    /// `xrEndFrame` with our world-anchored quad in the layer list when
    /// visible. Caller must have called `begin_frame` + `wait_frame` first.
    pub fn end_frame_with_layer(&mut self, display_time: xr::Time) -> Result<()> {
        let anchor = self.current_anchor.unwrap_or(xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: xr::Vector3f::default(),
        });
        let quad = build_quad_layer(
            anchor,
            self.quad_size,
            &self.swapchain,
            self.swapchain_extent,
            &self.space,
        );
        let layers: Vec<&xr::CompositionLayerBase<xr::D3D11>> =
            if self.visible && self.current_anchor.is_some() {
                vec![unsafe { quad.as_ref() }]
            } else {
                Vec::new()
            };
        self.frame_stream
            .end(display_time, xr::EnvironmentBlendMode::OPAQUE, &layers)
            .context("xrEndFrame")?;
        Ok(())
    }

    /// Sync the action set, sample trigger edges + hand poses, ray-cast.
    /// Returns one [`ClickHit`] per controller that just pressed its trigger
    /// AND whose ray intersects the quad.
    pub fn poll_click(&mut self, display_time: xr::Time) -> Result<Vec<ClickHit>> {
        if !self.is_running() {
            return Ok(Vec::new());
        }
        let active = xr::ActiveActionSet::new(&self.action_set);
        self.session
            .sync_actions(&[active])
            .context("xrSyncActions")?;

        let mut hits = Vec::new();
        for hand in [Hand::Left, Hand::Right] {
            let pressed = self
                .click_action
                .state(&self.session, xr::Path::NULL)
                .map(|s| s.current_state)
                .unwrap_or(false);
            // TODO(openxr-headset): subaction paths so we can distinguish
            // left vs right hand here. With the current bare action, both
            // hands trigger the same `pressed` bit — fine for proof of
            // life, needs subaction split before haptics route to the
            // right controller.
            let prev = match hand {
                Hand::Left => &mut self.prev_trigger_left,
                Hand::Right => &mut self.prev_trigger_right,
            };
            let edge = pressed && !*prev;
            *prev = pressed;
            if !edge {
                continue;
            }
            let space = match hand {
                Hand::Left => &self.left_hand_space,
                Hand::Right => &self.right_hand_space,
            };
            let loc = space
                .locate(&self.space, display_time)
                .context("locate hand pose")?;
            if !loc.location_flags.contains(
                xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
            ) {
                continue;
            }
            let Some(anchor) = self.current_anchor else {
                continue;
            };
            if let Some((u, v)) = ray_quad_intersect(&loc.pose, &anchor, self.quad_size) {
                hits.push(ClickHit { u, v, hand });
            }
        }
        Ok(hits)
    }

    pub fn fire_haptic(&self, hand: Hand, duration: std::time::Duration) -> Result<()> {
        // TODO(openxr-headset): route via subaction path so the buzz lands
        // on the right controller (right now both hands buzz together).
        let _ = hand;
        let event = xr::HapticVibration::new()
            .amplitude(1.0)
            .frequency(xr::FREQUENCY_UNSPECIFIED)
            .duration(xr::Duration::from_nanos(duration.as_nanos() as i64));
        self.haptic_action
            .apply_feedback(&self.session, xr::Path::NULL, &event)
            .context("xrApplyHapticFeedback")?;
        Ok(())
    }
}

/// Read the JSON manifest path the OpenXR loader uses to pick the active
/// runtime. Returns a short human-readable name (filename basename) for
/// surfacing in error messages. Best-effort — silently returns `None` if
/// the registry key is missing or unreadable.
fn read_active_openxr_runtime() -> Option<String> {
    use std::process::Command;
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Khronos\OpenXR\1",
            "/v",
            "ActiveRuntime",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("ActiveRuntime"))?;
    let path = line.split_whitespace().last()?;
    let basename = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    Some(basename.to_string())
}

// ---------------------------------------------------------------------------
// OpenXR loader discovery
// ---------------------------------------------------------------------------

/// `openxr_loader.dll` is the small bootstrap DLL that finds whichever
/// OpenXR runtime is set as active in the registry. Khronos publishes a
/// redistributable version; SteamVR also ships its own copy. We try the
/// default loader path first (PATH + exe dir), then fall back to
/// well-known install locations so users don't have to manually copy the
/// DLL.
fn load_openxr_entry() -> Result<xr::Entry> {
    // Default search: PATH + exe directory.
    if let Ok(entry) = unsafe { xr::Entry::load() } {
        return Ok(entry);
    }
    let candidates = [
        // Khronos's redistributable install path.
        r"C:\Program Files\Common Files\Khronos\OpenXR\1\x86_64\openxr_loader.dll",
        // SteamVR ships a loader as part of its install.
        r"C:\Program Files (x86)\Steam\steamapps\common\SteamVR\bin\win64\openxr_loader.dll",
    ];
    for path in candidates {
        let p = std::path::Path::new(path);
        if !p.exists() {
            continue;
        }
        match unsafe { xr::Entry::load_from(p) } {
            Ok(entry) => {
                tracing::info!(loader = path, "loaded OpenXR loader from fallback location");
                return Ok(entry);
            }
            Err(e) => {
                tracing::warn!(loader = path, error = ?e, "loader present but failed to load");
            }
        }
    }
    anyhow::bail!(
        "openxr_loader.dll not found. Install the Khronos OpenXR redistributable, or \
         ensure SteamVR is installed at its default location."
    )
}

// ---------------------------------------------------------------------------
// D3D11 device + swapchain helpers
// ---------------------------------------------------------------------------

fn create_d3d11_device(adapter_luid: LUID) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let target_luid = ((adapter_luid.HighPart as i64) << 32) | (adapter_luid.LowPart as i64);
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut adapter_to_use: Option<IDXGIAdapter1> = None;
    let mut i = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let luid =
                ((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64);
            if luid == target_luid {
                adapter_to_use = Some(adapter);
                break;
            }
        }
        i += 1;
    }
    let adapter = adapter_to_use.context("no DXGI adapter matched the OpenXR-required LUID")?;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
    }
    Ok((
        device.context("D3D11CreateDevice yielded null device")?,
        context.context("D3D11CreateDevice yielded null context")?,
    ))
}

fn make_swapchain(
    session: &xr::Session<xr::D3D11>,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(
    xr::Swapchain<xr::D3D11>,
    Vec<*mut std::ffi::c_void>,
    ID3D11Texture2D,
)> {
    let info = xr::SwapchainCreateInfo::<xr::D3D11> {
        create_flags: xr::SwapchainCreateFlags::EMPTY,
        usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
        format: DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
        sample_count: 1,
        width,
        height,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    };
    let swapchain = session
        .create_swapchain(&info)
        .context("xrCreateSwapchain")?;
    let images = swapchain.enumerate_images().context("enumerate images")?;

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .context("CreateTexture2D(staging)")?;
    }
    Ok((
        swapchain,
        images,
        staging.context("staging texture creation returned null")?,
    ))
}

// ---------------------------------------------------------------------------
// Composition layer + bindings + geometry
// ---------------------------------------------------------------------------

/// Owns the raw `CompositionLayerQuad` struct so the pointer we hand the
/// runtime stays valid through `xrEndFrame`. The lifetime parameter ties
/// it to the swapchain + space references baked into the struct.
struct QuadLayer<'a> {
    raw: xr::sys::CompositionLayerQuad,
    _phantom: std::marker::PhantomData<&'a ()>,
}
impl<'a> QuadLayer<'a> {
    /// # Safety
    /// `xr::CompositionLayerBase<G>` is a `#[repr(transparent)]` newtype
    /// over the XR base-header pointer the runtime walks; reinterpreting
    /// our owned struct as that type is sound because we keep the owning
    /// `QuadLayer` alive for the duration of `xrEndFrame`.
    unsafe fn as_ref(&self) -> &xr::CompositionLayerBase<'a, xr::D3D11> {
        unsafe { &*(&self.raw as *const _ as *const xr::CompositionLayerBase<'a, xr::D3D11>) }
    }
}

fn build_quad_layer<'a>(
    pose: xr::Posef,
    size: (f32, f32),
    swapchain: &'a xr::Swapchain<xr::D3D11>,
    extent: (u32, u32),
    space: &'a xr::Space,
) -> QuadLayer<'a> {
    let sub = xr::sys::SwapchainSubImage {
        swapchain: swapchain.as_raw(),
        image_rect: xr::sys::Rect2Di {
            offset: xr::sys::Offset2Di { x: 0, y: 0 },
            extent: xr::sys::Extent2Di {
                width: extent.0 as i32,
                height: extent.1 as i32,
            },
        },
        image_array_index: 0,
    };
    let raw = xr::sys::CompositionLayerQuad {
        ty: xr::sys::CompositionLayerQuad::TYPE,
        next: std::ptr::null(),
        layer_flags: xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
        space: space.as_raw(),
        eye_visibility: xr::EyeVisibility::BOTH,
        sub_image: sub,
        pose,
        size: xr::sys::Extent2Df {
            width: size.0,
            height: size.1,
        },
    };
    QuadLayer {
        raw,
        _phantom: std::marker::PhantomData,
    }
}

fn suggest_simple_controller_bindings(
    instance: &xr::Instance,
    click: &xr::Action<bool>,
    pose: &xr::Action<xr::Posef>,
    haptic: &xr::Action<xr::Haptic>,
) -> Result<()> {
    let profile = instance.string_to_path(INTERACTION_PROFILE)?;
    let left_trigger = instance.string_to_path("/user/hand/left/input/select/click")?;
    let right_trigger = instance.string_to_path("/user/hand/right/input/select/click")?;
    let left_pose = instance.string_to_path("/user/hand/left/input/aim/pose")?;
    let right_pose = instance.string_to_path("/user/hand/right/input/aim/pose")?;
    let left_haptic = instance.string_to_path("/user/hand/left/output/haptic")?;
    let right_haptic = instance.string_to_path("/user/hand/right/output/haptic")?;
    let bindings = [
        xr::Binding::new(click, left_trigger),
        xr::Binding::new(click, right_trigger),
        xr::Binding::new(pose, left_pose),
        xr::Binding::new(pose, right_pose),
        xr::Binding::new(haptic, left_haptic),
        xr::Binding::new(haptic, right_haptic),
    ];
    instance.suggest_interaction_profile_bindings(profile, &bindings)?;
    Ok(())
}

/// Ray-cast the hand's `+forward` (its `-Z` axis in OpenXR's pose
/// convention) at the quad plane. Returns texture-space `(u, v)` of the
/// intersection if it lands inside the quad's bounds, else `None`.
fn ray_quad_intersect(
    hand: &xr::Posef,
    quad: &xr::Posef,
    quad_size: (f32, f32),
) -> Option<(f32, f32)> {
    // TODO(openxr-headset): verify this against a real controller pose;
    // the quad's "front" normal and OpenXR's "aim pose forward = -Z"
    // conventions need confirming in-headset.
    let hand_pos = [hand.position.x, hand.position.y, hand.position.z];
    let hand_q = [
        hand.orientation.x,
        hand.orientation.y,
        hand.orientation.z,
        hand.orientation.w,
    ];
    let ray_dir = quat_rotate_vec(&hand_q, &[0.0, 0.0, -1.0]);

    let quad_pos = [quad.position.x, quad.position.y, quad.position.z];
    let quad_q = [
        quad.orientation.x,
        quad.orientation.y,
        quad.orientation.z,
        quad.orientation.w,
    ];
    let normal = quat_rotate_vec(&quad_q, &[0.0, 0.0, 1.0]);

    let denom = dot(&ray_dir, &normal);
    if denom.abs() < 1e-5 {
        return None;
    }
    let to_plane = [
        quad_pos[0] - hand_pos[0],
        quad_pos[1] - hand_pos[1],
        quad_pos[2] - hand_pos[2],
    ];
    let t = dot(&to_plane, &normal) / denom;
    if t < 0.0 {
        return None; // pointing away from the quad
    }
    let hit_world = [
        hand_pos[0] + t * ray_dir[0],
        hand_pos[1] + t * ray_dir[1],
        hand_pos[2] + t * ray_dir[2],
    ];
    let local = [
        hit_world[0] - quad_pos[0],
        hit_world[1] - quad_pos[1],
        hit_world[2] - quad_pos[2],
    ];
    let inv_q = [-quad_q[0], -quad_q[1], -quad_q[2], quad_q[3]];
    let local = quat_rotate_vec(&inv_q, &local);
    let half_w = quad_size.0 * 0.5;
    let half_h = quad_size.1 * 0.5;
    if local[0] < -half_w || local[0] > half_w || local[1] < -half_h || local[1] > half_h {
        return None;
    }
    // Quad UV: x maps to u in [0,1] left→right, y maps to v in [0,1]
    // top→bottom (so we flip y).
    let u = (local[0] + half_w) / quad_size.0;
    let v = 1.0 - (local[1] + half_h) / quad_size.1;
    Some((u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)))
}

fn dot(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Rotate vector `v` by quaternion `q = (x, y, z, w)`. Standard formula:
/// `v' = v + 2 * cross(q.xyz, cross(q.xyz, v) + q.w * v)`.
fn quat_rotate_vec(q: &[f32; 4], v: &[f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let s = q[3];
    let uv = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let uuv = [
        u[1] * uv[2] - u[2] * uv[1],
        u[2] * uv[0] - u[0] * uv[2],
        u[0] * uv[1] - u[1] * uv[0],
    ];
    [
        v[0] + 2.0 * (s * uv[0] + uuv[0]),
        v[1] + 2.0 * (s * uv[1] + uuv[1]),
        v[2] + 2.0 * (s * uv[2] + uuv[2]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yawed_quad(yaw_rad: f32) -> xr::Posef {
        let h = yaw_rad * 0.5;
        xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: h.sin(),
                z: 0.0,
                w: h.cos(),
            },
            position: xr::Vector3f {
                x: 0.0,
                y: 1.5,
                z: -1.5,
            },
        }
    }

    #[test]
    fn ray_center_hits_center_uv() {
        // Hand at origin, pointing at -Z (default orientation: identity quat).
        let hand = xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: xr::Vector3f {
                x: 0.0,
                y: 1.5,
                z: 0.0,
            },
        };
        let quad = yawed_quad(0.0);
        let (u, v) = ray_quad_intersect(&hand, &quad, (1.0, 1.0)).expect("should hit");
        assert!((u - 0.5).abs() < 0.01, "u was {u}");
        assert!((v - 0.5).abs() < 0.01, "v was {v}");
    }

    #[test]
    fn ray_pointing_away_misses() {
        // Hand at origin facing +Z (back to the quad).
        let h = std::f32::consts::FRAC_PI_2;
        let hand = xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: h.sin(),
                z: 0.0,
                w: h.cos(),
            },
            position: xr::Vector3f {
                x: 0.0,
                y: 1.5,
                z: 0.0,
            },
        };
        let quad = yawed_quad(0.0);
        assert!(ray_quad_intersect(&hand, &quad, (1.0, 1.0)).is_none());
    }
}
