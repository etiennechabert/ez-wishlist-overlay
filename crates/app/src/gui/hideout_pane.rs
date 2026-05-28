//! Left tab: hideout upgrades, one row per module with up to 4 level cells.

use crate::data::{
    HideoutModule, Item, ItemId, RecipeOverride, Requirement, Upgrade, UpgradeId, RECIPE_SLOTS,
};
use crate::gui::{icon_cache::IconCache, theme, SaveTick};
use crate::hierarchy::{category_virtual_id, module_category};
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

const MAX_LEVELS: usize = 4;
const SELECTED_ID: &str = "hideout-selected-upgrade";
const REQ_ICON_SIZE: f32 = 48.0;
const REQ_TILE_WIDTH: f32 = 170.0;
const REQ_GRID_COLS: usize = RECIPE_SLOTS;
const MODULE_NAME_W: f32 = 190.0;
const CELL_W: f32 = 210.0;
const ROW_H: f32 = 24.0;
/// Visual left-indent applied to child rows so the hierarchy is unambiguous
/// at a glance. ~one toggle-width — child toggle column aligns with the
/// parent's name column.
const CHILD_INDENT: f32 = 22.0;

enum HideoutRow<'a> {
    /// Non-interactive header for a category that has no matching buildable
    /// module (e.g. "Storage Zone", "Lounge"). Sub-modules render below it
    /// with `is_child=true`.
    SyntheticHeader(&'static str),
    Module {
        module: &'a HideoutModule,
        is_child: bool,
    },
}

fn build_hideout_rows(modules: &[HideoutModule]) -> Vec<HideoutRow<'_>> {
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<HideoutRow<'_>> = Vec::with_capacity(modules.len() + 5);

    for m in modules {
        if emitted.contains(&m.id) {
            continue;
        }
        match module_category(&m.id) {
            None => {
                rows.push(HideoutRow::Module {
                    module: m,
                    is_child: false,
                });
                emitted.insert(m.id.clone());
            }
            Some(cat) => {
                // Every category renders as a synthetic header; even modules
                // whose own name matches the category label (KitchenArea →
                // "Kitchen Area", MedicalArea → "Medical Area") become
                // children. Lets the user master-toggle the whole area from
                // the header row.
                rows.push(HideoutRow::SyntheticHeader(cat));
                for sib in modules
                    .iter()
                    .filter(|x| module_category(&x.id) == Some(cat))
                {
                    rows.push(HideoutRow::Module {
                        module: sib,
                        is_child: true,
                    });
                    emitted.insert(sib.id.clone());
                }
            }
        }
    }
    rows
}
const PICKER_TILE_W: f32 = 110.0;
const PICKER_TILE_H: f32 = 92.0;
const PICKER_TILE_ICON: f32 = 40.0;
const PICKER_TILE_SPACING: f32 = 6.0;
const PICKER_WINDOW_W: f32 = 760.0;
const PICKER_WINDOW_H: f32 = 560.0;

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let data = state.read().data.clone();

    presets_row(ui, state, save_tx);
    header_row(ui);
    ui.separator();

    let rows = build_hideout_rows(&data.modules);
    for (idx, row) in rows.iter().enumerate() {
        match row {
            HideoutRow::SyntheticHeader(name) => synthetic_header_row(ui, state, save_tx, name),
            HideoutRow::Module { module, is_child } => {
                module_row(ui, state, save_tx, idx, module, *is_child);
            }
        }
    }

    if let Some(sel) = selected(ui.ctx()) {
        ui.add_space(8.0);
        if let Some((module, upgrade)) = find_upgrade(&data.modules, &sel) {
            editable_recipe_panel(ui, state, save_tx, icons, &module.name, upgrade);
        }
    }
}

