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

use crate::ocr::{BoxScanStatus, BoxScanUpdate, OcrOutcome};
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
    },
    /// Pipeline ran but the screenshot wasn't an upgrade panel (no
    /// "Need to submit items" anchor). Nothing was written to state.
    NotAPanel,
    /// Pipeline found an upgrade panel but couldn't match the
    /// (module name, level) pair in `data.json` — almost always an
    /// upgrade the dataset doesn't have yet. Distinguished from
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
    /// Box/stash scan progress, published after each scroll capture while a
    /// box-scan session is active. Mirrors the desktop live window in the
    /// headset: what this shot read ("this capture") plus the cumulative
    /// merged series so far. Built by [`OcrFeedback::box_scan_progress`].
    BoxScanProgress {
        /// "Stash" or the container's name — shown as the card title.
        target_name: String,
        /// How many captures this session has merged so far.
        captures: u32,
        /// Merge outcome of the most recent capture.
        status: BoxScanStatus,
        /// Rows the most recent capture added to the series.
        last_rows_added: usize,
        /// Rows the most recent capture overlapped (already present) and skipped.
        last_rows_duplicate: usize,
        /// `(item_name, count)` read in the most recent capture alone, sorted
        /// desc by count then name. The renderer caps how many it draws.
        last_items: Vec<(String, u32)>,
        /// Unrecognized tiles in the most recent capture alone.
        last_unrecognized: usize,
        /// Total recognized items across the whole stitched series.
        total_items: u32,
        /// Total unrecognized tiles across the whole stitched series.
        total_unrecognized: usize,
        /// `(item_name, count)` for the cumulative series, sorted desc by count
        /// then name. The renderer caps how many it draws.
        series_items: Vec<(String, u32)>,
        /// The **most recent shot's** tile grid, normalized to crop space, each
        /// cell flagged matched (✓) / unreadable (✗). Drives the mini-grid card
        /// (#138); the text card ignores it.
        last_grid: Vec<crate::ocr::GridRow>,
        /// The box's total-weight readout, when one was parsed.
        observed_weight: Option<f32>,
        /// Computed weight of the stitched series — `Some` only when
        /// `observed_weight` is `Some` (the checksum needs both halves).
        computed_weight: Option<f32>,
    },
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
    /// Normalized crop-space rect of this requirement's cell, from
    /// [`crate::ocr::OcrOutcome::cells`]. Drives the per-cell layout of the
    /// mini-grid feedback card (#138) / on-the-items markers (#137). `None`
    /// when no geometry was carried (e.g. older callers / tests).
    pub pos: Option<crate::ocr::NormRect>,
}

