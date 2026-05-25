//! OCR pipeline for the "import upgrade from screenshot" flow.
//!
//! Recognizes text in user-supplied screenshots of in-game hideout / task
//! UI panels and extracts structured upgrade entries (name, level, cost,
//! required items + collected/needed counts). The output feeds the new
//! [`crate::wishlist`] module — see that doc for the broader rationale
//! (upstream scraper is months stale; OCR-from-screenshots replaces it
//! as the source of truth for tracked progress).
//!
//! POC scope: load an image file from disk, OCR it with Windows.Media.Ocr,
//! return raw line/word boxes. Parsing into a structured `CapturedUpgrade`
//! lives in [`parse`]; the GUI dialog that drives it lives in
//! `gui::ocr_dialog`.

#[cfg(target_os = "windows")]
mod engine;
pub mod parse;
pub mod watcher;

#[cfg(target_os = "windows")]
pub use engine::recognize_file;

/// Stub for non-Windows builds so the rest of the crate compiles cleanly
/// on macOS/Linux iteration toolchains. The real implementation needs
/// WinRT and only ships on the Windows target alongside the VR stack.
#[cfg(not(target_os = "windows"))]
pub fn recognize_file(_path: &std::path::Path) -> anyhow::Result<OcrResult> {
    anyhow::bail!("OCR is only supported on Windows in this build")
}

/// Raw OCR output: a flat list of word-level boxes plus the full
/// reading-order text. Bounding rects are in image pixel space (top-left
/// origin, +y down). Confidence is the per-word score Windows surfaces;
/// 1.0 means "high"; missing words come back as a fallback `0.0`.
#[derive(Debug, Clone)]
pub struct OcrResult {
    /// Image dimensions in pixels — handy for the parser when normalizing
    /// or visualizing.
    pub image_width: u32,
    pub image_height: u32,
    /// Full text as the OCR engine read it, line-wrapped. Useful for the
    /// debug pane and for raw_ocr_text persistence in wishlist entries
    /// (lets us re-parse later without re-OCR'ing).
    pub text: String,
    /// One entry per recognized word. Order = OCR reading order.
    pub words: Vec<OcrWord>,
}

#[derive(Debug, Clone)]
pub struct OcrWord {
    pub text: String,
    /// Bounding box in image pixel coordinates.
    pub rect: OcrRect,
}

/// Pixel-space rectangle. Top-left origin; +x right, +y down.
#[derive(Debug, Clone, Copy)]
pub struct OcrRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl OcrRect {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }
    pub fn center_x(&self) -> f32 {
        self.x + self.width * 0.5
    }
    pub fn center_y(&self) -> f32 {
        self.y + self.height * 0.5
    }
}
