//! Right pane: the aggregated active-items wishlist. By default it shows the
//! exact order the VR overlay uses; the sort selector can re-order it for
//! desktop planning (by remaining, or by value) WITHOUT touching the overlay,
//! which always keeps its own stable order.

use crate::gui::{icon_cache::IconCache, theme, SaveTick};
use crate::settings::{PreviewSort, Settings};
use crate::state::{ActiveItem, AppState};
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

const ICON_SIZE: f32 = 48.0;
/// Hit-target size for VR controller pointers. The wrist tremor on Quest/Index
/// controllers easily exceeds 1° of arc; sub-30px targets at typical overlay
/// distance translate to "click the wrong row" mistakes. 32px tall with
/// rounded corners keeps the buttons close to the row's icon height so the
/// overall row doesn't grow much beyond its previous height.
const BTN_H: f32 = 32.0;
const BTN_RADIUS: f32 = 16.0;

/// Return value of [`ui`]: signals the caller whether the preview-sort
/// preference changed this frame so it can persist `settings.json`. Mirrors
/// `HideoutOutcome`; otherwise the pane only ever touches `state.json` via
/// `save_tx`.
#[derive(Default)]
pub struct PreviewOutcome {
    pub settings_changed: bool,
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    settings: &Arc<RwLock<Settings>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) -> PreviewOutcome {
    let mut outcome = PreviewOutcome::default();
    let weak = ui.visuals().weak_text_color();

    // Snapshot the wishlist AND the prices the value-sort needs under a single
    // read lock (`ActiveItem` carries no price). The map is keyed only by items
    // actually on the wishlist, so it stays small.
    let (mut active, prices) = {
        let s = state.read();
        let active = s.active_items();
        let prices: HashMap<String, Option<u64>> = active
            .iter()
            .map(|a| {
                (
                    a.item_id.clone(),
                    s.index.items_by_id.get(&a.item_id).and_then(|i| i.price),
                )
            })
            .collect();
        (active, prices)
    };
    let sort = settings.read().preview_sort;
    apply_preview_sort(&mut active, sort, &prices);

    ui.heading("Active items");
    // The default mode reproduces the overlay; the others say so explicitly, so
    // the pane never silently stops mirroring the headset.
    let subtitle = match sort {
        PreviewSort::Overlay => {
            "Aggregated across every tracked upgrade — same order as the overlay."
        }
        PreviewSort::Remaining => {
            "Aggregated across every tracked upgrade · by remaining (desktop only; overlay unaffected)."
        }
        PreviewSort::Value => {
            "Aggregated across every tracked upgrade · by value (desktop only; overlay unaffected)."
        }
    };
    ui.label(egui::RichText::new(subtitle).small().color(weak));
    outcome.settings_changed |= sort_selector(ui, settings);
    ui.separator();

    if active.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label("Nothing tracked yet.");
            ui.label(
                egui::RichText::new("Open the Hideout tab and check 'Track' on a row.")
                    .small()
                    .color(weak),
            );
        });
        return outcome;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for item in &active {
            row(ui, state, icons, save_tx, item);
        }
    });
    outcome
}

/// Compact segmented control choosing how the desktop list is ordered. Returns
/// true when the choice changed so the caller persists `settings.json`. The VR
/// overlay is unaffected — it always renders `active_items()`'s stable order.
fn sort_selector(ui: &mut egui::Ui, settings: &Arc<RwLock<Settings>>) -> bool {
    let before = settings.read().preview_sort;
    let mut sort = before;
    ui.horizontal(|ui| {
        ui.label("Sort:");
        ui.selectable_value(&mut sort, PreviewSort::Overlay, "Overlay")
            .on_hover_text("Same order the VR overlay shows (pinned first, then biggest grinds).");
        ui.selectable_value(&mut sort, PreviewSort::Remaining, "Remaining")
            .on_hover_text("Most still-needed (needed − collected) first.");
        ui.selectable_value(&mut sort, PreviewSort::Value, "Value")
            .on_hover_text("Highest rouble value of the missing units (price × remaining) first.");
    });
    if sort != before {
        settings.write().preview_sort = sort;
        true
    } else {
        false
    }
}

