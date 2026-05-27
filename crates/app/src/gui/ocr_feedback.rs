//! Rich OCR feedback overlay with lifecycle states.
//!
//! The worker writes a [`OcrFeedback`] into a shared slot at every step
//! of the pipeline so the user sees something within ~1 frame of the
//! capture: first [`OcrFeedbackKind::Processing`] while the engine runs,
//! then one of [`OcrFeedbackKind::Done`] / [`NotAPanel`] / [`Failed`] when
//! it finishes. Each transition is also emitted to `tracing` via
//! [`OcrFeedback::log`] so the Debug-dialog logs mirror the overlay.
//!
//! Dismissal:
//! - `Processing` never auto-dismisses — it's transient and gets replaced
//!   when the pipeline finishes (with a 10s safety-net fallback if the
//!   worker hangs).
//! - Terminal kinds auto-fade after [`AUTO_DISMISS`] in release builds.
//! - Debug builds (`cfg!(debug_assertions)`) keep terminal kinds until the
//!   user clicks **Close** — the per-item readings need scrutiny while
//!   templates and the template-matcher are still being calibrated.

use crate::gui::IconCache;
use crate::ocr::OcrOutcome;
use crate::state::{AppState, OcrProgression};
use std::time::{Duration, Instant};

/// How long terminal overlay states stay before fading in release builds.
pub const AUTO_DISMISS: Duration = Duration::from_secs(3);
/// Safety-net for `Processing` overlays that never get replaced (e.g. the
/// worker thread panicked). Far longer than any realistic pipeline run.
const PROCESSING_TIMEOUT: Duration = Duration::from_secs(10);
const FADE_TAIL: Duration = Duration::from_millis(600);

#[derive(Clone, Debug)]
pub struct OcrFeedback {
    pub kind: OcrFeedbackKind,
    pub shown_at: Instant,
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
    pub icon_path: String,
    pub before: u32,
    pub after: u32,
    pub needed: u32,
}

impl OcrFeedback {
    pub fn processing() -> Self {
        Self {
            kind: OcrFeedbackKind::Processing,
            shown_at: Instant::now(),
        }
    }

