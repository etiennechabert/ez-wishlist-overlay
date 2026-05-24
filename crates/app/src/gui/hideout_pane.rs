//! Left tab: hideout upgrades, one row per module with up to 4 level cells.

use crate::data::{HideoutModule, Requirement, Upgrade};
use crate::gui::{icon_cache::IconCache, SaveTick};
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

const MAX_LEVELS: usize = 4;
const SELECTED_ID: &str = "hideout-selected-upgrade";
const REQ_ICON_SIZE: f32 = 48.0;
const REQ_TILE_WIDTH: f32 = 140.0;
const REQ_GRID_COLS: usize = 4;

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let data = state.read().data.clone();

    egui::Grid::new("hideout-grid")
        .num_columns(MAX_LEVELS + 1)
        .striped(true)
        .spacing([10.0, 6.0])
        .min_col_width(180.0)
        .show(ui, |ui| {
            ui.label("");
            for lvl in 1..=MAX_LEVELS {
                ui.label(
                    egui::RichText::new(format!("Level {lvl}"))
                        .strong()
                        .color(egui::Color32::LIGHT_GRAY),
                );
            }
            ui.end_row();

            for module in &data.modules {
                ui.label(egui::RichText::new(&module.name).strong());
                for slot in 0..MAX_LEVELS {
                    match module.upgrades.get(slot) {
                        Some(upgrade) => upgrade_cell(ui, state, save_tx, upgrade),
                        None => {
                            ui.label("");
                        }
                    }
                }
                ui.end_row();
            }
        });

    if let Some(sel) = selected(ui.ctx()) {
        ui.add_space(8.0);
        if let Some((module, upgrade)) = find_upgrade(&data.modules, &sel) {
            requirements_panel(ui, state, icons, &module.name, upgrade);
        }
    }
}

fn upgrade_cell(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    upgrade: &Upgrade,
) {
    let (mut tracked, mut done) = {
        let s = state.read();
        (
            s.tracked_upgrades.contains(&upgrade.id),
            s.completed_upgrades.contains(&upgrade.id),
        )
    };
    let original_tracked = tracked;
    let original_done = done;

    let fill = if done {
        egui::Color32::from_rgb(30, 70, 40)
    } else if tracked {
        egui::Color32::from_rgb(30, 60, 95)
    } else {
        egui::Color32::TRANSPARENT
    };

    let is_selected = selected(ui.ctx()).as_deref() == Some(upgrade.id.as_str());

    egui::Frame::group(ui.style())
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut tracked, "Track");
                ui.checkbox(&mut done, "Done");
                ui.add_space(4.0);

                let label = if is_selected { "Hide" } else { "Details" };
                if ui.small_button(label).clicked() {
                    if is_selected {
                        set_selected(ui.ctx(), None);
                    } else {
                        set_selected(ui.ctx(), Some(&upgrade.id));
                    }
                }
            });
        });

    if tracked != original_tracked {
        state.write().set_tracked_upgrade(&upgrade.id, tracked);
        notify(state, save_tx);
    }
    if done != original_done {
        state.write().set_completed_upgrade(&upgrade.id, done);
        notify(state, save_tx);
    }
}

fn requirements_panel(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    module_name: &str,
    upgrade: &Upgrade,
) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{module_name} — Level {}", upgrade.level)).strong(),
                );
                if ui.small_button("Close").clicked() {
                    set_selected(ui.ctx(), None);
                }
            });
            ui.add_space(6.0);

            if upgrade.requirements.is_empty() {
                ui.label(
                    egui::RichText::new("No requirements.")
                        .italics()
                        .color(egui::Color32::GRAY),
                );
                return;
            }

            egui::Grid::new(format!("reqs-grid-{}", upgrade.id))
                .num_columns(REQ_GRID_COLS)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    for (i, req) in upgrade.requirements.iter().enumerate() {
                        requirement_tile(ui, state, icons, req);
                        if (i + 1) % REQ_GRID_COLS == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
}

fn requirement_tile(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    req: &Requirement,
) {
    let (name, icon_path) = {
        let s = state.read();
        match s.index.items_by_id.get(&req.item_id) {
            Some(item) => (item.name.clone(), item.icon_path.clone()),
            None => (req.item_id.clone(), String::new()),
        }
    };

    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.set_width(REQ_TILE_WIDTH);
            ui.vertical_centered(|ui| {
                if !icon_path.is_empty() {
                    if let Some(tex) = icons.get(ui.ctx(), &icon_path) {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(egui::vec2(REQ_ICON_SIZE, REQ_ICON_SIZE)),
                        );
                    } else {
                        placeholder_icon(ui);
                    }
                } else {
                    placeholder_icon(ui);
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&name).strong());
                ui.label(
                    egui::RichText::new(format!("× {}", req.quantity))
                        .color(egui::Color32::LIGHT_GRAY),
                );
            });
        });
}

fn placeholder_icon(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(REQ_ICON_SIZE, REQ_ICON_SIZE), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, egui::Color32::from_gray(60));
}

fn find_upgrade<'a>(
    modules: &'a [HideoutModule],
    upgrade_id: &str,
) -> Option<(&'a HideoutModule, &'a Upgrade)> {
    for module in modules {
        if let Some(u) = module.upgrades.iter().find(|u| u.id == upgrade_id) {
            return Some((module, u));
        }
    }
    None
}

fn selected_key() -> egui::Id {
    egui::Id::new(SELECTED_ID)
}

fn selected(ctx: &egui::Context) -> Option<String> {
    ctx.memory(|m| m.data.get_temp::<String>(selected_key()))
        .filter(|s| !s.is_empty())
}

fn set_selected(ctx: &egui::Context, value: Option<&str>) {
    ctx.memory_mut(|m| m.data.insert_temp(selected_key(), value.unwrap_or("").to_string()));
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
