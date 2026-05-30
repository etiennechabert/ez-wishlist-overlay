//! Left tab: manage secondary containers (backpacks, item cases).
//!
//! The stash (`AppState::collected`) is the implicit *primary* container,
//! edited from the preview pane / VR / OCR. This tab manages the *secondary*
//! containers whose contents sum into owned totals via
//! [`crate::state::AppState::owned_total`] — so adding an item here can flip a
//! hideout upgrade to "ready" exactly like collecting it in the stash, and it
//! feeds the Items DB Quantity / Surplus columns too. Contents are entered
//! manually; a future feature will let OCR fill a box from screenshots of its
//! icon grid.

use crate::data::ItemId;
use crate::gui::hideout_pane::{
    collect_filtered_items, picker_tile, PICKER_TILE_SPACING, PICKER_TILE_W, PICKER_WINDOW_H,
    PICKER_WINDOW_W,
};
use crate::gui::{icon_cache::IconCache, theme, SaveTick};
use crate::state::{AppState, ContainerId};
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

/// Bundled bag icons a container can use. Each value is a file stem under
/// `assets/container_icons/` (sourced from the upstream ExfilZone
/// gear/Backpacks catalog). Order here is the picker-grid order.
const CONTAINER_ICONS: &[&str] = &[
    "backpack_3drt",
    "backpack_eliteops",
    "backpack_eliteops_green",
    "backpack_6sh118",
    "backpack_robinson",
    "backpack_hypertec",
    "backpack_gnjbackpack",
    "backpack_rucksack",
    "backpack_sportbag",
    "backpack_odldos_black",
    "backpack_odldos_flower",
];

/// Shown for a container that hasn't chosen an icon — a neutral tactical pack.
const DEFAULT_CONTAINER_ICON: &str = "backpack_3drt";

fn resolve_icon_key(icon: &Option<String>) -> &str {
    icon.as_deref().unwrap_or(DEFAULT_CONTAINER_ICON)
}

fn icon_asset_path(key: &str) -> String {
    format!("container_icons/{key}.png")
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    ui.add_space(4.0);
    ui.heading("Secondary containers");
    ui.label(
        egui::RichText::new(
            "Backpacks and boxes you keep items in outside the stash. Their \
             contents count toward hideout-upgrade readiness and the Items DB \
             totals, just like the stash.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(8.0);

    new_container_row(ui, state, save_tx);
    ui.separator();

    // Snapshot the container list (id + name + item count + icon) so we don't
    // hold the lock across the per-container UI; contents are read per section.
    let containers: Vec<(ContainerId, String, usize, Option<String>)> = {
        let s = state.read();
        s.containers
            .iter()
            .map(|c| {
                (
                    c.id.clone(),
                    c.name.clone(),
                    c.contents.len(),
                    c.icon.clone(),
                )
            })
            .collect()
    };

    if containers.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("No containers yet — create one above.")
                    .italics()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        return;
    }

    for (id, name, item_count, icon) in &containers {
        container_section(ui, state, icons, save_tx, id, name, *item_count, icon);
    }

    // The add-item picker (one container at a time, keyed in egui memory).
    item_picker_modal(ui.ctx(), state, icons, save_tx);
}

/// "New container: [name] [Add]". Add is disabled until a non-blank name is
/// typed; Enter in the field submits too. The in-progress name lives in egui
/// memory so it survives re-paints.
fn new_container_row(ui: &mut egui::Ui, state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let key = egui::Id::new("ctr-new-name");
    let mut name = ui.data(|d| d.get_temp::<String>(key)).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("New container:");
        let resp = ui.add(
            egui::TextEdit::singleline(&mut name)
                .hint_text("e.g. Backpack, Item case")
                .desired_width(220.0),
        );
        if resp.changed() {
            ui.data_mut(|d| d.insert_temp(key, name.clone()));
        }
        let has_name = !name.trim().is_empty();
        let submit_key = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let add = ui.add_enabled(has_name, egui::Button::new("Add")).clicked();
        if has_name && (add || submit_key) {
            state.write().create_container(name.trim().to_string());
            notify(state, save_tx);
            ui.data_mut(|d| d.remove::<String>(key));
        }
    });
}