    pub fn not_a_panel() -> Self {
        Self {
            kind: OcrFeedbackKind::NotAPanel,
            shown_at: Instant::now(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: OcrFeedbackKind::Failed(message.into()),
            shown_at: Instant::now(),
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

        let items = outcome
            .items
            .iter()
            .map(|(item_id, after)| {
                let before = *state_before.collected.get(item_id).unwrap_or(&0);
                let (item_name, icon_path) = match state_before.index.items_by_id.get(item_id) {
                    Some(item) => (item.name.clone(), item.icon_path.clone()),
                    None => (item_id.clone(), String::new()),
                };
                let needed = needed_by_item.get(item_id.as_str()).copied().unwrap_or(0);
                OcrItemDelta {
                    item_name,
                    icon_path,
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
            shown_at: Instant::now(),
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
                tracing::info!(
                    upgrade = %upgrade_name,
                    level = level,
                    "OCR overlay: done — applied counts to {} item(s)",
                    items.len(),
                );
                for item in items {
                    tracing::info!(
                        "  {}: {} → {} / {}",
                        item.item_name,
                        item.before,
                        item.after,
                        item.needed,
                    );
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

/// Returns `true` when the caller should drop the feedback — either the
/// auto-fade elapsed or the user clicked Close. Owning the slot stays
/// with the caller.
pub fn render(ctx: &egui::Context, feedback: &OcrFeedback, icons: &mut IconCache) -> bool {
    let manual_dismiss = cfg!(debug_assertions);
    let age = feedback.shown_at.elapsed();
    let processing = matches!(feedback.kind, OcrFeedbackKind::Processing);

    // Lifecycle: Processing has its own (much longer) timeout because the
    // worker replaces it normally; terminal kinds auto-fade in release,
    // and stay until clicked in debug.
    if processing {
        if age >= PROCESSING_TIMEOUT {
            return true;
        }
        // Keep repainting at ~20 Hz so the spinner animates smoothly.
        ctx.request_repaint_after(Duration::from_millis(50));
    } else if !manual_dismiss {
        if age >= AUTO_DISMISS {
            return true;
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    let alpha = if !processing && !manual_dismiss && AUTO_DISMISS.saturating_sub(age) < FADE_TAIL {
        let remaining = AUTO_DISMISS.saturating_sub(age).as_secs_f32();
        (remaining / FADE_TAIL.as_secs_f32()).clamp(0.0, 1.0)
    } else {
        1.0
    };

    let (title, accent) = match &feedback.kind {
        OcrFeedbackKind::Processing => (
            "Reading panel…",
            egui::Color32::from_rgb(200, 180, 90), // amber
        ),
        OcrFeedbackKind::Done { upgrade_name, .. } => (
            upgrade_name.as_str(),
            egui::Color32::from_rgb(90, 150, 220), // blue
        ),
        OcrFeedbackKind::NotAPanel => (
            "Not an upgrade panel",
            egui::Color32::from_rgb(200, 180, 90), // amber
        ),
        OcrFeedbackKind::Failed(_) => (
            "OCR failed",
            egui::Color32::from_rgb(220, 100, 90), // red
        ),
    };

    let scale = move |c: egui::Color32| {
        egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * alpha) as u8)
    };

    let mut close_clicked = false;
    egui::Area::new(egui::Id::new("ocr_feedback_overlay"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::new(0.0, 60.0))
        .order(egui::Order::Tooltip)
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(scale(egui::Color32::from_rgb(28, 28, 32)))
                .stroke(egui::Stroke::new(1.5, scale(accent)))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.vertical(|ui| {
                        render_header(ui, &feedback.kind, title, accent, &scale);
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        render_body(ui, icons, &feedback.kind, &scale);
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                        close_clicked =
                            render_footer(ui, &feedback.kind, age, manual_dismiss, &scale);
                    });
                });
        });

    close_clicked
}

fn render_header(
    ui: &mut egui::Ui,
    kind: &OcrFeedbackKind,
    title: &str,
    accent: egui::Color32,
    scale: &impl Fn(egui::Color32) -> egui::Color32,
) {
    ui.horizontal(|ui| {
        if matches!(kind, OcrFeedbackKind::Processing) {
            ui.add(egui::Spinner::new().size(14.0).color(scale(accent)));
        }
        ui.label(
            egui::RichText::new("OCR")
                .strong()
                .size(13.0)
                .color(scale(accent)),
        );
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(16.0)
                .color(scale(egui::Color32::from_gray(240))),
        );
        if let OcrFeedbackKind::Done { level, .. } = kind {
            if *level > 0 {
                ui.label(
                    egui::RichText::new(format!("Lv {level}"))
                        .size(13.0)
                        .color(scale(ui.visuals().weak_text_color())),
                );
            }
        }
    });
}

fn render_body(
    ui: &mut egui::Ui,
    icons: &mut IconCache,
    kind: &OcrFeedbackKind,
    scale: &impl Fn(egui::Color32) -> egui::Color32,
) {
    let weak = ui.visuals().weak_text_color();
    match kind {
        OcrFeedbackKind::Processing => {
            ui.label(
                egui::RichText::new("Identifying the upgrade and extracting owned counts…")
                    .size(13.0)
                    .color(scale(weak)),
            );
        }
        OcrFeedbackKind::Done {
            items,
            progression_notes,
            ..
        } => {
            if items.is_empty() {
                ui.label(
                    egui::RichText::new("No items were updated.")
                        .italics()
                        .color(scale(weak)),
                );
            } else {
                for item in items {
                    render_item_row(ui, icons, item, scale);
                }
            }
            if !progression_notes.is_empty() {
                ui.add_space(6.0);
                for note in progression_notes {
                    ui.label(
                        egui::RichText::new(format!("✓ {note}"))
                            .size(12.0)
                            .color(scale(egui::Color32::from_rgb(110, 200, 130))),
                    );
                }
            }
        }
        OcrFeedbackKind::NotAPanel => {
            ui.label(
                egui::RichText::new(
                    "The screenshot didn't contain the \"Need to submit items\" anchor. \
                     Open the Facility Upgrade panel and capture again.",
                )
                .size(13.0)
                .color(scale(weak)),
            );
        }
        OcrFeedbackKind::Failed(msg) => {
            ui.label(
                egui::RichText::new(msg)
                    .monospace()
                    .size(12.0)
                    .color(scale(egui::Color32::from_gray(230))),
            );
        }
    }
}

fn render_footer(
    ui: &mut egui::Ui,
    kind: &OcrFeedbackKind,
    age: Duration,
    manual_dismiss: bool,
    scale: &impl Fn(egui::Color32) -> egui::Color32,
) -> bool {
    let weak = ui.visuals().weak_text_color();
    let mut close_clicked = false;
    ui.horizontal(|ui| {
        let footer = match (kind, manual_dismiss) {
            (OcrFeedbackKind::Processing, _) => {
                "Working… this usually takes well under a second.".to_string()
            }
            (_, true) => "Debug build — overlay stays until dismissed.".to_string(),
            (_, false) => {
                let remaining = AUTO_DISMISS.saturating_sub(age).as_secs_f32();
                format!("Closing in {:.1}s", remaining.max(0.0))
            }
        };
        ui.label(egui::RichText::new(footer).small().color(scale(weak)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Processing has no Close button — the worker will replace
            // it when the pipeline finishes. If the user *really* needs
            // it gone, they can wait for the safety-net timeout.
            if !matches!(kind, OcrFeedbackKind::Processing) && ui.small_button("Close").clicked() {
                close_clicked = true;
            }
        });
    });
    close_clicked
}

fn render_item_row(
    ui: &mut egui::Ui,
    icons: &mut IconCache,
    item: &OcrItemDelta,
    scale: &impl Fn(egui::Color32) -> egui::Color32,
) {
    let delta_color = match item.after.cmp(&item.before) {
        std::cmp::Ordering::Greater => egui::Color32::from_rgb(110, 200, 130),
        std::cmp::Ordering::Less => egui::Color32::from_rgb(220, 140, 130),
        std::cmp::Ordering::Equal => ui.visuals().weak_text_color(),
    };

    ui.horizontal(|ui| {
        if !item.icon_path.is_empty() {
            if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                let size = egui::Vec2::splat(24.0);
                ui.add(egui::Image::new((tex.id(), size)).tint(scale(egui::Color32::WHITE)));
            } else {
                ui.add_space(28.0);
            }
        } else {
            ui.add_space(28.0);
        }

        ui.label(
            egui::RichText::new(&item.item_name)
                .size(13.0)
                .color(scale(egui::Color32::from_gray(230))),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let after_text = if item.needed > 0 {
                format!("{} / {}", item.after, item.needed)
            } else {
                format!("{}", item.after)
            };
            ui.label(
                egui::RichText::new(after_text)
                    .strong()
                    .size(13.0)
                    .color(scale(delta_color)),
            );
            if item.before != item.after {
                ui.label(
                    egui::RichText::new(format!("({}→)", item.before))
                        .size(12.0)
                        .color(scale(ui.visuals().weak_text_color())),
                );
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_one_paint(feedback: &OcrFeedback) {
        // Build a minimal headless egui Context and drive one paint pass
        // through the overlay renderer. If the layout code blows up
        // (overflow, panic, missing assets), this surfaces it.
        let ctx = egui::Context::default();
        let mut icons = IconCache::new();
        let raw_input = egui::RawInput::default();
        let _ = ctx.run(raw_input, |ctx| {
            let _ = render(ctx, feedback, &mut icons);
        });
    }

    #[test]
    fn processing_variant_renders_without_panic() {
        run_one_paint(&OcrFeedback::processing());
    }

    #[test]
    fn not_a_panel_variant_renders_without_panic() {
        run_one_paint(&OcrFeedback::not_a_panel());
    }

    #[test]
    fn failed_variant_renders_without_panic() {
        run_one_paint(&OcrFeedback::failed("file not found"));
    }

    #[test]
    fn done_variant_with_progression_renders_without_panic() {
        let fb = OcrFeedback {
            kind: OcrFeedbackKind::Done {
                upgrade_name: "Bitcoin Mine".into(),
                level: 2,
                items: vec![
                    OcrItemDelta {
                        item_name: "BPU".into(),
                        icon_path: String::new(),
                        before: 0,
                        after: 4,
                        needed: 4,
                    },
                    OcrItemDelta {
                        item_name: "Floppy Disk".into(),
                        icon_path: String::new(),
                        before: 3,
                        after: 2,
                        needed: 4,
                    },
                ],
                progression_notes: vec![
                    "Auto-completed Bitcoin Mine Lv 1".into(),
                    "Now tracking Bitcoin Mine Lv 2".into(),
                ],
            },
            shown_at: Instant::now(),
        };
        run_one_paint(&fb);
    }
}
