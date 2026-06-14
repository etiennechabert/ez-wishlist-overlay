//! Windows.Media.Ocr (WinRT) wrapper — the pre-#181 OCR engine, kept behind
//! the off-by-default `ocr-winrt` cargo feature as an A/B fallback while the
//! PP-OCR engine ([`crate::ocr::engine_ppocr`]) bakes. Build with
//! `--features ocr-winrt` to swap it back in; delete this module once the
//! migration has soaked in the field.
//!
//! Known structural limits that motivated the replacement: closed API (no
//! vocabulary, confidence, or training hooks; needs an installed OS language
//! pack) and a document-style line detector whose graphics suppression
//! deterministically eats short item names next to busy icons (RAM, CD,
//! CPU Fan — the stash scan captured 4/17 RAM tiles).
//!
//! Why this dance: `BitmapDecoder` would be the direct WinRT path, but it
//! wants the file as a `StorageFile` (sandboxed) or an `IRandomAccessStream`
//! we'd have to materialize. In-process decoding is simpler.

use crate::ocr::{OcrRect, OcrWord};
use anyhow::{Context, Result};
use image::DynamicImage;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// Run Windows.Media.Ocr on an already-decoded image. Returns the
/// word-level boxes in pixel coordinates (downstream code only uses
/// these — the full `Text` and image dimensions are dropped).
pub fn recognize_image(img: &DynamicImage) -> Result<Vec<OcrWord>> {
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        anyhow::bail!("zero-sized image cannot be OCR'd");
    }

    // WinRT wants BGRA8 with the alpha channel marked premultiplied (the
    // engine doesn't actually use alpha, but it validates the descriptor).
    let mut bgra = rgba.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let bitmap = build_software_bitmap(width, height, &bgra)?;

    // User-profile language list: works on a default Win10/11 install with
    // en-US. Pinning to a specific language risks a null return on machines
    // without that pack.
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .context("no OCR language pack installed — Settings → Time & Language → Language")?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .context("RecognizeAsync")?
        .get()
        .context("await OCR result")?;

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

    tracing::debug!(width, height, words = words.len(), "OCR finished");
    Ok(words)
}

/// Materialize a BGRA8 `SoftwareBitmap` from raw pixels. Allocates a WinRT
/// buffer via `DataWriter` (the only ergonomic path to `IBuffer` without
/// unsafe ABI calls), copies the pixels in, then asks `SoftwareBitmap` to
/// take a view of them.
fn build_software_bitmap(width: u32, height: u32, bgra: &[u8]) -> Result<SoftwareBitmap> {
    let writer = DataWriter::new().context("alloc DataWriter")?;
    writer.WriteBytes(bgra).context("DataWriter::WriteBytes")?;
    let buf = writer.DetachBuffer().context("DataWriter::DetachBuffer")?;

    SoftwareBitmap::CreateCopyFromBuffer(
        &buf,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )
    .context("CreateCopyFromBuffer")
}