impl OcrFeedback {
    /// A short, one-line confirmation for the head-locked guide-box status chip
    /// (issue #136): shown over "Ready — pull trigger" for a few seconds after a
    /// capture so the user sees what was read without the centre card. Returns
    /// the chip text + fill color, or `None` for `Processing` (no result yet).
    #[allow(dead_code)] // shown by the Windows-only guide overlay
    pub fn chip_confirm(&self) -> Option<(String, (u8, u8, u8))> {
        const GREEN: (u8, u8, u8) = (80, 180, 100);
        const AMBER: (u8, u8, u8) = (200, 180, 80);
        const RED: (u8, u8, u8) = (220, 100, 90);
        match &self.kind {
            OcrFeedbackKind::Processing => None,
            OcrFeedbackKind::Done {
                upgrade_name,
                level,
                ..
            } => Some((format!("Saved {upgrade_name} Lv {level}"), GREEN)),
            OcrFeedbackKind::BoxScanProgress {
                target_name,
                captures,
                total_items,
                ..
            } => Some((
                format!("{target_name}: {total_items} items (#{captures})"),
                GREEN,
            )),
            OcrFeedbackKind::NotAPanel => Some(("Not an upgrade panel".to_string(), AMBER)),
            OcrFeedbackKind::UnknownUpgrade { .. } => {
                Some(("Unknown upgrade — not in data".to_string(), AMBER))
            }
            OcrFeedbackKind::Failed(_) => Some(("OCR failed".to_string(), RED)),
        }
    }

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
            .enumerate()
            .map(|(i, (item_id, after))| {
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
                    // `cells` is built 1:1 with `items` by the pipeline; `.get`
                    // tolerates the stub/empty case without panicking.
                    pos: outcome.cells.get(i).copied(),
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

    /// Build the box/stash-scan progress card from a [`BoxScanUpdate`].
    ///
    /// Item ids are resolved to names and the computed-weight checksum is
    /// derived here — the worker holds `state` — so [`crate::vr::ocr_render`]
    /// stays a pure formatter with no [`AppState`] dependency (same split as
    /// [`Self::done`]). Both item lists are sorted desc by count then name so
    /// the in-headset card and the desktop window agree on ordering (matches
    /// `containers_pane::tally_rows` and its sort).
    pub fn box_scan_progress(
        state: &AppState,
        target_name: String,
        update: &BoxScanUpdate,
    ) -> Self {
        let rows = |t: &std::collections::HashMap<crate::data::ItemId, u32>| -> Vec<(String, u32)> {
            let mut v: Vec<(String, u32)> = t
                .iter()
                .map(|(id, &n)| {
                    let name = state
                        .index
                        .items_by_id
                        .get(id)
                        .map(|it| it.name.clone())
                        .unwrap_or_else(|| id.clone());
                    (name, n)
                })
                .collect();
            v.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
            });
            v
        };

        let total_items: u32 = update.tally.values().sum();
        // Only meaningful against an observed readout — mirror the desktop's
        // `if let Some(observed)` gate and its `computed_weight` formula.
        let computed_weight = update.observed_weight.map(|_| {
            update
                .tally
                .iter()
                .map(|(id, &n)| {
                    state
                        .index
                        .items_by_id
                        .get(id)
                        .and_then(|it| it.weight)
                        .unwrap_or(0.0)
                        * n as f32
                })
                .sum()
        });

        Self {
            kind: OcrFeedbackKind::BoxScanProgress {
                target_name,
                captures: update.captures,
                status: update.status,
                last_rows_added: update.last_rows_added,
                last_rows_duplicate: update.last_rows_duplicate,
                last_items: rows(&update.last_tally),
                last_unrecognized: update.last_unrecognized,
                total_items,
                total_unrecognized: update.unrecognized,
                series_items: rows(&update.tally),
                last_grid: update.last_grid.clone(),
                observed_weight: update.observed_weight,
                computed_weight,
            },
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
            OcrFeedbackKind::BoxScanProgress {
                target_name,
                captures,
                status,
                last_rows_added,
                last_rows_duplicate,
                last_unrecognized,
                total_items,
                total_unrecognized,
                ..
            } => {
                tracing::info!(
                    target = %target_name,
                    captures = captures,
                    ?status,
                    rows_added = last_rows_added,
                    rows_duplicate = last_rows_duplicate,
                    last_unrecognized = last_unrecognized,
                    total_items = total_items,
                    total_unrecognized = total_unrecognized,
                    "OCR overlay: box-scan progress",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{GameData, Item};
    use crate::ocr::ScanTarget;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn item(id: &str, name: &str, weight: Option<f32>) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            icon_path: String::new(),
            category: None,
            subcategory: None,
            weight,
            price: None,
            rarity: None,
            scan_alias: None,
        }
    }

    fn state_with_items(items: Vec<Item>) -> AppState {
        AppState::new(Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: vec![],
            items,
            research: Vec::new(),
        }))
    }

    fn update(tally: HashMap<String, u32>, observed_weight: Option<f32>) -> BoxScanUpdate {
        BoxScanUpdate {
            target: ScanTarget::Stash,
            captures: 2,
            tally,
            unrecognized: 1,
            observed_weight,
            status: BoxScanStatus::Ok,
            last_tally: HashMap::new(),
            last_unrecognized: 0,
            last_rows_added: 0,
            last_rows_duplicate: 0,
            rows: Vec::new(),
            last_grid: Vec::new(),
        }
    }

    #[test]
    fn chip_confirm_per_kind() {
        // Processing has no result yet → no chip confirmation.
        assert!(OcrFeedback::processing().chip_confirm().is_none());

        // A matched panel confirms with the upgrade name + level.
        let done = OcrFeedback {
            kind: OcrFeedbackKind::Done {
                upgrade_name: "Bitcoin Mine".into(),
                level: 2,
                items: vec![],
                progression_notes: vec![],
            },
        };
        let (text, _rgb) = done.chip_confirm().expect("Done confirms");
        assert!(text.contains("Bitcoin Mine") && text.contains("Lv 2"));

        // Terminal-but-unsuccessful kinds still confirm (so the chip doesn't get
        // stuck on "Reading…"), just with a non-success message.
        assert!(OcrFeedback {
            kind: OcrFeedbackKind::NotAPanel
        }
        .chip_confirm()
        .is_some());
        assert!(OcrFeedback {
            kind: OcrFeedbackKind::Failed("boom".into())
        }
        .chip_confirm()
        .is_some());
    }