/// One container as a collapsible section. The header carries the chosen bag
/// icon + name + item count (visible even when collapsed); the body holds
/// rename/delete, the contents list with per-item steppers, the add-item
/// button, and the icon picker.
#[allow(clippy::too_many_arguments)]
fn container_section(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
    name: &str,
    item_count: usize,
    icon: &Option<String>,
) {
    // Clone the header texture so the header closure doesn't borrow `icons`
    // (the body closure needs it mutably right after).
    let header_tex = icons
        .get(ui.ctx(), &icon_asset_path(resolve_icon_key(icon)))
        .cloned();
    let title = format!(
        "{name}  ({item_count} {})",
        if item_count == 1 { "item" } else { "items" }
    );
    let cid = ui.make_persistent_id(("container-collapse", id));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), cid, false)
        .show_header(ui, |ui| {
            if let Some(tex) = &header_tex {
                ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(28.0, 28.0)));
            }
            ui.label(egui::RichText::new(title).strong());
        })
        .body(|ui| {
            ui.horizontal(|ui| {
                ui.label("Name:");
                rename_field(ui, state, save_tx, id, name);
                ui.separator();
                delete_control(ui, state, save_tx, id);
            });
            ui.add_space(6.0);
            container_contents(ui, state, icons, save_tx, id);
            ui.add_space(4.0);
            if ui
                .button("+ Add item")
                .on_hover_text("Search the catalog and add items to this container")
                .clicked()
            {
                set_active_picker(ui.ctx(), Some(id.clone()));
                set_picker_filter(ui.ctx(), String::new());
            }
            ui.add_space(2.0);
            icon_picker(ui, state, icons, save_tx, id, icon);
        });
}

/// Closed-by-default sub-section: a wrapped grid of the bundled bag icons.
/// Clicking one assigns it to the container; the current choice is highlighted.
fn icon_picker(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
    icon: &Option<String>,
) {
    let current = resolve_icon_key(icon).to_string();
    egui::CollapsingHeader::new("Icon")
        .id_salt((id.as_str(), "icon"))
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for &key in CONTAINER_ICONS {
                    if icon_choice(ui, icons, key, key == current) {
                        state.write().set_container_icon(id, Some(key.to_string()));
                        notify(state, save_tx);
                    }
                }
            });
        });
}

/// One selectable icon tile. Returns true if it was clicked this frame.
fn icon_choice(ui: &mut egui::Ui, icons: &mut IconCache, key: &str, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, 5.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 5.0, theme::row_hover(ui.visuals().dark_mode));
    }
    if let Some(tex) = icons.get(ui.ctx(), &icon_asset_path(key)) {
        egui::Image::new(tex).paint_at(ui, rect.shrink(5.0));
    }
    resp.on_hover_text(key).clicked()
}

/// Inline rename. The in-progress edit lives in egui memory (seeded from the
/// live name) so clearing the field mid-edit doesn't get clobbered by a
/// re-seed; it commits on focus loss and ignores a blank name.
fn rename_field(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
    live_name: &str,
) {
    let key = egui::Id::new(("ctr-rename-buf", id.as_str()));
    let mut buf = ui
        .data(|d| d.get_temp::<String>(key))
        .unwrap_or_else(|| live_name.to_string());
    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .desired_width(220.0)
            .id_salt(("ctr-rename", id.as_str())),
    );
    if resp.changed() {
        ui.data_mut(|d| d.insert_temp(key, buf.clone()));
    }
    if resp.lost_focus() {
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            state.write().rename_container(id, trimmed.to_string());
            notify(state, save_tx);
        }
        // Drop the scratch buffer so the field re-seeds from the live name
        // next frame (the committed value, or the unchanged old name).
        ui.data_mut(|d| d.remove::<String>(key));
    }
}

/// "Delete" with an inline two-step confirm (no separate dialog) so an
/// accidental click can't wipe a container's contents.
fn delete_control(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
) {
    if pending_delete(ui.ctx()).as_deref() == Some(id.as_str()) {
        ui.label(
            egui::RichText::new("Delete?")
                .small()
                .color(ui.visuals().warn_fg_color),
        );
        if ui
            .add(egui::Button::new("Confirm").fill(egui::Color32::DARK_RED))
            .clicked()
        {
            state.write().delete_container(id);
            notify(state, save_tx);
            set_pending_delete(ui.ctx(), None);
        }
        if ui.button("Cancel").clicked() {
            set_pending_delete(ui.ctx(), None);
        }
    } else if ui.button("Delete").clicked() {
        set_pending_delete(ui.ctx(), Some(id.clone()));
    }
}

