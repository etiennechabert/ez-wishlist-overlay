//! VR screenshot capture via the SteamVR compositor mirror texture.
//!
//! Steam's F12 screenshot is a JPEG that mangles the chunky pixel-art digit
//! font we need to OCR. The compositor's mirror texture, exposed by
//! `IVRCompositor::GetMirrorTextureD3D11`, is the lossless eye render-target
//! the game submitted, *before* JPEG compression and *before* SteamVR's
//! overlays (including this app's own overlay) are composited on top. That
//! makes it ideal for grabbing a clean shot of the in-game UI for OCR.
//!
//! This module is Windows-only because it leans on D3D11. On other targets
//! the entire `vr::overlay` + `vr::capture` chain is `cfg`'d out.
//!
//! The `openvr` 0.9.0 safe wrapper does not expose `GetMirrorTextureD3D11`,
//! so we reach into `openvr_sys` directly and look up the
//! `VR_IVRCompositor_FnTable` ourselves via `VR_GetGenericInterface`. This is
//! the same path the safe wrapper uses for every other interface — we just
//! happen to need a method it doesn't surface.

use anyhow::{anyhow, Context as _, Result};
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process capture sequence number. Stamped on every trace line
/// when `settings.ocr_capture_trace` is on so the user can cross-
/// reference logs from one capture across the VR thread, OCR worker,
/// and pipeline. Always incremented; the counter is essentially free
/// when no tracing fires.
static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// FNV-1a 64-bit hash over a byte slice. Used by the
/// `ocr_capture_trace` path to fingerprint pixel buffers and the
/// encoded PNG so consecutive captures can be compared: two captures
/// reporting the same `pixel_fnv` means the compositor mirror handed
/// back the same buffer twice (the classic "previous-screenshot"
/// bug); a `decoded_fnv` mismatch against the capture's `rgb_fnv`
/// means the file mutated between write and read.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

use openvr_sys as sys;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONASTERISK, MB_ICONHAND};

/// Audible feedback for the capture button — fires from the VR worker thread
/// so the user hears it even with the desktop window minimized / out of focus.
/// `success=true` plays the system "ding" (information), `false` plays the
/// "error" sound. Both are async — they don't block the render loop.
pub fn play_capture_done_beep(success: bool) {
    let style = if success {
        MB_ICONASTERISK
    } else {
        MB_ICONHAND
    };
    // SAFETY: MessageBeep has no preconditions; it queues a sound on the
    // shell audio thread and returns immediately.
    unsafe {
        let _ = MessageBeep(style);
    }
}

/// Which compositor mirror eye texture to capture. The default is the
/// right eye — empirically that channel stayed in sync with in-game
/// state when the left-eye mirror was leaking the previous frame on
/// consecutive captures (the "second screenshot OCR's the first" bug
/// we kept chasing). Configurable from [`crate::settings::Settings`].
#[derive(Clone, Copy, Debug)]
pub enum CaptureEye {
    Left,
    Right,
}

impl From<crate::settings::CaptureEye> for CaptureEye {
    fn from(s: crate::settings::CaptureEye) -> Self {
        match s {
            crate::settings::CaptureEye::Left => CaptureEye::Left,
            crate::settings::CaptureEye::Right => CaptureEye::Right,
        }
    }
}

impl CaptureEye {
    fn sys(self) -> sys::EVREye {
        match self {
            CaptureEye::Left => sys::EVREye_Eye_Left,
            CaptureEye::Right => sys::EVREye_Eye_Right,
        }
    }
    fn label(self) -> &'static str {
        match self {
            CaptureEye::Left => "left",
            CaptureEye::Right => "right",
        }
    }
}