/// Compact "is this module available right now?" toggle that sits at the left
/// edge of each module row. Disabling a quest-locked module keeps the user's
/// track/done picks intact (toggle back on and they resurface) but excludes
/// its requirements from the wishlist aggregation, so the right pane stops
/// nagging about items the user literally can't act on yet.
fn module_toggle(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    module_id: &str,
    disabled: bool,
) {
    // ●/○ live in Geometric Shapes (U+25A0–25FF), which Hack covers — so
    // they render through the proportional→Hack fallback wired in gui/mod.rs.
    // Earlier ✓/✕ glyphs sit in Dingbats (U+27xx), which neither Ubuntu-Light
    // nor Hack carry, so they came through as missing-glyph tofu.
    let (glyph, tooltip) = if disabled {
        (
            "○",
            "Module locked — its requirements are hidden from the wishlist. \
             Click to re-enable.",
        )
    } else {
        (
            "●",
            "Module available — click to mark as locked (quest-gated, etc). \
             Its requirements will drop out of the wishlist until re-enabled.",
        )
    };
    let resp = ui
        .add(
            egui::Button::new(egui::RichText::new(glyph).strong()).min_size(egui::vec2(22.0, 22.0)),
        )
        .on_hover_text(tooltip);
    if resp.clicked() {
        state
            .write()
            .set_module_disabled(&module_id.to_string(), !disabled);
        notify(state, save_tx);
    }
    ui.add_space(4.0);
}

fn header_row(ui: &mut egui::Ui) {
    let header_color = ui.visuals().strong_text_color();
    ui.horizontal(|ui| {
        ui.add_space(MODULE_NAME_W);
        for lvl in 1..=MAX_LEVELS {
            ui.allocate_ui_with_layout(
                egui::vec2(CELL_W, ROW_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    // allocate_ui_with_layout collapses back to content size
                    // by default; pin min_size so the header columns line up
                    // with the wider data cells below.
                    ui.set_min_size(egui::vec2(CELL_W, ROW_H));
                    ui.label(
                        egui::RichText::new(format!("Level {lvl}"))
                            .strong()
                            .color(header_color),
                    );
                },
            );
        }
    });
}

/// Sits above the column headers: the two preset buttons (Starter,
/// Natural-Progression) with their "N/M tracked" counters, plus a
/// trailing "Deselect all" button when anything is currently
/// tracked. All on a single horizontal row so the user can take in
/// the whole "what's the state of my tracking?" question at a glance
/// — splitting them vertically (the previous layout) made each
/// counter feel disconnected from the others, and forced an extra
/// scan to figure out total coverage.
fn presets_row(ui: &mut egui::Ui, state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    ui.horizontal(|ui| {
        starter_preset_controls(ui, state, save_tx);
        ui.add_space(20.0);
        natural_progression_controls(ui, state, save_tx);

        // Untrack-all is only meaningful when there's something to
        // untrack, so it conditionally renders rather than sitting
        // disabled. Tracked > 0 is the right signal — completed
        // upgrades don't count (they're not "tracking", they're
        // "done" and should stay that way).
        let has_tracked = !state.read().tracked_upgrades.is_empty();
        if has_tracked {
            ui.add_space(20.0);
            untrack_all_button(ui, state, save_tx);
        }
    });
}

/// Apply / Undo button + "N/M starter upgrades tracked" counter for
/// the community-recommended starter set. Adds itself to whatever
/// horizontal layout the caller has open (see [`presets_row`]).
fn starter_preset_controls(
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
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(format!("{covered}/{total} starter upgrades tracked"))
            .small()
            .color(weak),
    );
}

