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

use openvr_sys as sys;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONASTERISK, MB_ICONHAND};

/// Audible feedback for the capture button — fires from the VR worker thread
/// so the user hears it even with the desktop window minimized / out of focus.
/// `success=true` plays the system "ding" (information), `false` plays the
/// "error" sound. Both are async — they don't block the render loop.
pub fn play_capture_done_beep(success: bool) {
    let style = if success { MB_ICONASTERISK } else { MB_ICONHAND };
    // SAFETY: MessageBeep has no preconditions; it queues a sound on the
    // shell audio thread and returns immediately.
    unsafe {
        let _ = MessageBeep(style);
    }
}

/// Pull the left-eye mirror texture from the SteamVR compositor and write it
/// to `out_path` as PNG. Caller must guarantee `VR_Init` has already run on
/// this process (the OverlaySession constructor does).
pub fn capture_compositor_mirror_to_png(out_path: &Path) -> Result<()> {
    let d3d = D3d11Context::create().context("D3D11CreateDevice")?;
    let fn_table = lookup_compositor_fn_table().context("look up IVRCompositor fn-table")?;
    let mirror = MirrorTexture::acquire(fn_table, &d3d.device, sys::EVREye_Eye_Left)
        .context("GetMirrorTextureD3D11")?;

    let texture = unsafe { mirror.srv().GetResource() }
        .context("ID3D11ShaderResourceView::GetResource")?
        .cast::<ID3D11Texture2D>()
        .context("SRV resource isn't an ID3D11Texture2D")?;

    let desc = unsafe {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut d);
        d
    };

    let pixels = readback_texture(&d3d, &texture, &desc).context("readback")?;

    // Diagnostic: sample two pixels to confirm RGB carries data even when
    // PNG viewers render alpha=0 as transparent (which looks blank-white).
    if pixels.len() >= 4 {
        let mid = (pixels.len() / 8) * 4; // roughly the centre row's start
        tracing::info!(
            "capture sample pixels — first RGBA=[{},{},{},{}], centre RGBA=[{},{},{},{}]",
            pixels[0], pixels[1], pixels[2], pixels[3],
            pixels[mid], pixels[mid + 1], pixels[mid + 2], pixels[mid + 3],
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

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let img = image::RgbImage::from_raw(desc.Width, desc.Height, rgb)
        .ok_or_else(|| anyhow!("RgbImage::from_raw: pixel buffer size mismatch"))?;
    img.save(out_path)
        .with_context(|| format!("writing PNG to {}", out_path.display()))?;

    tracing::info!(
        path = %out_path.display(),
        w = desc.Width,
        h = desc.Height,
        format = desc.Format.0,
        "captured compositor mirror"
    );
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
            return Err(anyhow!("GetMirrorTextureD3D11 returned EVRCompositorError {err}"));
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
        return Err(anyhow!("VR_GetGenericInterface(IVRCompositor) returned null"));
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
    // BGRA, swap channels.
    let fmt = desc.Format;
    if fmt == DXGI_FORMAT_B8G8R8A8_UNORM || fmt == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB {
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
    } else if fmt != DXGI_FORMAT_R8G8B8A8_UNORM && fmt != DXGI_FORMAT_R8G8B8A8_UNORM_SRGB {
        tracing::warn!(format = fmt.0, "unrecognized mirror texture format — saving raw bytes");
    }

    Ok(pixels)
}