/// Pull the configured eye's mirror texture from the SteamVR compositor
/// and write it to `out_path` as PNG. Caller must guarantee `VR_Init`
/// has already run on this process (the OverlaySession constructor does).
///
/// Defaults to the right eye (settings-configurable); the left-eye
/// mirror lagged the actual compositor frame by multiple slots on
/// some headsets. The right eye is stable for repeated captures of
/// the same panel, but on the FIRST capture after a panel switch
/// even it returns the frame the compositor wrote one slot ago. We
/// guard against that with a single drain cycle (acquire → release →
/// sleep one vsync → acquire) so the keeper read lands on a
/// freshly-composited frame.
///
/// Every D3D11 resource is reallocated per call (fresh device, fresh
/// fn-table lookup, fresh SRVs, fresh staging texture) so there's no
/// carry-over between captures.
///
/// When `trace` is true, emits the full per-step diagnostic log
/// (capture_seq, pointers, FNV fingerprints of every byte buffer,
/// compositor frame timing, opposite-eye probe). Off by default —
/// the FNV passes hash ~30 MB per capture and the logs are very
/// chatty.
pub fn capture_compositor_mirror_to_png(
    out_path: &Path,
    eye: CaptureEye,
    trace: bool,
) -> Result<()> {
    let capture_seq = if trace {
        let s = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            capture_seq = s,
            out_path = %out_path.display(),
            eye = eye.label(),
            "capture: ENTER capture_compositor_mirror_to_png"
        );
        s
    } else {
        0
    };
    let t_start = if trace {
        Some(std::time::Instant::now())
    } else {
        None
    };

    let t_d3d_start = std::time::Instant::now();
    let d3d = D3d11Context::create().context("D3D11CreateDevice")?;
    if trace {
        tracing::info!(
            capture_seq,
            device_ptr = format!("{:p}", d3d.device.as_raw()),
            context_ptr = format!("{:p}", d3d.context.as_raw()),
            elapsed_ms = t_d3d_start.elapsed().as_millis() as u64,
            "capture: D3D11 device created"
        );
    }
    let fn_table = lookup_compositor_fn_table().context("look up IVRCompositor fn-table")?;
    if trace {
        // Compositor frame timing. If two consecutive captures share
        // `frame_index`, the compositor itself is stuck on a frame.
        let mut t: sys::Compositor_FrameTiming = unsafe { std::mem::zeroed() };
        t.m_nSize = std::mem::size_of::<sys::Compositor_FrameTiming>() as u32;
        let got = unsafe {
            (*fn_table)
                .GetFrameTiming
                .map(|f| f(&mut t as *mut _, 0))
                .unwrap_or(false)
        };
        if got {
            tracing::info!(
                capture_seq,
                fn_table_ptr = format!("{:p}", fn_table),
                frame_index = t.m_nFrameIndex,
                frame_system_time_s = t.m_flSystemTimeInSeconds,
                num_dropped = t.m_nNumDroppedFrames,
                num_mispresented = t.m_nNumMisPresented,
                "capture: fn-table looked up + compositor frame timing"
            );
        }
    }

    // Drain the mirror swap chain before the real read. The compositor
    // exposes frames through a small queue and returns the next slot
    // each call; in field reports a single drain cycle didn't always
    // walk past every stale entry — the user navigated from panel A
    // to panel B, captured, and STILL got A's frame back. Three
    // cycles (acquire → release → sleep) at 60 ms each guarantees
    // walking past a 3-slot-deep queue plus gives the compositor real
    // time to push the latest frame into the buffer the keeper
    // acquire below will pick up. Total ~180 ms — imperceptible
    // against the existing GPU readback + PNG-encode latency
    // (~150 ms + ~3.7 s respectively).
    const PRIMER_CYCLES: usize = 3;
    const PRIMER_SLEEP_MS: u64 = 60;
    for _ in 0..PRIMER_CYCLES {
        let _primer = MirrorTexture::acquire(fn_table, &d3d.device, eye.sys())
            .with_context(|| format!("GetMirrorTextureD3D11({}) primer", eye.label()))?;
        drop(_primer);
        std::thread::sleep(std::time::Duration::from_millis(PRIMER_SLEEP_MS));
    }

    let t_acq_start = std::time::Instant::now();
    let mirror = MirrorTexture::acquire(fn_table, &d3d.device, eye.sys())
        .with_context(|| format!("GetMirrorTextureD3D11({})", eye.label()))?;
    if trace {
        tracing::info!(
            capture_seq,
            eye = eye.label(),
            srv_ptr = format!("{:p}", mirror.srv_raw),
            elapsed_ms = t_acq_start.elapsed().as_millis() as u64,
            "capture: GetMirrorTextureD3D11 (keeper) returned SRV"
        );
        // Opposite-eye probe. If the captured eye's hash matches the
        // previous capture but the OTHER eye's hash advances, the
        // captured eye is lagging while the other is fresh.
        let other = match eye {
            CaptureEye::Left => sys::EVREye_Eye_Right,
            CaptureEye::Right => sys::EVREye_Eye_Left,
        };
        let other_label = match eye {
            CaptureEye::Left => "right",
            CaptureEye::Right => "left",
        };
        if let Ok(other_mirror) = MirrorTexture::acquire(fn_table, &d3d.device, other) {
            unsafe {
                if let Ok(res) = other_mirror.srv().GetResource() {
                    if let Ok(other_tex) = res.cast::<ID3D11Texture2D>() {
                        let mut odesc = D3D11_TEXTURE2D_DESC::default();
                        other_tex.GetDesc(&mut odesc);
                        if let Ok(opix) = readback_texture(&d3d, &other_tex, &odesc) {
                            let probe_len = opix.len().min(65536);
                            let other_probe_fnv = fnv1a64(&opix[..probe_len]);
                            tracing::info!(
                                capture_seq,
                                eye = other_label,
                                probe_fnv = format!("{other_probe_fnv:#018x}"),
                                "capture: OTHER-eye probe"
                            );
                        }
                    }
                }
            }
        }
    }

    let texture = unsafe { mirror.srv().GetResource() }
        .context("ID3D11ShaderResourceView::GetResource")?
        .cast::<ID3D11Texture2D>()
        .context("SRV resource isn't an ID3D11Texture2D")?;

    let desc = unsafe {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut d);
        d
    };

    let t_read_start = std::time::Instant::now();
    let pixels = readback_texture(&d3d, &texture, &desc).context("readback")?;
    if trace {
        let pixel_fnv = fnv1a64(&pixels);
        let mid = (pixels.len() / 8) * 4;
        let last_start = pixels.len().saturating_sub(4);
        tracing::info!(
            capture_seq,
            width = desc.Width,
            height = desc.Height,
            format = desc.Format.0,
            pixel_bytes = pixels.len(),
            readback_ms = t_read_start.elapsed().as_millis() as u64,
            pixel_fnv = format!("{pixel_fnv:#018x}"),
            first_rgba = format!("[{},{},{},{}]", pixels[0], pixels[1], pixels[2], pixels[3]),
            centre_rgba = format!(
                "[{},{},{},{}]",
                pixels[mid],
                pixels[mid + 1],
                pixels[mid + 2],
                pixels[mid + 3]
            ),
            last_rgba = format!(
                "[{},{},{},{}]",
                pixels[last_start],
                pixels[last_start + 1],
                pixels[last_start + 2],
                pixels[last_start + 3]
            ),
            "capture: readback complete + pixel fingerprint"
        );
    }

    // Save as RGB8: the compositor's eye render-target alpha channel is
    // garbage / always zero in practice (the compositor doesn't use it for
    // mirror-texture output), and an alpha-0 PNG renders as fully transparent
    // in every image viewer — which looks like a blank screenshot. Dropping
    // the alpha entirely sidesteps that.
    let mut rgb = Vec::with_capacity((desc.Width * desc.Height * 3) as usize);
    for chunk in pixels.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    let rgb_fnv = if trace { Some(fnv1a64(&rgb)) } else { None };

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let img = image::RgbImage::from_raw(desc.Width, desc.Height, rgb)
        .ok_or_else(|| anyhow!("RgbImage::from_raw: pixel buffer size mismatch"))?;
    let t_save_start = std::time::Instant::now();
    img.save(out_path)
        .with_context(|| format!("writing PNG to {}", out_path.display()))?;

    if trace {
        let png_fnv = std::fs::read(out_path).ok().map(|b| fnv1a64(&b));
        let png_size = std::fs::metadata(out_path).ok().map(|m| m.len());
        tracing::info!(
            capture_seq,
            path = %out_path.display(),
            w = desc.Width,
            h = desc.Height,
            eye = eye.label(),
            rgb_fnv = rgb_fnv.map(|v| format!("{v:#018x}")),
            png_size = ?png_size,
            png_fnv = ?png_fnv.map(|v| format!("{v:#018x}")),
            save_ms = t_save_start.elapsed().as_millis() as u64,
            total_ms = t_start.map(|s| s.elapsed().as_millis() as u64),
            "capture: EXIT — PNG written (full fingerprint)"
        );
    } else {
        tracing::info!(
            path = %out_path.display(),
            w = desc.Width,
            h = desc.Height,
            eye = eye.label(),
            "captured compositor mirror"
        );
    }
    Ok(())
}