/// "Natural progression" preset: every module's Level 1 upgrade. The
/// set is computed from the loaded `GameData` rather than a
/// hand-curated list so it stays correct when modules are
/// added/renamed upstream. Adds itself to the caller's horizontal
/// layout (see [`presets_row`]).
fn natural_progression_controls(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
) {
    let lv1_ids: Vec<String> = {
        let s = state.read();
        s.data
            .modules
            .iter()
            .filter_map(|m| {
                m.upgrades
                    .iter()
                    .find(|u| u.level == 1)
                    .map(|u| u.id.clone())
            })
            .collect()
    };

    let (missing, to_untrack): (Vec<String>, Vec<String>) = {
        let s = state.read();
        let missing = lv1_ids
            .iter()
            .filter(|id| !s.tracked_upgrades.contains(*id) && !s.completed_upgrades.contains(*id))
            .cloned()
            .collect();
        let to_untrack = lv1_ids
            .iter()
            .filter(|id| s.tracked_upgrades.contains(*id))
            .cloned()
            .collect();
        (missing, to_untrack)
    };

    let total = lv1_ids.len();
    let covered = total - missing.len();
    let fully_covered = missing.is_empty();
    let undo_available = fully_covered && !to_untrack.is_empty();

    let (label, action_apply, enabled) = if !fully_covered {
        ("Apply natural progression", true, true)
    } else if undo_available {
        ("Undo natural progression", false, true)
    } else {
        ("Natural progression applied", false, false)
    };
    let tooltip = "Bootstraps natural progression: tracks the Level 1 upgrade \
        of every hideout module. As you mark each level Done, the next level \
        in the same module is auto-tracked, so the wishlist always shows your \
        current target. Apply tracks the missing Lv1s; Undo untracks them \
        again (completed upgrades stay completed).";

    let resp = ui
        .add_enabled(enabled, egui::Button::new(label))
        .on_hover_text(tooltip)
        .on_disabled_hover_text(tooltip);
    if resp.clicked() && enabled {
        let mut s = state.write();
        let ids = if action_apply { &missing } else { &to_untrack };
        for id in ids {
            s.set_tracked_upgrade(id, action_apply);
        }
        drop(s);
        notify(state, save_tx);
    }
    ui.add_space(8.0);
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(format!("{covered}/{total} Level 1 upgrades tracked"))
            .small()
            .color(weak),
    );
}

/// "Untrack all" — untracks every currently-tracked upgrade in one
/// click. Doesn't touch completed upgrades (those are "done", not
/// "tracking", and the user expects them to stay marked done). The
/// caller in [`presets_row`] gates rendering on
/// `tracked_upgrades.is_empty()` so this only appears when there's
/// something to clear; rendering it disabled would be visual noise
/// on a fresh install.
fn untrack_all_button(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
) {
    let tracked: Vec<String> = state.read().tracked_upgrades.iter().cloned().collect();
    let count = tracked.len();
    let tooltip = format!(
        "Untrack every currently-tracked upgrade ({count} total). \
         Completed upgrades stay completed."
    );
    let resp = ui
        .add(egui::Button::new("Untrack all"))
        .on_hover_text(&tooltip);
    if resp.clicked() {
        let mut s = state.write();
        for id in &tracked {
            s.set_tracked_upgrade(id, false);
        }
        drop(s);
        notify(state, save_tx);
    }
    ui.add_space(8.0);
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(format!("{count} tracked"))
            .small()
            .color(weak),
    );
}

/// Master-toggle header row for a category. Carries the same ●/○ toggle as
/// individual modules — clicking it disables every child below until
/// re-enabled. Disable state is stored under a virtual `@cat:Name` id in
/// `disabled_modules` so it persists across runs alongside per-module state.
fn synthetic_header_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    name: &str,
) {
    let virtual_id = category_virtual_id(name);
    let disabled = state.read().disabled_modules.contains(&virtual_id);
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let name_color = if disabled { weak } else { strong };

    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        module_toggle(ui, state, save_tx, &virtual_id, disabled);
        let mut text = egui::RichText::new(name).strong().color(name_color);
        if disabled {
            text = text.strikethrough();
        }
        ui.label(text);
    });
}

