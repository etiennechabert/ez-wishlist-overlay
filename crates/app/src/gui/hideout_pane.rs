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
const MODULE_NAME_W: f32 = 160.0;
const CELL_W: f32 = 210.0;
const ROW_H: f32 = 30.0;

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let data = state.read().data.clone();

    header_row(ui, state, save_tx);
    ui.separator();

    for (idx, module) in data.modules.iter().enumerate() {
        module_row(ui, state, save_tx, idx, module);
    }

    if let Some(sel) = selected(ui.ctx()) {
        ui.add_space(8.0);
        if let Some((module, upgrade)) = find_upgrade(&data.modules, &sel) {
            requirements_panel(ui, state, icons, &module.name, upgrade);
        }
    }
}

fn header_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
) {
    ui.horizontal(|ui| {
        ui.add_space(MODULE_NAME_W);
        for lvl in 1..=MAX_LEVELS {
            ui.allocate_ui_with_layout(
                egui::vec2(CELL_W, ROW_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(
                        egui::RichText::new(format!("Level {lvl}"))
                            .strong()
                            .color(egui::Color32::LIGHT_GRAY),
                    );
                },
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            starter_preset_button(ui, state, save_tx);
        });
    });
}

fn starter_preset_button(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
) {
    let (missing, to_untrack): (Vec<&'static str>, Vec<&'static str>) = {
        let s = state.read();
        let missing = crate::presets::STARTER_HIDEOUT
            .iter()
            .copied()
            .filter(|id| !s.tracked_upgrades.contains(*id) && !s.completed_upgrades.contains(*id))
            .collect();
        let to_untrack = crate::presets::STARTER_HIDEOUT
            .iter()
            .copied()
            .filter(|id| s.tracked_upgrades.contains(*id))
            .collect();
        (missing, to_untrack)
    };

    let total = crate::presets::STARTER_HIDEOUT.len();
    let covered = total - missing.len();
    let fully_covered = missing.is_empty();
    let undo_available = fully_covered && !to_untrack.is_empty();

    let (label, action_apply, enabled) = if !fully_covered {
        ("Apply starter preset", true, true)
    } else if undo_available {
        ("Undo starter preset", false, true)
    } else {
        ("Starter preset applied", false, false)
    };
    let tooltip = format!(
        "Community-recommended starter upgrades:\n  • {}\n\n\
         Apply tracks the missing ones; Undo untracks them again \
         (completed upgrades stay completed).",
        crate::presets::STARTER_HIDEOUT.join("\n  • "),
    );

    let resp = ui
        .add_enabled(enabled, egui::Button::new(label))
        .on_hover_text(&tooltip)
        .on_disabled_hover_text(&tooltip);
    if resp.clicked() && enabled {
        let mut s = state.write();
        let ids = if action_apply { &missing } else { &to_untrack };
        for id in ids {
            s.set_tracked_upgrade(&id.to_string(), action_apply);
        }
        drop(s);
        notify(state, save_tx);
    }
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(format!("{covered}/{total} starter upgrades tracked"))
            .small()
            .color(egui::Color32::GRAY),
    );
}

fn module_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    row_idx: usize,
    module: &HideoutModule,
) {
    let bg_idx = ui.painter().add(egui::Shape::Noop);

    let inner = ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.add_sized(
            [MODULE_NAME_W, ROW_H],
            egui::Label::new(egui::RichText::new(&module.name).strong()).truncate(),
        );
        for slot in 0..MAX_LEVELS {
            ui.allocate_ui_with_layout(
                egui::vec2(CELL_W, ROW_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| match module.upgrades.get(slot) {
                    Some(upgrade) => upgrade_cell(ui, state, save_tx, upgrade),
                    None => {}
                },
            );
        }
    });

    let row_rect = inner.response.rect;
    let hovered = ui.rect_contains_pointer(row_rect);
    let stripe = row_idx % 2 == 1;
    let bg = if hovered {
        egui::Color32::from_white_alpha(22)
    } else if stripe {
        egui::Color32::from_white_alpha(3)
    } else {
        egui::Color32::TRANSPARENT
    };
    if bg != egui::Color32::TRANSPARENT {
        ui.painter()
            .set(bg_idx, egui::epaint::RectShape::filled(row_rect, 2.0, bg));
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

            if !upgrade.description.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(&upgrade.description).color(egui::Color32::LIGHT_GRAY),
                );
            }
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
