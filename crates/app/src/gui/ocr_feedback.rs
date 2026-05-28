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
//! - Terminal kinds auto-fade after `settings.ocr_dismiss_seconds`
//!   (default 4 s) when `settings.ocr_debug` is off.
//! - When `settings.ocr_debug` is on, terminal kinds stay visible
//!   until the next capture replaces them — the user is inspecting
//!   the in-headset card alongside the on-disk debug artifacts and
//!   needs time to read both.

use crate::ocr::OcrOutcome;
use crate::state::{AppState, OcrProgression};

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
        /// `true` when fewer than half of the cells were successfully
        /// read — the in-headset card surfaces a "try recapturing
        /// straight on" hint and the tracing log mirrors it. The
        /// safety filter has already preserved existing counts on
        /// UNREAD cells, so this is informational rather than
        /// destructive, but it tells the user a straight-on retake
        /// would likely fix the gap (tilt, glare, and head-distance
        /// all funnel into this signal — the issue [#56] notes
        /// aggregate read-rate is the easiest first hint that
        /// catches the general "something looks wrong" case).
        low_read_rate: bool,
    },
    /// Pipeline ran but the screenshot wasn't an upgrade panel (no
    /// "Need to submit items" anchor). Nothing was written to state.
    NotAPanel,
    /// Pipeline found an upgrade panel but couldn't match the
    /// (module name, level) pair in `data.json` — almost always an
    /// upgrade the scraper hasn't picked up yet. Distinguished from
    /// `NotAPanel` because the user's capture WAS a valid panel and
    /// they shouldn't be told "not a panel" (they'd spend time
    /// re-taking the screenshot for nothing).
    UnknownUpgrade {
        /// Best guess at the module name from OCR tokens (e.g. "Moreitem").
        /// `None` when we couldn't extract a sensible token.
        module_hint: Option<String>,
        /// `LV<n>` parsed from the panel header. Target level (what
        /// the user would buy) is `current_level + 1`.
        current_level: u32,
        /// Path to the captured screenshot so the message can guide
        /// the user to "attach this file to a GitHub issue." Only
        /// populated when `settings.ocr_debug` is on; in the fast
        /// path no PNG is written and the user would need to
        /// re-capture with the toggle enabled to file a report.
        screenshot_path: Option<std::path::PathBuf>,
    },
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

    pub fn unknown_upgrade(
        module_hint: Option<String>,
        current_level: u32,
        screenshot_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            kind: OcrFeedbackKind::UnknownUpgrade {
                module_hint,
                current_level,
                screenshot_path,
            },
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

        // Aggregate read-rate signal (per issue #56). The pipeline
        // returns `None` for cells it couldn't confidently parse — at
        // a low read-rate the most common cause is a non-perpendicular
        // capture angle (the "Need to submit items" row visibly skews
        // across the panel, and the strip-positioning picker drifts
        // off the digit row). A `2/n` threshold (strict majority of
        // unread cells) keeps the hint off for the common "one bad
        // cell on a clean capture" case while still firing on the
        // canonical tilt failures (IntelligentLv2 1/4, MedDeskLv1
        // 1/4 in the fixture suite).
        let total = items.len();
        let unread = items.iter().filter(|i| i.after.is_none()).count();
        let low_read_rate = total > 0 && unread * 2 > total;

        Self {
            kind: OcrFeedbackKind::Done {
                upgrade_name: outcome.upgrade_name.clone(),
                level,
                items,
                progression_notes: Vec::new(),
                low_read_rate,
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
                low_read_rate,
            } => {
                let applied = items.iter().filter(|i| i.after.is_some()).count();
                let unread = items.len() - applied;
                tracing::info!(
                    upgrade = %upgrade_name,
                    level = level,
                    applied = applied,
                    unread = unread,
                    low_read_rate = *low_read_rate,
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
                if *low_read_rate {
                    tracing::warn!(
                        applied,
                        total = items.len(),
                        "OCR overlay: low read-rate — try recapturing straight on \
                         (panel tilt / distance / glare are the usual causes)"
                    );
                }
            }
            OcrFeedbackKind::NotAPanel => {
                tracing::info!(
                    "OCR overlay: not-a-panel — screenshot didn't match the upgrade-panel anchor"
                );
            }
            OcrFeedbackKind::UnknownUpgrade {
                module_hint,
                current_level,
                screenshot_path,
            } => {
                let screenshot = screenshot_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none — enable ocr_debug to retain PNGs>".to_string());
                tracing::warn!(
                    module_hint = ?module_hint,
                    current_level,
                    screenshot = %screenshot,
                    "OCR overlay: unknown-upgrade — panel detected but no matching \
                     upgrade in data.json. Add the recipe via Desktop → Manual \
                     Recipe Entry and file a GitHub issue with the screenshot."
                );
            }
            OcrFeedbackKind::Failed(msg) => {
                tracing::warn!("OCR overlay: failed — {msg}");
            }
        }
    }
}