fn module_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    row_idx: usize,
    module: &HideoutModule,
    is_child: bool,
) {
    let dark = ui.visuals().dark_mode;
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    // Direct vs effective disable are tracked separately so the toggle still
    // reflects this module's own state (lets the user pre-stage individual
    // picks under a disabled parent), but the row's visuals + cell-disabled
    // state follow the cascade.
    let (own_disabled, effective_disabled) = {
        let s = state.read();
        (
            s.disabled_modules.contains(&module.id),
            s.is_module_effectively_disabled(&module.id),
        )
    };
    let name_color = if effective_disabled { weak } else { strong };
    let bg_idx = ui.painter().add(egui::Shape::Noop);

    let inner = ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.allocate_ui_with_layout(
            egui::vec2(MODULE_NAME_W, ROW_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_size(egui::vec2(MODULE_NAME_W, ROW_H));
                if is_child {
                    ui.add_space(CHILD_INDENT);
                }
                // Greying the toggle when only the parent is disabled signals
                // "the parent overrides me right now" — click still works so
                // the user can pre-set individual children, but the row stays
                // visually off until the parent is re-enabled.
                let parent_locked = effective_disabled && !own_disabled;
                ui.add_enabled_ui(!parent_locked, |ui| {
                    module_toggle(ui, state, save_tx, &module.id, own_disabled);
                });
                let mut name_text = egui::RichText::new(&module.name).strong().color(name_color);
                if effective_disabled {
                    name_text = name_text.strikethrough();
                }
                ui.add(egui::Label::new(name_text).truncate());
            },
        );
        for slot in 0..MAX_LEVELS {
            ui.allocate_ui_with_layout(
                egui::vec2(CELL_W, ROW_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    // Pin the cell width so empty cells (modules with fewer
                    // than 4 levels) still reserve their column slot, keeping
                    // every row's columns aligned with the headers.
                    ui.set_min_size(egui::vec2(CELL_W, ROW_H));
                    if let Some(upgrade) = module.upgrades.get(slot) {
                        ui.add_enabled_ui(!effective_disabled, |ui| {
                            upgrade_cell(ui, state, save_tx, upgrade);
                        });
                    }
                },
            );
        }
    });

    let row_rect = inner.response.rect;
    let hovered = ui.rect_contains_pointer(row_rect);
    let stripe = row_idx % 2 == 1;
    let bg = if hovered {
        theme::row_hover(dark)
    } else if stripe {
        theme::row_stripe(dark)
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
    let (mut tracked, mut done, overridden, ready) = {
        let s = state.read();
        (
            s.tracked_upgrades.contains(&upgrade.id),
            s.completed_upgrades.contains(&upgrade.id),
            s.is_overridden(&upgrade.id),
            s.is_upgrade_ready(&upgrade.id),
        )
    };
    let original_tracked = tracked;
    let original_done = done;

    let dark = ui.visuals().dark_mode;
    let fill = if done {
        theme::done_fill(dark)
    } else if ready {
        theme::ready_fill(dark)
    } else if tracked {
        theme::tracked_fill(dark)
    } else {
        egui::Color32::TRANSPARENT
    };

    let is_selected = selected(ui.ctx()).as_deref() == Some(upgrade.id.as_str());

    egui::Frame::group(ui.style())
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut tracked, "Track");
                ui.checkbox(&mut done, "Done");
                ui.add_space(4.0);

                let label = if is_selected { "Hide" } else { "Edit" };
                let mut btn = egui::Button::new(label).small();
                if overridden {
                    btn = btn.fill(theme::override_marker(dark));
                }
                let mut resp = ui.add(btn);
                if overridden {
                    resp = resp.on_hover_text("Recipe customized — click to edit");
                }
                if resp.clicked() {
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

fn editable_recipe_panel(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    icons: &mut IconCache,
    module_name: &str,
    upgrade: &Upgrade,
) {
    let (mut slots, overridden) = {
        let s = state.read();
        (s.effective_slots(&upgrade.id), s.is_overridden(&upgrade.id))
    };
    let original_slots = slots.clone();

    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{module_name} — Level {}", upgrade.level))
                        .strong(),
                );
                if overridden {
                    ui.label(
                        egui::RichText::new("(customized)")
                            .italics()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Close").clicked() {
                        set_selected(ui.ctx(), None);
                    }
                    let reset =
                        ui.add_enabled(overridden, egui::Button::new("Reset to official").small());
                    let reset_tip = if overridden {
                        "Discard your edits and use the bundled recipe again."
                    } else {
                        "Recipe already matches the bundled data."
                    };
                    if reset.on_hover_text(reset_tip).clicked() && overridden {
                        state.write().clear_recipe_override(&upgrade.id);
                        notify(state, save_tx);
                    }
                });
            });

            let weak = ui.visuals().weak_text_color();
            if !upgrade.description.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&upgrade.description).color(weak));
            }
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "Wrong items or counts? Edit any slot below. \"Export corrections\" \
                     in the header opens a GitHub issue so we can fold fixes into the bundled \
                     dataset.",
                )
                .small()
                .color(weak),
            );
            ui.add_space(6.0);

            egui::Grid::new(format!("reqs-grid-{}", upgrade.id))
                .num_columns(REQ_GRID_COLS)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    for (slot_idx, slot) in slots.iter_mut().enumerate() {
                        slot_editor(ui, state, icons, &upgrade.id, slot_idx, slot);
                    }
                    ui.end_row();
                });
        });

    // Modal item picker — rendered as a free-floating Window so it floats
    // above the recipe panel. Reads which slot wants picking from egui memory
    // (set by `slot_editor` when the user clicks an item button) and writes
    // back into `slots` on selection.
    if let Some(slot_idx) = active_picker_slot(ui.ctx(), &upgrade.id) {
        if let Some(choice) = item_picker_modal(
            ui.ctx(),
            icons,
            state,
            &upgrade.id,
            slot_idx,
            module_name,
            upgrade.level,
        ) {
            let current_qty = slots[slot_idx]
                .as_ref()
                .map(|s| s.quantity.max(1))
                .unwrap_or(1);
            slots[slot_idx] = match choice {
                PickerChoice::None => None,
                PickerChoice::Item(item_id) => Some(Requirement {
                    item_id,
                    quantity: current_qty,
                }),
            };
        }
    }

    if !slots_match(&slots, &original_slots) {
        let new_override = RecipeOverride { slots };
        state.write().set_recipe_override(&upgrade.id, new_override);
        notify(state, save_tx);
    }
}