/// Owned D3D11 device + immediate context, scoped to one capture call.
struct D3d11Context {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl D3d11Context {
    fn create() -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?
        }
        Ok(Self {
            device: device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?,
            context: context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?,
        })
    }
}

/// RAII wrapper around the SRV handed out by GetMirrorTextureD3D11. On drop
/// we call ReleaseMirrorTextureD3D11 so the compositor can recycle it. We
/// store the SRV as a raw pointer and only borrow a typed wrapper when we
/// need to call methods on it — that keeps `ReleaseMirrorTextureD3D11` as
/// the only thing that ever Release()es the underlying COM object (a typed
/// wrapper would double-free).
struct MirrorTexture {
    srv_raw: *mut c_void,
    release: unsafe extern "C" fn(*mut c_void),
}

impl MirrorTexture {
    fn acquire(
        fn_table: *const sys::VR_IVRCompositor_FnTable,
        device: &ID3D11Device,
        eye: sys::EVREye,
    ) -> Result<Self> {
        let get_mirror = unsafe { (*fn_table).GetMirrorTextureD3D11 }
            .ok_or_else(|| anyhow!("GetMirrorTextureD3D11 slot empty in fn-table"))?;
        let release_mirror = unsafe { (*fn_table).ReleaseMirrorTextureD3D11 }
            .ok_or_else(|| anyhow!("ReleaseMirrorTextureD3D11 slot empty in fn-table"))?;

        let mut srv_raw: *mut c_void = ptr::null_mut();
        let device_raw = device.as_raw();
        let err = unsafe { get_mirror(eye, device_raw, &mut srv_raw) };
        if err != sys::EVRCompositorError_VRCompositorError_None {
            return Err(anyhow!(
                "GetMirrorTextureD3D11 returned EVRCompositorError {err}"
            ));
        }
        if srv_raw.is_null() {
            return Err(anyhow!("GetMirrorTextureD3D11 succeeded but SRV is null"));
        }

        Ok(Self {
            srv_raw,
            release: release_mirror,
        })
    }