/// The container's items, one row each: icon + name + `[-] qty [+]` stepper +
/// a remove (✕). Mutations go through the scoped container mutators and bump
/// `version`, so the save loop and VR overlay pick them up like any other edit.
fn container_contents(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
) {
    // Snapshot (item id, name, icon, qty) for this container, alphabetised.
    let mut items: Vec<(ItemId, String, String, u32)> = {
        let s = state.read();
        match s.containers.iter().find(|c| &c.id == id) {
            Some(c) => c
                .contents
                .iter()
                .map(|(iid, &qty)| {
                    let (name, icon) = s
                        .index
                        .items_by_id
                        .get(iid)
                        .map(|it| (it.name.clone(), it.icon_path.clone()))
                        .unwrap_or_else(|| (iid.clone(), String::new()));
                    (iid.clone(), name, icon, qty)
                })
                .collect(),
            None => Vec::new(),
        }
    };
    items.sort_by_key(|a| a.1.to_lowercase());

    if items.is_empty() {
        ui.label(
            egui::RichText::new("Empty — add items below.")
                .small()
                .italics()
                .color(ui.visuals().weak_text_color()),
        );
        return;
    }

    for (item_id, name, icon, qty) in &items {
        ui.horizontal(|ui| {
            if !icon.is_empty() {
                if let Some(tex) = icons.get(ui.ctx(), icon) {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(24.0, 24.0)));
                }
            }
            ui.add(egui::Label::new(name).wrap_mode(egui::TextWrapMode::Truncate));

            // Controls hug the right edge. In a right-to-left layout the
            // first-added widget sits rightmost, so add ✕, +, qty, − to read
            // left→right as "[−] qty [+] ✕".
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕")
                    .on_hover_text("Remove from this container")
                    .clicked()
                {
                    state.write().set_container_item(id, item_id, 0);
                    notify(state, save_tx);
                }
                if ui.small_button("+").clicked() {
                    state.write().adjust_container_item(id, item_id, 1);
                    notify(state, save_tx);
                }
                let mut q = *qty;
                if ui
                    .add(egui::DragValue::new(&mut q).range(0..=9999).speed(0.1))
                    .changed()
                {
                    state.write().set_container_item(id, item_id, q);
                    notify(state, save_tx);
                }
                if ui
                    .add_enabled(*qty > 0, egui::Button::new("-").small())
                    .clicked()
                {
                    state.write().adjust_container_item(id, item_id, -1);
                    notify(state, save_tx);
                }
            });
        });
    }
}

/// Centered modal: search bar + scrollable tile grid of all catalog items.
/// Clicking a tile adds one of that item to the active container (so clicking
/// N times sets qty N); the modal stays open for bulk entry until the user
/// closes it. Reuses the hideout picker's item list + tile rendering.
fn item_picker_modal(
    ctx: &egui::Context,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let Some(active) = active_picker(ctx) else {
        return;
    };
    // The container could have been deleted while the picker was open.
    let Some(container_name) = state
        .read()
        .containers
        .iter()
        .find(|c| c.id == active)
        .map(|c| c.name.clone())
    else {
        set_active_picker(ctx, None);
        return;
    };

    let mut open = true;
    let mut filter = picker_filter(ctx);
    let mut chosen: Option<ItemId> = None;

    egui::Window::new(format!("Add items to {container_name}"))
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
                        .desired_width(ui.available_width() - 40.0),
                );
                if resp.changed() {
                    set_picker_filter(ui.ctx(), filter.clone());
                }
            });
            ui.label(
                egui::RichText::new("Click an item to add one; click again to add more.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.separator();

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
                                chosen = Some(item.id.clone());
                            }
                        }
                    });
                    ui.add_space(PICKER_TILE_SPACING);
                }
            });
        });

    if let Some(item_id) = chosen {
        state.write().adjust_container_item(&active, &item_id, 1);
        notify(state, save_tx);
    }
    if !open {
        set_active_picker(ctx, None);
        set_picker_filter(ctx, String::new());
    }
}

// --- egui-memory helpers for transient UI state ----------------------------

fn pending_delete(ctx: &egui::Context) -> Option<ContainerId> {
    ctx.data(|d| d.get_temp::<ContainerId>(egui::Id::new("ctr-pending-delete")))
}
fn set_pending_delete(ctx: &egui::Context, v: Option<ContainerId>) {
    let key = egui::Id::new("ctr-pending-delete");
    match v {
        Some(id) => ctx.data_mut(|d| d.insert_temp(key, id)),
        None => ctx.data_mut(|d| d.remove::<ContainerId>(key)),
    }
}

fn active_picker(ctx: &egui::Context) -> Option<ContainerId> {
    ctx.data(|d| d.get_temp::<ContainerId>(egui::Id::new("ctr-picker-active")))
}
fn set_active_picker(ctx: &egui::Context, v: Option<ContainerId>) {
    let key = egui::Id::new("ctr-picker-active");
    match v {
        Some(id) => ctx.data_mut(|d| d.insert_temp(key, id)),
        None => ctx.data_mut(|d| d.remove::<ContainerId>(key)),
    }
}

fn picker_filter(ctx: &egui::Context) -> String {
    ctx.data(|d| d.get_temp::<String>(egui::Id::new("ctr-picker-filter")))
        .unwrap_or_default()
}
fn set_picker_filter(ctx: &egui::Context, v: String) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("ctr-picker-filter"), v));
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