fn slots_match(
    a: &[Option<Requirement>; RECIPE_SLOTS],
    b: &[Option<Requirement>; RECIPE_SLOTS],
) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.item_id == y.item_id && x.quantity == y.quantity,
        _ => false,
    })
}

fn slot_editor(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    upgrade_id: &UpgradeId,
    slot_idx: usize,
    slot: &mut Option<Requirement>,
) {
    let weak = ui.visuals().weak_text_color();
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.set_width(REQ_TILE_WIDTH);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!("Slot {}", slot_idx + 1))
                        .small()
                        .color(weak),
                );

                // Icon + name share one click target so users can hit the
                // icon, the text, or anywhere between them to open the picker.
                let (icon_path, current_label) = {
                    let s = state.read();
                    let item = slot
                        .as_ref()
                        .and_then(|r| s.index.items_by_id.get(&r.item_id));
                    let icon = item.map(|i| i.icon_path.clone()).unwrap_or_default();
                    let label = item
                        .map(|i| i.name.clone())
                        .unwrap_or_else(|| "(empty — click to pick)".to_string());
                    (icon, label)
                };

                let click_area = egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(2.0, 4.0))
                    .show(ui, |ui| {
                        ui.set_width(REQ_TILE_WIDTH - 24.0);
                        ui.vertical_centered(|ui| {
                            if !icon_path.is_empty() {
                                if let Some(tex) = icons.get(ui.ctx(), &icon_path) {
                                    ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(
                                        REQ_ICON_SIZE,
                                        REQ_ICON_SIZE,
                                    )));
                                } else {
                                    placeholder_icon(ui);
                                }
                            } else {
                                placeholder_icon(ui);
                            }
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(egui::RichText::new(&current_label).strong())
                                    .wrap_mode(egui::TextWrapMode::Truncate),
                            );
                        });
                    });

                let pick_resp = click_area
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Click to change item");
                if pick_resp.clicked() {
                    set_active_picker_slot(ui.ctx(), upgrade_id, Some(slot_idx));
                    // Wipe the previous filter and request focus on next frame
                    // so the user can start typing immediately.
                    set_picker_filter(ui.ctx(), upgrade_id, String::new());
                    mark_picker_needs_focus(ui.ctx(), upgrade_id);
                }

                ui.add_space(6.0);

                // Quantity row: [-] big number [+], spread across the slot's
                // full width so the count is the dominant element.
                if let Some(req) = slot.as_mut() {
                    let collected = *state.read().collected.get(&req.item_id).unwrap_or(&0);
                    ui.horizontal(|ui| {
                        let dec_enabled = req.quantity > 1;
                        if ui
                            .add_enabled(
                                dec_enabled,
                                egui::Button::new(egui::RichText::new("-").strong().size(16.0))
                                    .min_size(egui::vec2(28.0, 26.0)),
                            )
                            .on_hover_text("Decrease quantity")
                            .clicked()
                        {
                            req.quantity = req.quantity.saturating_sub(1).max(1);
                        }

                        // Center the stock/required pair in whatever's left
                        // after the two 28px buttons + spacing. Stock is the
                        // user's global collected count for this item; it'll
                        // exceed `req.quantity` if other tracked upgrades also
                        // need it and the user has gathered enough for them
                        // too, which is fine — the display caps at the target.
                        let center_w = (ui.available_width() - 32.0).max(0.0);
                        let satisfied = collected >= req.quantity;
                        let stock_color = if satisfied {
                            ui.visuals().strong_text_color()
                        } else {
                            ui.visuals().weak_text_color()
                        };
                        ui.allocate_ui_with_layout(
                            egui::vec2(center_w, 26.0),
                            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                            |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{}",
                                            collected.min(req.quantity)
                                        ))
                                        .color(stock_color)
                                        .strong()
                                        .size(18.0),
                                    );
                                    ui.label(egui::RichText::new("/").color(weak).size(16.0));
                                    ui.label(
                                        egui::RichText::new(format!("{}", req.quantity))
                                            .strong()
                                            .size(18.0),
                                    );
                                });
                            },
                        );

                        let inc_enabled = req.quantity < 99;
                        if ui
                            .add_enabled(
                                inc_enabled,
                                egui::Button::new(egui::RichText::new("+").strong().size(16.0))
                                    .min_size(egui::vec2(28.0, 26.0)),
                            )
                            .on_hover_text("Increase quantity")
                            .clicked()
                        {
                            req.quantity = (req.quantity + 1).min(99);
                        }
                    });
                } else {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("— empty —")
                            .small()
                            .italics()
                            .color(weak),
                    );
                }
            });
        });
}

