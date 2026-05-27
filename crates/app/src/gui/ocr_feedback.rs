//! OCR feedback data + lifecycle constants, consumed by the VR overlay.
//!
//! The worker emits an [`OcrFeedback`] at every step of the pipeline so
//! the in-headset card pops up within ~1 frame of capture:
//! [`OcrFeedbackKind::Processing`] first, then a terminal kind
//! ([`Done`] / [`NotAPanel`] / [`Failed`]) when the pipeline finishes.
//! Each transition is also emitted to `tracing` via [`OcrFeedback::log`]
//! so the Debug-dialog log mirrors the overlay content.
//!
//! Rendering and lifecycle live in [`crate::vr::ocr_render`] (pixmap
//! drawing) and [`crate::vr::runtime`] (show/hide + fade) — this module
//! is purely the data type the worker fills in. It lives under `gui/`
//! for historical reasons; the GUI itself no longer reads it.
//!
//! Dismissal semantics, implemented by `vr::runtime::drive_ocr_overlay`:
//! - `Processing` never auto-dismisses — replaced when the pipeline
//!   finishes.
//! - Terminal kinds auto-fade after [`AUTO_DISMISS`] in release builds.
//! - Debug builds keep terminal kinds visible until the next capture
//!   replaces them — the per-item readings need scrutiny while the
//!   template matcher is still being calibrated.

use crate::ocr::OcrOutcome;
use crate::state::{AppState, OcrProgression};
use std::time::Duration;

/// How long terminal overlay states stay before fading in release
/// builds. Read by `vr::runtime::drive_ocr_overlay`
/// (`#[cfg(target_os = "windows")]`); marked dead-code-allowed so the
/// Linux build doesn't fail at clippy/-D warnings.
#[allow(dead_code)]
pub const AUTO_DISMISS: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct OcrFeedback {
    pub kind: OcrFeedbackKind,
}

#[derive(Clone, Debug)]
pub enum OcrFeedbackKind {
    /// OCR pipeline started — replaced by a terminal kind when it finishes.
    Processing,
    /// Pipeline matched an upgrade panel and applied owned-count writes.
    Done {
        upgrade_name: String,
        /// Resolved upgrade level. Pulled from `data.json` via the matched
        /// `upgrade_id`, not from the title-line OCR.
        level: u32,
        items: Vec<OcrItemDelta>,
        /// One-line summaries of any tracking / completion changes the
        /// worker made as a side effect of identifying the panel (e.g.
        /// "Auto-completed Bitcoin Mine Lv 1", "Now tracking Bitcoin
        /// Mine Lv 2"). Empty when nothing changed.
        progression_notes: Vec<String>,
    },
    /// Pipeline ran but the screenshot wasn't an upgrade panel (no
    /// "Need to submit items" anchor). Nothing was written to state.
    NotAPanel,
    /// Pipeline errored out. The message is shown verbatim — kept brief.
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct OcrItemDelta {
    pub item_name: String,
    pub before: u32,
    /// `Some(after)` when the OCR pipeline successfully read the
    /// cell's owned count. `None` when the cell was unreadable
    /// (binarised strip didn't yield a parseable X/Y); the existing
    /// `before` count was preserved instead of being overwritten with
    /// a false 0.
    pub after: Option<u32>,
    pub needed: u32,
}

impl OcrFeedback {
    pub fn processing() -> Self {
        Self {
            kind: OcrFeedbackKind::Processing,
        }
    }

    pub fn not_a_panel() -> Self {
        Self {
            kind: OcrFeedbackKind::NotAPanel,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: OcrFeedbackKind::Failed(message.into()),
        }
    }

