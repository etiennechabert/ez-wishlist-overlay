//! Rich OCR feedback overlay.
//!
//! When the OCR worker finishes applying an upgrade panel's owned counts,
//! it builds an [`OcrFeedback`] (snapshotting the *previous* `collected`
//! value for each item) and drops it in a shared slot. The GUI drains
//! the slot once per frame and renders a centred card listing every item
//! that was overwritten, with a "before → after / needed" line so the
//! user can verify the OCR read each cell correctly.
//!
//! Dismissal:
//! - Release builds auto-fade after [`AUTO_DISMISS`].
//! - Debug builds (`cfg!(debug_assertions)`) require an explicit "Close"
//!   click — handy while developing because the per-item readings need
//!   close inspection.
//!
//! The card lives in its own egui `Window` (not an `Area`) so it can host
//! a Close button without fighting Area's `interactable(false)` toast
//! semantics. It still anchors centre-of-screen and doesn't take focus
//! from the wishlist underneath.
//!
//! Per-item icons reuse the shared [`crate::gui::IconCache`] so the same
//! decoded textures the hideout/tasks panes already loaded get reused.

use crate::gui::IconCache;
use crate::ocr::OcrOutcome;
use crate::state::AppState;
use std::time::{Duration, Instant};

/// How long the overlay stays before fading away in release builds. Kept
/// short — the user will trigger another capture soon and we don't want
/// to obscure the wishlist.
pub const AUTO_DISMISS: Duration = Duration::from_secs(3);
const FADE_TAIL: Duration = Duration::from_millis(600);

#[derive(Clone, Debug)]
pub struct OcrFeedback {
    pub upgrade_name: String,
    /// Resolved upgrade level (e.g. "Lv 2"). Pulled from `data.json` via
    /// the OCR-matched upgrade id, not from the title-line OCR.
    pub level: u32,
    pub items: Vec<OcrItemDelta>,
    pub shown_at: Instant,
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
    /// Build the feedback record from the OCR outcome and the state
    /// snapshot *just before* the worker applied the new values. Reading
    /// the pre-update counts is the whole reason this is a GUI-side type
    /// rather than something the pipeline returns directly.
    pub fn from_outcome(outcome: &OcrOutcome, state_before: &AppState) -> Self {
        let upgrade = state_before
            .index
            .upgrades_by_id
            .get(&outcome.upgrade_id);
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
            upgrade_name: outcome.upgrade_name.clone(),
            level,
            items,
            shown_at: Instant::now(),
        }
    }
}

/// Render the overlay if `feedback` is `Some`. Returns `true` when the
/// caller should drop the feedback (auto-fade elapsed or user clicked
/// Close). The caller owns the slot and handles `Option` setting/clearing.
pub fn render(
    ctx: &egui::Context,
    feedback: &OcrFeedback,
    icons: &mut IconCache,
) -> bool {
    let manual_dismiss = cfg!(debug_assertions);
    let age = feedback.shown_at.elapsed();

    // Release build: fade and self-dismiss after AUTO_DISMISS.
    if !manual_dismiss && age >= AUTO_DISMISS {
        return true;
    }
    let alpha = if !manual_dismiss && AUTO_DISMISS.saturating_sub(age) < FADE_TAIL {
        let remaining = AUTO_DISMISS.saturating_sub(age).as_secs_f32();
        (remaining / FADE_TAIL.as_secs_f32()).clamp(0.0, 1.0)
    } else {
        1.0
    };
    // Keep repainting until the auto-dismiss path elapses; debug builds
    // sit until clicked so a 1 Hz repaint is enough.
    if !manual_dismiss {
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    let accent = egui::Color32::from_rgb(90, 150, 220);
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
                        // Header
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("OCR")
                                    .strong()
                                    .size(13.0)
                                    .color(scale(accent)),
                            );
                            ui.label(
                                egui::RichText::new(&feedback.upgrade_name)
                                    .strong()
                                    .size(16.0)
                                    .color(scale(egui::Color32::from_gray(240))),
                            );
                            if feedback.level > 0 {
                                ui.label(
                                    egui::RichText::new(format!("Lv {}", feedback.level))
                                        .size(13.0)
                                        .color(scale(ui.visuals().weak_text_color())),
                                );
                            }
                        });
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);

                        if feedback.items.is_empty() {
                            ui.label(
                                egui::RichText::new("No items were updated.")
                                    .italics()
                                    .color(scale(ui.visuals().weak_text_color())),
                            );
                        } else {
                            for item in &feedback.items {
                                render_item_row(ui, icons, item, alpha);
                            }
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let footer = if manual_dismiss {
                                "Debug build — overlay stays until dismissed.".to_string()
                            } else {
                                let remaining = AUTO_DISMISS.saturating_sub(age).as_secs_f32();
                                format!("Closing in {:.1}s", remaining.max(0.0))
                            };
                            ui.label(
                                egui::RichText::new(footer)
                                    .small()
                                    .color(scale(ui.visuals().weak_text_color())),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Close").clicked() {
                                        close_clicked = true;
                                    }
                                },
                            );
                        });
                    });
                });
        });

    close_clicked
}

fn render_item_row(
    ui: &mut egui::Ui,
    icons: &mut IconCache,
    item: &OcrItemDelta,
    alpha: f32,
) {
    let scale = |c: egui::Color32| {
        egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * alpha) as u8)
    };
    let delta_color = match item.after.cmp(&item.before) {
        std::cmp::Ordering::Greater => egui::Color32::from_rgb(110, 200, 130),
        std::cmp::Ordering::Less => egui::Color32::from_rgb(220, 140, 130),
        std::cmp::Ordering::Equal => ui.visuals().weak_text_color(),
    };

    ui.horizontal(|ui| {
        // Icon
        if !item.icon_path.is_empty() {
            if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                let size = egui::Vec2::splat(24.0);
                ui.add(
                    egui::Image::new((tex.id(), size))
                        .tint(scale(egui::Color32::WHITE)),
                );
            } else {
                ui.add_space(28.0);
            }
        } else {
            ui.add_space(28.0);
        }

        // Item name
        ui.label(
            egui::RichText::new(&item.item_name)
                .size(13.0)
                .color(scale(egui::Color32::from_gray(230))),
        );

        // Right-aligned count + delta
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let needed = item.needed;
            let after_text = if needed > 0 {
                format!("{} / {}", item.after, needed)
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