enum PickerChoice {
    None,
    Item(ItemId),
}

// --- Picker-modal egui-memory helpers --------------------------------------

fn picker_slot_key(upgrade_id: &UpgradeId) -> egui::Id {
    egui::Id::new(("picker-active-slot", upgrade_id.as_str()))
}
fn picker_filter_key(upgrade_id: &UpgradeId) -> egui::Id {
    egui::Id::new(("picker-filter", upgrade_id.as_str()))
}
fn picker_focus_key(upgrade_id: &UpgradeId) -> egui::Id {
    egui::Id::new(("picker-needs-focus", upgrade_id.as_str()))
}

fn active_picker_slot(ctx: &egui::Context, upgrade_id: &UpgradeId) -> Option<usize> {
    ctx.data(|d| d.get_temp::<usize>(picker_slot_key(upgrade_id)))
}
fn set_active_picker_slot(ctx: &egui::Context, upgrade_id: &UpgradeId, slot: Option<usize>) {
    let key = picker_slot_key(upgrade_id);
    match slot {
        Some(idx) => ctx.data_mut(|d| d.insert_temp(key, idx)),
        None => ctx.data_mut(|d| d.remove::<usize>(key)),
    }
}
fn picker_filter(ctx: &egui::Context, upgrade_id: &UpgradeId) -> String {
    ctx.data(|d| d.get_temp::<String>(picker_filter_key(upgrade_id)))
        .unwrap_or_default()
}
fn set_picker_filter(ctx: &egui::Context, upgrade_id: &UpgradeId, v: String) {
    ctx.data_mut(|d| d.insert_temp(picker_filter_key(upgrade_id), v));
}
fn take_picker_needs_focus(ctx: &egui::Context, upgrade_id: &UpgradeId) -> bool {
    let key = picker_focus_key(upgrade_id);
    let v = ctx.data(|d| d.get_temp::<bool>(key)).unwrap_or(false);
    if v {
        ctx.data_mut(|d| d.remove::<bool>(key));
    }
    v
}
fn mark_picker_needs_focus(ctx: &egui::Context, upgrade_id: &UpgradeId) {
    ctx.data_mut(|d| d.insert_temp(picker_focus_key(upgrade_id), true));
}