    /// Borrow the SRV as a typed wrapper for method calls. The borrow's
    /// lifetime is tied to `self`, so Drop can't run while a caller holds
    /// the reference.
    fn srv(&self) -> &ID3D11ShaderResourceView {
        // SAFETY: `srv_raw` was checked non-null in `acquire` and remains
        // valid until `Drop` calls ReleaseMirrorTextureD3D11. `from_raw_borrowed`
        // does NOT take ownership of the AddRef, so dropping the returned
        // reference does not Release the COM object.
        unsafe {
            ID3D11ShaderResourceView::from_raw_borrowed(&self.srv_raw)
                .expect("srv_raw was non-null at acquire time")
        }
    }
}

impl Drop for MirrorTexture {
    fn drop(&mut self) {
        unsafe { (self.release)(self.srv_raw) };
    }
}

/// Resolve the `IVRCompositor` function table via the same `VR_GetGenericInterface`
/// path the `openvr` safe wrapper uses internally — we just need a method it
/// doesn't expose. Requires VR_Init to have already run on this process.
fn lookup_compositor_fn_table() -> Result<*const sys::VR_IVRCompositor_FnTable> {
    // OpenVR's convention: ask for "FnTable:<interface-version-string>" and
    // you get back a pointer to a struct of function pointers. Asking for the
    // bare version string would get the C++ vtable instead, which we can't
    // call from Rust.
    let mut magic: Vec<u8> = b"FnTable:".to_vec();
    magic.extend_from_slice(sys::IVRCompositor_Version);
    let mut error: sys::EVRInitError = sys::EVRInitError_VRInitError_None;
    let ptr = unsafe { sys::VR_GetGenericInterface(magic.as_ptr() as *const i8, &mut error) };
    if error != sys::EVRInitError_VRInitError_None {
        return Err(anyhow!(
            "VR_GetGenericInterface(IVRCompositor) error code {error}"
        ));
    }
    if ptr == 0 {
        return Err(anyhow!(
            "VR_GetGenericInterface(IVRCompositor) returned null"
        ));
    }
    Ok(ptr as *const sys::VR_IVRCompositor_FnTable)
}

