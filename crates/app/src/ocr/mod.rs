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
// Available in both debug and release on Windows now that the dump
// is gated by the user's `settings::Settings::ocr_debug` flag, not
// the build profile. Release users who want to file a GitHub issue
// can flip the toggle and get the bundle without rebuilding from
// source.
#[cfg(target_os = "windows")]
pub mod debug_dump;
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
    /// One entry per `data.json` requirement, in declared order.
    ///
    /// `Some(owned)` — pipeline successfully read the cell's count;
    /// caller writes it to `AppState.collected`.
    ///
    /// `None` — pipeline saw the cell but `split_progress` couldn't
    /// parse an `X/Y` shape. Caller must leave the existing collected
    /// value untouched (overwriting with 0 silently destroyed real
    /// progress when the strip Y misaligned).
    pub items: Vec<(String, Option<u32>)>,
}

/// What the pipeline produces for a given screenshot.
///
/// The previous shape (`Result<Option<OcrOutcome>>`) collapsed two
/// genuinely different outcomes — "screenshot isn't a panel" and
/// "panel detected but I can't find that upgrade in `data.json`" —
/// into a single `Ok(None)` that the worker rendered as "not a
/// panel." That misled users into thinking their capture failed when
/// in fact the data file was the gap; they spent time re-taking
/// screenshots that were already perfectly fine. Splitting the
/// outcomes lets the in-headset card say something actionable.
// `Identified` and `UnknownUpgrade` are only constructed by the
// Windows-gated `pipeline::process_screenshot`. Linux gets a stub
// that only returns `NoPanel`, which would otherwise fire
// `dead_code` lints on the unused variants. The variants are still
// part of the public surface — the worker matches on all three —
// so suppress the lint at the enum level rather than gating the
// variants themselves.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum OcrPipelineResult {
    /// Anchor found, upgrade resolved, cells read.
    Identified(OcrOutcome),
    /// `Need to submit items` anchor wasn't found — the screenshot
    /// probably isn't an upgrade panel.
    NoPanel,
    /// Anchor found (so the screenshot IS an upgrade panel) but the
    /// strict resolver couldn't match it to any
    /// `module.name` + `upgrade.level` pair in `data.json`. Almost
    /// always means a missing upgrade in the dataset — the scraper
    /// hasn't picked it up yet, or this is a level we don't model.
    UnknownUpgrade {
        /// First OCR token from the panel header area that looked
        /// like a module name (e.g. `"Moreitem"`, `"Quality"`) when
        /// we can pick one out, otherwise `None`. Lets the overlay
        /// hint at what the user should add to `data.json`.
        module_hint: Option<String>,
        /// Parsed `LV<n>` digit from the header, when present. The
        /// resolver looks for `level == current_level + 1`, so this
        /// is what the user's next upgrade level would be minus one.
        current_level: u32,
    },
}