/// Centered modal Window: search bar at top + scrollable tile grid of all
/// catalog items. Click a tile to pick, "Clear slot" to set to empty, Esc /
/// title-bar X / Cancel to close without changes.
fn item_picker_modal(
    ctx: &egui::Context,
    icons: &mut IconCache,
    state: &Arc<RwLock<AppState>>,
    upgrade_id: &UpgradeId,
    slot_idx: usize,
    module_name: &str,
    level: u32,
) -> Option<PickerChoice> {
    let mut open = true;
    let mut chosen: Option<PickerChoice> = None;
    let mut filter = picker_filter(ctx, upgrade_id);
    let needs_focus = take_picker_needs_focus(ctx, upgrade_id);

    let title = format!(
        "{module_name} Lv{level} — pick item for slot {}",
        slot_idx + 1
    );
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([PICKER_WINDOW_W, PICKER_WINDOW_H])
        .min_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Search:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut filter)
                        .hint_text("type to filter by name…")
                        .desired_width(ui.available_width() - 200.0),
                );
                if needs_focus {
                    resp.request_focus();
                }
                if resp.changed() {
                    set_picker_filter(ui.ctx(), upgrade_id, filter.clone());
                }
                if ui.button("Clear slot").clicked() {
                    chosen = Some(PickerChoice::None);
                }
            });

            ui.separator();

            // Heuristic column count based on the actual available width.
            let avail = ui.available_width();
            let cols = ((avail + PICKER_TILE_SPACING) / (PICKER_TILE_W + PICKER_TILE_SPACING))
                .floor()
                .max(1.0) as usize;

            let items = collect_filtered_items(state, &filter);
            if items.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("No items match that filter.")
                            .italics()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for chunk in items.chunks(cols) {
                    ui.horizontal(|ui| {
                        for item in chunk {
                            if picker_tile(ui, icons, item).clicked() {
                                chosen = Some(PickerChoice::Item(item.id.clone()));
                            }
                        }
                    });
                    ui.add_space(PICKER_TILE_SPACING);
                }
            });
        });

    if chosen.is_some() || !open {
        // Either a pick happened or the user dismissed the modal — clear the
        // active slot so the modal closes; the filter wipes on next open.
        set_active_picker_slot(ctx, upgrade_id, None);
    }
    chosen
}

/// One clickable tile in the picker grid. Frame + icon + name; click sense on
/// the whole frame so the user doesn't have to aim at the label specifically.
fn picker_tile(ui: &mut egui::Ui, icons: &mut IconCache, item: &ItemListEntry) -> egui::Response {
    let dark = ui.visuals().dark_mode;
    let inner = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(4.0))
        .show(ui, |ui| {
            ui.set_width(PICKER_TILE_W);
            ui.set_height(PICKER_TILE_H);
            ui.vertical_centered(|ui| {
                if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                    ui.add(
                        egui::Image::new(tex)
                            .fit_to_exact_size(egui::vec2(PICKER_TILE_ICON, PICKER_TILE_ICON)),
                    );
                } else {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(PICKER_TILE_ICON, PICKER_TILE_ICON),
                        egui::Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(rect, 2.0, theme::placeholder_icon(dark));
                }
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(&item.name).small())
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
            });
        });

    let resp = inner.response.interact(egui::Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(resp.rect, 4.0, theme::row_hover(dark));
    }
    resp.on_hover_text(&item.name)
}

#[derive(Clone)]
struct ItemListEntry {
    id: ItemId,
    name: String,
    icon_path: String,
}

fn collect_filtered_items(state: &Arc<RwLock<AppState>>, filter: &str) -> Vec<ItemListEntry> {
    let needle = filter.trim().to_lowercase();
    let mut out: Vec<ItemListEntry> = state
        .read()
        .data
        .items
        .iter()
        .filter(|i| needle.is_empty() || i.name.to_lowercase().contains(&needle))
        .map(|i: &Item| ItemListEntry {
            id: i.id.clone(),
            name: i.name.clone(),
            icon_path: i.icon_path.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn placeholder_icon(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(REQ_ICON_SIZE, REQ_ICON_SIZE),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, 4.0, theme::placeholder_icon(dark));
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
    ctx.memory_mut(|m| {
        m.data
            .insert_temp(selected_key(), value.unwrap_or("").to_string())
    });
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