    #[test]
    fn box_scan_progress_resolves_names_and_sorts() {
        let state = state_with_items(vec![
            item("a", "Apple", Some(1.0)),
            item("b", "Banana", Some(2.0)),
        ]);
        let mut tally = HashMap::new();
        tally.insert("a".to_string(), 1);
        tally.insert("b".to_string(), 3);
        // An id with no catalog entry must survive, falling back to the raw id.
        tally.insert("ghost".to_string(), 2);

        let fb = OcrFeedback::box_scan_progress(&state, "Stash".into(), &update(tally, Some(10.0)));
        let OcrFeedbackKind::BoxScanProgress {
            series_items,
            total_items,
            total_unrecognized,
            computed_weight,
            ..
        } = fb.kind
        else {
            panic!("expected BoxScanProgress");
        };

        // Sorted desc by count: Banana(3), ghost(2), Apple(1).
        assert_eq!(series_items[0], ("Banana".to_string(), 3));
        assert_eq!(series_items[1], ("ghost".to_string(), 2));
        assert_eq!(series_items[2], ("Apple".to_string(), 1));
        assert_eq!(total_items, 6);
        assert_eq!(total_unrecognized, 1);
        // Banana 3×2.0 + Apple 1×1.0 + ghost 2×0.0 (unknown weight) = 7.0.
        assert_eq!(computed_weight, Some(7.0));
    }

    #[test]
    fn done_threads_cell_positions_aligned_with_items() {
        // The pipeline carries one normalized cell rect per requirement, 1:1 with
        // `items`; `done` must attach each to the matching `OcrItemDelta`.
        let state = state_with_items(vec![item("a", "Apple", None), item("b", "Banana", None)]);
        let outcome = OcrOutcome {
            upgrade_id: "missing-from-data".into(),
            upgrade_name: "Some Upgrade".into(),
            items: vec![("a".into(), Some(2)), ("b".into(), None)],
            cells: vec![
                crate::ocr::NormRect {
                    x: 0.1,
                    y: 0.4,
                    w: 0.2,
                    h: 0.1,
                },
                crate::ocr::NormRect {
                    x: 0.6,
                    y: 0.4,
                    w: 0.2,
                    h: 0.1,
                },
            ],
        };
        let fb = OcrFeedback::done(&outcome, &state);
        let OcrFeedbackKind::Done { items, .. } = fb.kind else {
            panic!("expected Done");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].after, Some(2));
        assert_eq!(
            items[0].pos,
            Some(crate::ocr::NormRect {
                x: 0.1,
                y: 0.4,
                w: 0.2,
                h: 0.1
            })
        );
        assert_eq!(items[1].after, None);
        assert_eq!(items[1].pos.map(|p| p.x), Some(0.6));
    }

    #[test]
    fn done_pos_is_none_when_no_geometry_carried() {
        // Older/stub callers leave `cells` empty — every delta gets `pos: None`.
        let state = state_with_items(vec![item("a", "Apple", None)]);
        let outcome = OcrOutcome {
            upgrade_id: "x".into(),
            upgrade_name: "X".into(),
            items: vec![("a".into(), Some(1))],
            cells: vec![],
        };
        let fb = OcrFeedback::done(&outcome, &state);
        let OcrFeedbackKind::Done { items, .. } = fb.kind else {
            panic!("expected Done");
        };
        assert_eq!(items[0].pos, None);
    }

    #[test]
    fn box_scan_progress_weight_none_without_observed() {
        let state = state_with_items(vec![item("a", "Apple", Some(1.0))]);
        let mut tally = HashMap::new();
        tally.insert("a".to_string(), 1);

        let fb = OcrFeedback::box_scan_progress(&state, "Stash".into(), &update(tally, None));
        let OcrFeedbackKind::BoxScanProgress {
            observed_weight,
            computed_weight,
            ..
        } = fb.kind
        else {
            panic!("expected BoxScanProgress");
        };
        assert_eq!(observed_weight, None);
        // No observed readout → no computed half (the checksum needs both).
        assert_eq!(computed_weight, None);
    }
}