/// Re-sort the desktop preview list in place. `Overlay` leaves the overlay
/// order untouched; the other modes are desktop-only planning views. Done items
/// (collected ≥ needed) always sink so the eye lands on what's still needed,
/// then the mode's key, then name for stability.
fn apply_preview_sort(
    items: &mut [ActiveItem],
    sort: PreviewSort,
    prices: &HashMap<String, Option<u64>>,
) {
    fn remaining(a: &ActiveItem) -> u32 {
        a.needed.saturating_sub(a.collected)
    }
    fn done(a: &ActiveItem) -> bool {
        a.collected >= a.needed
    }
    match sort {
        // Already in overlay order (pinned-first, needed-desc) from active_items().
        PreviewSort::Overlay => {}
        PreviewSort::Remaining => items.sort_by(|a, b| {
            done(a)
                .cmp(&done(b))
                .then_with(|| remaining(b).cmp(&remaining(a)))
                .then_with(|| a.name.cmp(&b.name))
        }),
        PreviewSort::Value => {
            let value = |a: &ActiveItem| {
                prices
                    .get(&a.item_id)
                    .copied()
                    .flatten()
                    .unwrap_or(0)
                    .saturating_mul(remaining(a) as u64)
            };
            items.sort_by(|a, b| {
                done(a)
                    .cmp(&done(b))
                    .then_with(|| value(b).cmp(&value(a)))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
    }
}

fn row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    item: &crate::state::ActiveItem,
) {
    let dark = ui.visuals().dark_mode;
    let done = item.collected >= item.needed && item.needed > 0;
    let frame_fill = if done {
        theme::done_frame_fill(dark)
    } else {
        egui::Color32::TRANSPARENT
    };

    egui::Frame::default()
        .fill(frame_fill)
        .inner_margin(egui::Margin::same(6.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                row_icon(ui, icons, &item.icon_path, dark);
                ui.vertical(|ui| {
                    // Tighten inter-element spacing so the name + controls +
                    // sources stack stays within the bigger icon's height.
                    // Without this the chunky VR-sized controls would push the
                    // row taller than the previous compact layout.
                    ui.spacing_mut().item_spacing.y = 1.0;
                    row_name(ui, &item.name, done, item.pinned, dark);
                    ui.horizontal(|ui| row_controls(ui, state, save_tx, item));
                    row_sources(ui, &item.sources, dark);
                });
            });
        });
    ui.add_space(2.0);
}

fn row_icon(ui: &mut egui::Ui, icons: &mut IconCache, icon_path: &str, dark: bool) {
    if let Some(tex) = icons.get(ui.ctx(), icon_path) {
        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE)));
    } else {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 4.0, theme::placeholder_icon(dark));
    }
}

fn row_name(ui: &mut egui::Ui, name: &str, done: bool, pinned: bool, dark: bool) {
    let color = if done {
        theme::done_text(dark)
    } else {
        ui.visuals().strong_text_color()
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(name).color(color));
        if pinned {
            // Small priority tag — this item leads the overlay because one of
            // its source upgrades is pinned. No star glyph (tofu in our fonts).
            ui.label(
                egui::RichText::new("pinned")
                    .small()
                    .strong()
                    .color(theme::pinned_accent(dark)),
            )
            .on_hover_text("A pinned upgrade needs this item, so it leads the overlay.");
        }
    });
}

fn row_controls(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    item: &crate::state::ActiveItem,
) {
    let progress = if item.needed == 0 {
        0.0
    } else {
        (item.collected as f32 / item.needed as f32).clamp(0.0, 1.0)
    };
    ui.add_sized(
        egui::vec2(160.0, BTN_H),
        egui::ProgressBar::new(progress).text(format!(
            "{} / {}",
            item.collected.min(item.needed),
            item.needed
        )),
    );

    if ui.add(round_button("−", 36.0)).clicked() {
        state.write().adjust_collected(&item.item_id, -1);
        notify(state, save_tx);
    }
    // Shows the combined owned total but edits the stash: apply the change as
    // a delta so dragging the number nudges the loose/stash count, never the
    // container contents (managed on the Containers tab). `adjust_collected`
    // clamps at 0, so it can't pull the total below what containers hold.
    let mut value = item.collected;
    if ui
        .add_sized(
            egui::vec2(56.0, BTN_H),
            egui::DragValue::new(&mut value).range(0..=9999).speed(0.05),
        )
        .changed()
    {
        let delta = value as i64 - item.collected as i64;
        state.write().adjust_collected(&item.item_id, delta);
        notify(state, save_tx);
    }
    if ui.add(round_button("+", 36.0)).clicked() {
        state.write().adjust_collected(&item.item_id, 1);
        notify(state, save_tx);
    }
    // One button that flips between "Done" (top up to the target so the row
    // reads as ready) and "Reset" (drop the stash back to 0). Both act on the
    // stash; items declared in a secondary container stay put (manage those on
    // the Containers tab), so after "Reset" the bar only falls as far as the
    // container holdings.
    let done = item.needed > 0 && item.collected >= item.needed;
    let (label, tooltip) = if done {
        (
            "Reset",
            "Clear the stash count for this item (containers untouched)",
        )
    } else {
        ("Done", "Top up the stash so the total meets the target")
    };
    if ui
        .add(round_button(label, 64.0))
        .on_hover_text(tooltip)
        .clicked()
    {
        if done {
            state.write().set_collected(&item.item_id, 0);
        } else {
            let shortfall = item.needed.saturating_sub(item.collected);
            state
                .write()
                .adjust_collected(&item.item_id, shortfall as i64);
        }
        notify(state, save_tx);
    }
}

/// Pill-shaped button sized for VR-pointer accuracy: tall enough that wrist
/// tremor stays inside the hit-box, with a large corner radius so the visual
/// affordance matches the bigger hit-area.
fn round_button(label: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).strong())
        .min_size(egui::vec2(width, BTN_H))
        .rounding(BTN_RADIUS)
}

fn row_sources(ui: &mut egui::Ui, sources: &[String], dark: bool) {
    if sources.is_empty() {
        return;
    }
    ui.label(
        egui::RichText::new(format!("↳ {}", sources.join(" • ")))
            .small()
            .color(theme::source_text(dark)),
    );
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