/// Copy the GPU-side texture into a staging texture, map it, and pull the
/// pixels into a tightly-packed RGBA `Vec<u8>` regardless of GPU row pitch.
fn readback_texture(
    d3d: &D3d11Context,
    texture: &ID3D11Texture2D,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Result<Vec<u8>> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        d3d.device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
    };
    let staging = staging.ok_or_else(|| anyhow!("CreateTexture2D(staging) returned None"))?;

    unsafe { d3d.context.CopyResource(&staging, texture) };
    // Force the GPU to finish the CopyResource before we Map the
    // staging texture. Without this Flush the D3D11 driver may defer
    // the copy and Map can read torn or pre-copy contents.
    unsafe { d3d.context.Flush() };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        d3d.context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
    };

    let width = desc.Width as usize;
    let height = desc.Height as usize;
    let row_pitch = mapped.RowPitch as usize;
    let mut pixels = vec![0u8; width * height * 4];
    unsafe {
        let src = mapped.pData as *const u8;
        for y in 0..height {
            ptr::copy_nonoverlapping(
                src.add(y * row_pitch),
                pixels.as_mut_ptr().add(y * width * 4),
                width * 4,
            );
        }
        d3d.context.Unmap(&staging, 0);
    }

    // The PNG encoder wants RGBA8. The compositor's mirror texture is usually
    // R8G8B8A8_UNORM (or its sRGB sibling) — direct copy. If it ever hands us
    // BGRA, swap channels. TYPELESS variants share the byte layout of their
    // typed siblings (only the channel-interpretation hint differs), so we
    // accept them as identity for the R-family and channel-swap for the
    // B-family — the actual sample data lands in the right place either way.
    let fmt = desc.Format;
    let is_rgba = fmt == DXGI_FORMAT_R8G8B8A8_UNORM
        || fmt == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
        || fmt == DXGI_FORMAT_R8G8B8A8_TYPELESS;
    let is_bgra = fmt == DXGI_FORMAT_B8G8R8A8_UNORM
        || fmt == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        || fmt == DXGI_FORMAT_B8G8R8A8_TYPELESS;
    if is_bgra {
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
    } else if !is_rgba {
        tracing::warn!(
            format = fmt.0,
            "unrecognized mirror texture format — saving raw bytes"
        );
    }

    Ok(pixels)
}
