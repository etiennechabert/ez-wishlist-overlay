//! OCR pipeline: turn a VR-mirror screenshot into an `AppState.collected`
//! update by reading the Facility Upgrade panel.
//!
//! Architecture: lossless input is provided upstream by [`crate::vr::capture`]
//! (compositor mirror-texture PNG). We trust [`crate::data::GameData`] as the
//! source of truth for which items each upgrade requires, in what order. OCR
//! therefore only does the minimum:
//!
//! 1. Detect that the screenshot is an upgrade panel (anchor: "Need to submit items").
//! 2. Identify which upgrade — strict match against `module.name` from data.json
//!    (Levenshtein distance only tolerates OCR character errors, never dataset drift).
//! 3. For each requirement slot (known + ordered from data.json), read the owned
//!    count via per-digit template matching.
//!
//! Returns an [`OcrOutcome`] the caller can apply to `AppState`.

// All OCR helpers compile only on Windows. The pipeline's `Ok(None)`
// stub on other targets means none of these are reachable there, and
// clippy on Linux gets unhappy about every `pub fn` looking unused
// when the only caller is the Windows-gated pipeline.
#[cfg(target_os = "windows")]
pub mod anchor;
#[cfg(target_os = "windows")]
pub mod engine;
#[cfg(target_os = "windows")]
pub mod match_upgrade;
pub mod pipeline;
#[cfg(target_os = "windows")]
pub mod prep;
#[cfg(target_os = "windows")]
pub mod templates;

pub use pipeline::process_screenshot;

/// Pixel-space bounding box from the OCR engine. Float coordinates because
/// Windows.Media.Ocr can report sub-pixel boxes; `anchor` rounds to integer
/// pixel space at the boundary. Windows-only — only the engine /
/// anchor / pipeline consume it.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
pub struct OcrRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One word recognized by the OCR engine.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
pub struct OcrWord {
    pub text: String,
    pub rect: OcrRect,
}

/// Successful OCR outcome — what the pipeline returns when it both identifies
/// an upgrade panel and reads its owned-count cells. The caller applies this
/// to `AppState.collected` via repeated `set_collected` calls (snapshot is
/// truth — see [`crate::ocr::pipeline`] doc for the rationale).
#[derive(Clone, Debug)]
pub struct OcrOutcome {
    pub upgrade_id: String,
    pub upgrade_name: String,
    /// `(item_id, owned_count)` pairs in data.json requirement order. The
    /// caller is expected to write every entry to `AppState.collected`.
    pub items: Vec<(String, u32)>,
}