    /// Build the `Done` variant from an [`OcrOutcome`] plus a `state`
    /// snapshot *taken before* the worker applies the new values. The
    /// before/after pair is what makes the overlay useful — reading the
    /// pre-update counts after `set_collected` would always show
    /// `before == after`.
    pub fn done(outcome: &OcrOutcome, state_before: &AppState) -> Self {
        let upgrade = state_before.index.upgrades_by_id.get(&outcome.upgrade_id);
        let level = upgrade.map(|u| u.upgrade.level).unwrap_or(0);

        let needed_by_item: std::collections::HashMap<&str, u32> = upgrade
            .map(|u| {
                u.upgrade
                    .requirements
                    .iter()
                    .map(|r| (r.item_id.as_str(), r.quantity))
                    .collect()
            })
            .unwrap_or_default();

        let items: Vec<OcrItemDelta> = outcome
            .items
            .iter()
            .map(|(item_id, after)| {
                let before = *state_before.collected.get(item_id).unwrap_or(&0);
                let item_name = state_before
                    .index
                    .items_by_id
                    .get(item_id)
                    .map(|item| item.name.clone())
                    .unwrap_or_else(|| item_id.clone());
                let needed = needed_by_item.get(item_id.as_str()).copied().unwrap_or(0);
                OcrItemDelta {
                    item_name,
                    before,
                    after: *after,
                    needed,
                }
            })
            .collect();

        Self {
            kind: OcrFeedbackKind::Done {
                upgrade_name: outcome.upgrade_name.clone(),
                level,
                items,
                progression_notes: Vec::new(),
            },
        }
    }

    /// Fold a [`OcrProgression`] into the `Done` variant's
    /// `progression_notes`. Looks up the upgrade ids in `state` so the
    /// notes use human-readable names rather than internal ids.
    /// No-op when called on a non-Done variant — the worker only calls
    /// this immediately after [`Self::done`].
    pub fn attach_progression(&mut self, state: &AppState, prog: OcrProgression) {
        let OcrFeedbackKind::Done {
            upgrade_name,
            level,
            progression_notes,
            ..
        } = &mut self.kind
        else {
            return;
        };
        for prior_id in &prog.completed_priors {
            let label = state
                .index
                .upgrades_by_id
                .get(prior_id)
                .map(|u| format!("{} Lv {}", u.module_name, u.upgrade.level))
                .unwrap_or_else(|| prior_id.clone());
            progression_notes.push(format!("Auto-completed {label}"));
        }
        if prog.tracked_self {
            progression_notes.push(format!("Now tracking {upgrade_name} Lv {level}"));
        }
    }

    /// Emit the same content the overlay shows to the tracing log, so the
    /// in-app Debug dialog and any stdout consumer have an audit trail of
    /// every OCR run. Called by the worker on each state transition; the
    /// overlay renderer doesn't log (logging from a render loop would
    /// spam at the repaint rate).
    pub fn log(&self) {
        match &self.kind {
            OcrFeedbackKind::Processing => {
                tracing::info!("OCR overlay: processing — reading panel");
            }
            OcrFeedbackKind::Done {
                upgrade_name,
                level,
                items,
                progression_notes,
            } => {
                let applied = items.iter().filter(|i| i.after.is_some()).count();
                let unread = items.len() - applied;
                tracing::info!(
                    upgrade = %upgrade_name,
                    level = level,
                    applied = applied,
                    unread = unread,
                    "OCR overlay: done"
                );
                for item in items {
                    match item.after {
                        Some(after) => tracing::info!(
                            "  {}: {} → {} / {}",
                            item.item_name,
                            item.before,
                            after,
                            item.needed,
                        ),
                        None => tracing::warn!(
                            "  {}: kept {} / {} (cell unreadable — see .ocr-debug.txt)",
                            item.item_name,
                            item.before,
                            item.needed,
                        ),
                    }
                }
                for note in progression_notes {
                    tracing::info!("  {note}");
                }
            }
            OcrFeedbackKind::NotAPanel => {
                tracing::info!(
                    "OCR overlay: not-a-panel — screenshot didn't match the upgrade-panel anchor"
                );
            }
            OcrFeedbackKind::Failed(msg) => {
                tracing::warn!("OCR overlay: failed — {msg}");
            }
        }
    }
}
