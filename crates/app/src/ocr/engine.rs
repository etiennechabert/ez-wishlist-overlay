//! Windows.Media.Ocr (WinRT) wrapper.
//!
//! Loads an image file via [`image`] (so we can use the same decoder as
//! the rest of the app), converts to a BGRA8 `SoftwareBitmap`, and runs
//! the `OcrEngine` over it. Returns word-level boxes in image pixel
//! coordinates.
//!
//! Why this dance: `BitmapDecoder` from `Storage.Streams` would be the
//! direct WinRT path, but it requires the file as a `StorageFile` (async,
//! sandboxed to known folders) or as an `IRandomAccessStream` we'd have
//! to materialize. Easier to decode in-process with `image` and hand the
//! engine raw BGRA bytes.

use crate::ocr::{OcrRect, OcrResult, OcrWord};
use anyhow::{Context, Result};
use image::DynamicImage;
use std::path::Path;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// Decode the image at `path` and OCR it. Thin wrapper over
/// [`recognize_image`] kept for callers that just want the file path
/// interface (e.g. the manual file-picker dialog).
pub fn recognize_file(path: &Path) -> Result<OcrResult> {
    let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
    recognize_image(&img)
}

/// Run Windows.Media.Ocr on an already-decoded image. The two-pass
/// pipeline calls this once on the full screenshot, then again on each
/// preprocessed per-cell crop without re-loading from disk.
pub fn recognize_image(img: &DynamicImage) -> Result<OcrResult> {
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        anyhow::bail!("zero-sized image cannot be OCR'd");
    }

    // WinRT wants BGRA8 with the alpha channel marked premultiplied (the
    // engine doesn't actually use alpha, but it validates the descriptor).
    // Convert RGBA → BGRA in place; we'd allocate a fresh buffer anyway
    // for the WinRT IBuffer copy below.
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let bitmap = build_software_bitmap(width, height, &bgra)?;

    // Use whatever language packs the user has installed (almost always
    // includes en-US on a default Win10/11). Pinning to a specific
    // language risks a null return on machines without that pack; the
    // user-profile variant picks the best available.
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .context("no OCR language pack installed — Settings → Time & Language → Language")?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .context("RecognizeAsync")?
        .get()
        .context("await OCR result")?;

    let text = result.Text().context("OCR Text")?.to_string_lossy();
    let mut words = Vec::new();

    for line in result.Lines().context("OCR Lines")? {
        for word in line.Words().context("OCR Words")? {
            let rect = word.BoundingRect().context("OCR BoundingRect")?;
            words.push(OcrWord {
                text: word.Text().context("OCR word text")?.to_string_lossy(),
                rect: OcrRect {
                    x: rect.X,
                    y: rect.Y,
                    width: rect.Width,
                    height: rect.Height,
                },
            });
        }
    }

    tracing::debug!(
        width,
        height,
        words = words.len(),
        chars = text.len(),
        "OCR finished",
    );

    Ok(OcrResult {
        image_width: width,
        image_height: height,
        text,
        words,
    })
}

/// Materialize a BGRA8 `SoftwareBitmap` from raw pixels. We allocate a
/// WinRT buffer via `DataWriter` (the only ergonomic path to an
/// `IBuffer` without unsafe ABI calls), copy the pixels in, then ask
/// SoftwareBitmap to take a view of those bytes.
fn build_software_bitmap(width: u32, height: u32, bgra: &[u8]) -> Result<SoftwareBitmap> {
    let writer = DataWriter::new().context("alloc DataWriter")?;
    writer.WriteBytes(bgra).context("DataWriter::WriteBytes")?;
    let buf = writer
        .DetachBuffer()
        .context("DataWriter::DetachBuffer")?;

    // `CreateCopyFromBuffer` returns Bgra8 with the default alpha mode
    // (Premultiplied). The OCR engine accepts that as-is — no Convert
    // needed unless we ever start feeding it non-BGRA8 pixels.
    SoftwareBitmap::CreateCopyFromBuffer(
        &buf,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )
    .context("CreateCopyFromBuffer")
}
