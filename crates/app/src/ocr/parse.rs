//! Turn raw word boxes from [`crate::ocr::OcrResult`] into a structured
//! [`CapturedUpgrade`] entry.
//!
//! POC v1 parsing strategy — designed to be robust against the in-game
//! UI's whitespace/font quirks without over-fitting to one screenshot:
//!
//! 1. **Find anchor tokens.** Every item cell has a `\d+/\d+` progress
//!    string (e.g. "1/3", "3/3"). These are the most reliable visual
//!    markers; we locate them first.
//! 2. **Recover the cell grid.** Cluster the anchor positions by Y
//!    (rows) and X (columns). For the hideout panels we usually see
//!    one row of 4 cells; tasks UI may differ.
//! 3. **Read the cell name.** For each anchor, the item name is the
//!    largest run of text directly above the anchor within the cell's
//!    horizontal extent.
//! 4. **Read the header.** Take the topmost non-cell text — that's the
//!    upgrade title ("Storage Room A") and its level chip ("LV0").
//!    Joined together they form the [`CapturedUpgrade::key`] that the
//!    wishlist uses to de-duplicate re-captures of the same upgrade.
//! 5. **Read the cost.** A plain-number token near the header (no
//!    slash, currency-glyph nearby) — optional; missing on tasks UI.
//!
//! Confidence: every field carries the raw OCR string alongside the
//! parsed value so the GUI confirmation step (v2) can flag low-trust
//! reads without re-OCR'ing. For now the parser is best-effort and
//! returns whatever it could pull out — see TODOs below for the
//! known gaps.

use crate::ocr::OcrResult;
use serde::{Deserialize, Serialize};

/// A single upgrade screen captured from a screenshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedUpgrade {
    /// "Storage Room A LV0" — title + level, joined with a space. Used
    /// as the wishlist dedup key; re-screenshotting the same upgrade
    /// overwrites the prior entry.
    pub key: String,
    /// Display title alone ("Storage Room A").
    pub title: String,
    /// Level string as the UI shows it ("LV0", "LV1", ...). Parsed
    /// digits could be cheaper but we keep the raw string so the dedup
    /// key matches what the user sees.
    pub level: String,
    /// Hideout cost in the game's currency. Missing for non-hideout
    /// screens (tasks UI doesn't have a flat cost).
    pub cost: Option<u64>,
    /// One entry per item cell on the screen.
    pub items: Vec<CapturedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedItem {
    /// Name exactly as OCR read it — preserved for the wishlist so the
    /// fuzzy match against the catalog can be re-run later with new
    /// rules without re-OCR'ing.
    pub name: String,
    pub collected: u32,
    pub needed: u32,
}

/// Best-effort parse of OCR output → structured upgrade entry. v1 is
/// intentionally permissive: returns whatever it could extract and lets
/// the GUI surface gaps. Returns `None` only when no "X/Y" anchors are
/// present (i.e. the image doesn't look like an upgrade screen at all).
pub fn parse_upgrade(ocr: &OcrResult) -> Option<CapturedUpgrade> {
    // TODO(v2): real implementation — anchor detection + spatial
    // clustering. v1 returns a stub so the rest of the pipeline (OCR
    // call → debug UI → wishlist write) can be wired and tested with
    // real screenshots before this gets fleshed out. The raw text and
    // word boxes from `ocr` are sufficient input; this just hasn't
    // been written yet.
    if ocr.words.is_empty() {
        return None;
    }
    Some(CapturedUpgrade {
        key: "<unparsed>".to_string(),
        title: "<unparsed>".to_string(),
        level: String::new(),
        cost: None,
        items: Vec::new(),
    })
}
