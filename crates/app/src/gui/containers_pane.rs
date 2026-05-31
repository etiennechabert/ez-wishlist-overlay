//! Left tab: manage the stash + secondary containers (backpacks, item cases).
//!
//! The stash (`AppState::collected`) is the *primary* container — also edited
//! from the preview pane / VR / OCR. This tab pins it on top and lets you
//! manage *secondary* containers below it. Every container's contents sum into
//! owned totals via [`crate::state::AppState::owned_total`] — so adding an item
//! here can flip a hideout upgrade to "ready" exactly like collecting it in the
//! stash, and it feeds the Items DB Quantity / Surplus columns too. Contents
//! are entered manually; a future feature will let OCR fill a box from
//! screenshots of its icon grid.
//!
//! Layout: a sortable KPI table — one bigger row per container showing its
//! icon, name, item count, total weight, and total value. The Stash row is
//! pinned first; secondary containers follow, sorted (default: total value,
//! descending). Click a row's triangle to unfold its item list and editing
//! controls.

use crate::data::ItemId;
use crate::gui::hideout_pane::{
    collect_filtered_items, picker_tile, PICKER_TILE_SPACING, PICKER_TILE_W, PICKER_WINDOW_H,
    PICKER_WINDOW_W,
};
use crate::gui::items_db_pane::format_price;
use crate::gui::{icon_cache::IconCache, theme, SaveTick};
use crate::state::{AppState, ContainerId};
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::collections::HashMap;
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
/// Fixed icon for the pinned primary Stash row.
const STASH_ICON: &str = "stash";

// KPI-table column widths. Header titles and row cells share these so the
// columns line up. `LEAD` matches the width of the collapse triangle so the
// header titles align over the row content that sits after it.
const ROW_H: f32 = 38.0;
const LEAD: f32 = 22.0;
const W_ICON: f32 = 32.0;
const W_NAME: f32 = 230.0;
const W_ITEMS: f32 = 70.0;
const W_WEIGHT: f32 = 95.0;
const W_VALUE: f32 = 120.0;

fn resolve_icon_key(icon: &Option<String>) -> &str {
    icon.as_deref().unwrap_or(DEFAULT_CONTAINER_ICON)
}

fn icon_asset_path(key: &str) -> String {
    format!("container_icons/{key}.png")
}

/// Which store a Containers-tab row edits: the primary stash
/// (`AppState::collected`) or a secondary container by id. Lets the contents
/// editor, steppers, and add-item picker share one code path.
#[derive(Clone)]
enum Target {
    Stash,
    Container(ContainerId),
}

impl Target {
    /// Stable per-row key for egui ids (collapse state, etc.).
    fn key(&self) -> &str {
        match self {
            Target::Stash => "stash",
            Target::Container(id) => id.as_str(),
        }
    }
    fn is_stash(&self) -> bool {
        matches!(self, Target::Stash)
    }
}

/// One KPI row, snapshot once per frame so the table body never re-locks state.
/// Weight/value are best-effort sums (items with unknown weight/price are
/// skipped, matching the Items DB footer).
struct Row {
    target: Target,
    name: String,
    /// Icon key to render in the header.
    display_icon: String,
    /// The container's stored icon choice (None = default). Unused for the stash.
    icon: Option<String>,
    /// Distinct item types held — matches the historical "(N items)" count.
    item_count: usize,
    weight: f32,
    value: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Name,
    Items,
    Weight,
    Value,
}

#[derive(Clone, Copy)]
struct Sort {
    col: SortCol,
    desc: bool,
}

/// Sum (distinct-item count, total weight, total value) over a contents map.
fn compute_kpis(s: &AppState, contents: &HashMap<ItemId, u32>) -> (usize, f32, u64) {
    let mut weight = 0.0f32;
    let mut value = 0u64;
    for (iid, &qty) in contents {
        if let Some(it) = s.index.items_by_id.get(iid) {
            if let Some(w) = it.weight {
                weight += w * qty as f32;
            }
            if let Some(p) = it.price {
                value = value.saturating_add(p * qty as u64);
            }
        }
    }
    (contents.len(), weight, value)
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Containers")
            .heading()
            .strong()
            .size(26.0)
            .color(ui.visuals().strong_text_color()),
    );
    ui.label(
        egui::RichText::new(
            "Your stash, plus any backpacks and boxes you keep items in. All of \
             it counts toward hideout-upgrade readiness and the Items DB totals.",
        )
        .small()
        .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(8.0);

    // Snapshot the stash row + every container row in one read lock.
    let (stash_row, mut rows) = {
        let s = state.read();
        let (sc, sw, sv) = compute_kpis(&s, &s.collected);
        let stash_row = Row {
            target: Target::Stash,
            name: "Stash".to_string(),
            display_icon: STASH_ICON.to_string(),
            icon: None,
            item_count: sc,
            weight: sw,
            value: sv,
        };
        let rows: Vec<Row> = s
            .containers
            .iter()
            .map(|c| {
                let (count, weight, value) = compute_kpis(&s, &c.contents);
                Row {
                    target: Target::Container(c.id.clone()),
                    name: c.name.clone(),
                    display_icon: resolve_icon_key(&c.icon).to_string(),
                    icon: c.icon.clone(),
                    item_count: count,
                    weight,
                    value,
                }
            })
            .collect();
        (stash_row, rows)
    };

    // --- Primary table: the stash, on its own. ---
    ui.add_space(2.0);
    section_title(ui, "Primary");
    column_header(ui, false);
    container_row(ui, state, icons, save_tx, &stash_row);

    // --- Vertical gap, then the secondary table with its own sortable header. ---
    ui.add_space(18.0);
    section_title(ui, "Secondary");
    column_header(ui, true);
    let sort = sort_state(ui.ctx());
    sort_rows(&mut rows, sort);
    for row in &rows {
        container_row(ui, state, icons, save_tx, row);
    }

    // "New container" lives at the bottom of the Secondary section — it's the
    // action that adds a row here, so it sits where the next row would go
    // (and stands in for an empty-state message when there are none).
    ui.add_space(6.0);
    if ui
        .add(egui::Button::new(
            egui::RichText::new("➕  New container").strong(),
        ))
        .on_hover_text("Create a backpack or box: name it and pick an icon")
        .clicked()
    {
        open_new_container_modal(ui.ctx());
    }

    // The add-item picker (one target at a time, keyed in egui memory).
    item_picker_modal(ui.ctx(), state, icons, save_tx);

    // The "New container" modal (name + icon grid), when open.
    new_container_modal(ui.ctx(), state, icons, save_tx);
}

// --- "New container" modal -------------------------------------------------

const NEW_NAME_KEY: &str = "ctr-new-name";
const NEW_ICON_KEY: &str = "ctr-new-icon";
const NEW_OPEN_KEY: &str = "ctr-new-open";
const NEW_FOCUS_KEY: &str = "ctr-new-focus";
/// When present, the modal is editing this existing container (Save writes to
/// it) rather than creating a new one.
const NEW_EDIT_KEY: &str = "ctr-new-edit-id";

/// Open the modal in *create* mode: blank name, default icon, focus the field.
fn open_new_container_modal(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_OPEN_KEY), true);
        d.remove::<String>(egui::Id::new(NEW_EDIT_KEY));
        d.insert_temp(egui::Id::new(NEW_NAME_KEY), String::new());
        d.insert_temp(
            egui::Id::new(NEW_ICON_KEY),
            DEFAULT_CONTAINER_ICON.to_string(),
        );
        d.insert_temp(egui::Id::new(NEW_FOCUS_KEY), true);
    });
}

/// Open the modal in *edit* mode: pre-fill the given container's name + icon;
/// Save writes back to it.
fn open_edit_container_modal(ctx: &egui::Context, id: &ContainerId, name: &str, icon: &str) {
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_OPEN_KEY), true);
        d.insert_temp(egui::Id::new(NEW_EDIT_KEY), id.clone());
        d.insert_temp(egui::Id::new(NEW_NAME_KEY), name.to_string());
        d.insert_temp(egui::Id::new(NEW_ICON_KEY), icon.to_string());
        d.insert_temp(egui::Id::new(NEW_FOCUS_KEY), true);
    });
}

fn close_new_container_modal(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_OPEN_KEY), false);
        d.remove::<ContainerId>(egui::Id::new(NEW_EDIT_KEY));
        d.remove::<String>(egui::Id::new(NEW_NAME_KEY));
        d.remove::<String>(egui::Id::new(NEW_ICON_KEY));
    });
}

/// Centered modal to create a container: a name field plus a grid of large
/// icon tiles, with Create / Cancel. Create is disabled until a non-blank name
/// is typed (Enter in the field also submits). Reuses [`icon_choice`] at a
/// bigger tile size than the inline per-container picker.
fn new_container_modal(
    ctx: &egui::Context,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let is_open = ctx.data(|d| {
        d.get_temp::<bool>(egui::Id::new(NEW_OPEN_KEY))
            .unwrap_or(false)
    });
    if !is_open {
        return;
    }

    let mut name = ctx
        .data(|d| d.get_temp::<String>(egui::Id::new(NEW_NAME_KEY)))
        .unwrap_or_default();
    let mut chosen_icon = ctx
        .data(|d| d.get_temp::<String>(egui::Id::new(NEW_ICON_KEY)))
        .unwrap_or_else(|| DEFAULT_CONTAINER_ICON.to_string());
    let want_focus = ctx
        .data(|d| d.get_temp::<bool>(egui::Id::new(NEW_FOCUS_KEY)))
        .unwrap_or(false);
    let edit_id = ctx.data(|d| d.get_temp::<ContainerId>(egui::Id::new(NEW_EDIT_KEY)));
    let editing = edit_id.is_some();
    let title = if editing {
        "Edit container"
    } else {
        "New container"
    };
    let submit_label = if editing { "Save" } else { "Create" };

    let mut open = true; // window-chrome close button → cancel
    let mut do_create = false;
    let mut do_cancel = false;

    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(560.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label("Name:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .hint_text("e.g. Backpack, Item case")
                        .desired_width(280.0),
                );
                if want_focus {
                    resp.request_focus();
                    ui.ctx()
                        .data_mut(|d| d.remove::<bool>(egui::Id::new(NEW_FOCUS_KEY)));
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    do_create = true;
                }
            });

            ui.add_space(10.0);
            ui.label("Icon:");
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for &key in CONTAINER_ICONS {
                    if icon_choice(ui, icons, key, key == chosen_icon, 88.0) {
                        chosen_icon = key.to_string();
                    }
                }
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(4.0);
            let has_name = !name.trim().is_empty();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(has_name, egui::Button::new(submit_label))
                    .clicked()
                {
                    do_create = true;
                }
                if ui.button("Cancel").clicked() {
                    do_cancel = true;
                }
                if !has_name {
                    ui.label(
                        egui::RichText::new("Enter a name")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            });
        });

    // Persist the in-progress edits for the next frame.
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_NAME_KEY), name.clone());
        d.insert_temp(egui::Id::new(NEW_ICON_KEY), chosen_icon.clone());
    });

    if do_create && !name.trim().is_empty() {
        let trimmed = name.trim().to_string();
        match &edit_id {
            Some(id) => {
                // Edit mode: write back to the existing container.
                let mut w = state.write();
                w.rename_container(id, trimmed);
                w.set_container_icon(id, Some(chosen_icon));
            }
            None => {
                let id = state.write().create_container(trimmed);
                state.write().set_container_icon(&id, Some(chosen_icon));
            }
        }
        notify(state, save_tx);
        close_new_container_modal(ctx);
    } else if do_cancel || !open {
        close_new_container_modal(ctx);
    }
}

// --- KPI table -------------------------------------------------------------

fn sort_state(ctx: &egui::Context) -> Sort {
    ctx.data(|d| d.get_temp::<Sort>(egui::Id::new("ctr-sort")))
        .unwrap_or(Sort {
            col: SortCol::Value,
            desc: true,
        })
}
fn set_sort_state(ctx: &egui::Context, s: Sort) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("ctr-sort"), s));
}

/// Value/quantity columns default to descending (biggest first — the obvious
/// first click); the Name column defaults to ascending.
fn default_desc(col: SortCol) -> bool {
    !matches!(col, SortCol::Name)
}

fn sort_rows(rows: &mut [Row], sort: Sort) {
    rows.sort_by(|a, b| {
        let ord = match sort.col {
            SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortCol::Items => a.item_count.cmp(&b.item_count),
            SortCol::Weight => a
                .weight
                .partial_cmp(&b.weight)
                .unwrap_or(std::cmp::Ordering::Equal),
            SortCol::Value => a.value.cmp(&b.value),
        }
        // Stable tiebreak so equal-KPI rows don't shuffle frame to frame.
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        if sort.desc {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// Horizontal padding inside numeric cells so values aren't flush against the
/// next column or the window edge. Applied identically to header and rows.
const CELL_PAD: f32 = 8.0;

/// Absolute column rectangles for one table row, derived from the row's full
/// rect. Both the header and the data rows place their content at these exact
/// positions via `ui.put`, so alignment can't drift no matter the spacing — the
/// recurring bug with hand-rolled `horizontal` layouts.
struct Cols {
    tri: egui::Rect,
    icon: egui::Rect,
    name: egui::Rect,
    items: egui::Rect,
    weight: egui::Rect,
    value: egui::Rect,
    /// Everything right of Value, for the Edit/Delete buttons.
    actions: egui::Rect,
    /// Triangle..Value inclusive — the fold/unfold click target (excludes the
    /// actions column so its buttons handle their own clicks).
    toggle: egui::Rect,
}

fn cols(row: egui::Rect) -> Cols {
    let (t, b) = (row.top(), row.bottom());
    let mk = |x: f32, w: f32| egui::Rect::from_min_max(egui::pos2(x, t), egui::pos2(x + w, b));
    let tri = mk(row.left(), LEAD);
    let icon = mk(tri.right(), W_ICON);
    let name = mk(icon.right(), W_NAME);
    let items = mk(name.right(), W_ITEMS);
    let weight = mk(items.right(), W_WEIGHT);
    let value = mk(weight.right(), W_VALUE);
    let actions = egui::Rect::from_min_max(
        egui::pos2(value.right(), t),
        egui::pos2(row.right().max(value.right()), b),
    );
    let toggle = egui::Rect::from_min_max(row.left_top(), value.right_bottom());
    Cols {
        tri,
        icon,
        name,
        items,
        weight,
        value,
        actions,
        toggle,
    }
}

/// Right-aligned text in `rect` (with cell padding). Optionally clickable
/// (header sort) and/or strong; returns the response.
fn put_right(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    text: &str,
    strong: bool,
    click: bool,
) -> egui::Response {
    let mut t = egui::RichText::new(text);
    if strong {
        t = t.strong();
    }
    let mut label = egui::Label::new(t).selectable(false);
    if click {
        label = label.sense(egui::Sense::click());
    }
    let inner = rect.shrink2(egui::vec2(CELL_PAD, 0.0));
    let mut c = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    c.add(label)
}

/// Left-aligned, clipped, truncating text in `rect` (with cell padding).
fn put_left(ui: &mut egui::Ui, rect: egui::Rect, text: egui::RichText) {
    let inner = rect.shrink2(egui::vec2(CELL_PAD, 0.0));
    let mut c = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    c.set_clip_rect(inner);
    c.add(egui::Label::new(text).selectable(false).truncate());
}

/// Prominent "Primary" / "Secondary" section heading above each table.
fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .strong()
            .size(17.0)
            .color(ui.visuals().strong_text_color()),
    );
    ui.add_space(2.0);
}

/// Column header row, aligned to the same [`cols`] geometry as the data rows.
/// When `sortable`, the KPI cells toggle the persisted sort on click.
fn column_header(ui: &mut egui::Ui, sortable: bool) {
    let sort = sort_state(ui.ctx());
    let full_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(full_w, 20.0), egui::Sense::hover());
    let c = cols(rect);

    // "Container" header — left-aligned, and sortable-by-name when this is the
    // secondary table.
    {
        let active = sortable && sort.col == SortCol::Name;
        let arrow = if active {
            if sort.desc {
                " ▼"
            } else {
                " ▲"
            }
        } else {
            ""
        };
        let inner = c.name.shrink2(egui::vec2(CELL_PAD, 0.0));
        let mut nc = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut label = egui::Label::new(egui::RichText::new(format!("Container{arrow}")).strong())
            .selectable(false);
        if sortable {
            label = label.sense(egui::Sense::click());
        }
        if nc.add(label).clicked() && sortable {
            let next = if sort.col == SortCol::Name {
                Sort {
                    col: SortCol::Name,
                    desc: !sort.desc,
                }
            } else {
                Sort {
                    col: SortCol::Name,
                    desc: default_desc(SortCol::Name),
                }
            };
            set_sort_state(ui.ctx(), next);
        }
    }

    let mut header = |rect: egui::Rect, label: &str, col: SortCol| {
        let active = sortable && sort.col == col;
        let arrow = if active {
            if sort.desc {
                " ▼"
            } else {
                " ▲"
            }
        } else {
            ""
        };
        let resp = put_right(ui, rect, &format!("{label}{arrow}"), true, sortable);
        if sortable && resp.clicked() {
            let next = if sort.col == col {
                Sort {
                    col,
                    desc: !sort.desc,
                }
            } else {
                Sort {
                    col,
                    desc: default_desc(col),
                }
            };
            set_sort_state(ui.ctx(), next);
        }
    };
    header(c.items, "Items", SortCol::Items);
    header(c.weight, "Weight", SortCol::Weight);
    header(c.value, "Value", SortCol::Value);
    ui.separator();
}

/// One container as a collapsible KPI row showing triangle, icon, name, item
/// count, weight, value, and (for secondary containers) Edit/Delete actions.
/// All columns are placed at absolute [`cols`] rects shared with
/// [`column_header`], so alignment can't drift. The triangle..Value span is the
/// fold/unfold click target; the actions column to its right is always visible
/// (folded or unfolded) and handles its own button clicks.
fn container_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    row: &Row,
) {
    let header_tex = icons
        .get(ui.ctx(), &icon_asset_path(&row.display_icon))
        .cloned();
    let cid = ui.make_persistent_id(("container-row", row.target.key()));
    let mut collapse =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), cid, false);
    let is_open = collapse.is_open();

    let full_w = ui.available_width();
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(full_w, ROW_H), egui::Sense::hover());
    let c = cols(row_rect);

    // Fold/unfold click target: triangle..Value (not the actions column).
    let toggle_resp = ui
        .interact(c.toggle, cid.with("toggle"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    if ui.is_rect_visible(row_rect) {
        if toggle_resp.hovered() {
            ui.painter()
                .rect_filled(c.toggle, 3.0, theme::row_hover(ui.visuals().dark_mode));
        }
        // Triangle.
        ui.painter().text(
            c.tri.center(),
            egui::Align2::CENTER_CENTER,
            if is_open { "▼" } else { "▶" },
            egui::FontId::proportional(12.0),
            ui.visuals().weak_text_color(),
        );
        // Icon.
        if let Some(tex) = &header_tex {
            let icon_rect = egui::Rect::from_center_size(c.icon.center(), egui::vec2(28.0, 28.0));
            egui::Image::new(tex).paint_at(ui, icon_rect);
        }
        put_left(ui, c.name, egui::RichText::new(&row.name).strong());
        put_right(ui, c.items, &row.item_count.to_string(), false, false);
        put_right(ui, c.weight, &format!("{:.2} kg", row.weight), false, false);
        put_right(
            ui,
            c.value,
            &format!("{} RUB", format_price(row.value)),
            true,
            false,
        );

        // Actions column (secondary containers only): Edit + Delete, always
        // visible. Lives outside the toggle rect so its buttons don't fold.
        if let Target::Container(id) = &row.target {
            let mut a = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(c.actions.shrink2(egui::vec2(CELL_PAD, 4.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            row_actions(
                &mut a,
                state,
                save_tx,
                id,
                &row.name,
                resolve_icon_key(&row.icon),
            );
        }
    }

    if toggle_resp.clicked() {
        collapse.toggle(ui);
    }
    collapse.store(ui.ctx());

    // Body: the item list + add-item, only while expanded.
    if is_open {
        container_body(ui, state, icons, save_tx, row);
    }
}

/// Edit + Delete buttons for a secondary container, shown in the row's actions
/// column. Delete uses a two-step inline confirm so a stray click can't wipe
/// contents; Edit reopens the create modal in edit mode.
fn row_actions(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    id: &ContainerId,
    name: &str,
    icon_key: &str,
) {
    if ui
        .button("Edit")
        .on_hover_text("Rename and change the icon")
        .clicked()
    {
        open_edit_container_modal(ui.ctx(), id, name, icon_key);
    }
    if pending_delete(ui.ctx()).as_deref() == Some(id.as_str()) {
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
    } else if ui
        .button("Delete")
        .on_hover_text("Delete this container")
        .clicked()
    {
        set_pending_delete(ui.ctx(), Some(id.clone()));
    }
}

/// The expanded body: a one-line hint for the stash, then the item list and
/// the add-item button. Edit/Delete live in the row's actions column (always
/// visible), not here.
fn container_body(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    row: &Row,
) {
    if row.target.is_stash() {
        ui.horizontal(|ui| {
            ui.add_space(LEAD);
            ui.label(
                egui::RichText::new(
                    "Loose items in the main stash. Also updated by the preview pane and OCR.",
                )
                .small()
                .color(ui.visuals().weak_text_color()),
            );
        });
        ui.add_space(6.0);
    }

    contents_editor(ui, state, icons, save_tx, &row.target);

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(LEAD);
        if ui
            .button("+ Add item")
            .on_hover_text("Search the catalog and add items here")
            .clicked()
        {
            set_active_picker(ui.ctx(), Some(row.target.clone()));
            set_picker_filter(ui.ctx(), String::new());
        }
    });
}

/// One selectable square icon tile of the given size. Returns true if it was
/// clicked this frame. Padding scales with the tile so the icon fills it the
/// same way at any size.
fn icon_choice(
    ui: &mut egui::Ui,
    icons: &mut IconCache,
    key: &str,
    selected: bool,
    size: f32,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let rounding = size * 0.11;
    if selected {
        ui.painter()
            .rect_filled(rect, rounding, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, rounding, theme::row_hover(ui.visuals().dark_mode));
    }
    if let Some(tex) = icons.get(ui.ctx(), &icon_asset_path(key)) {
        egui::Image::new(tex).paint_at(ui, rect.shrink(size * 0.12));
    }
    resp.on_hover_text(key).clicked()
}

/// Set the quantity of `item` in `target` (stash or a container). 0 removes it.
fn target_set_item(state: &Arc<RwLock<AppState>>, target: &Target, item: &ItemId, value: u32) {
    let mut w = state.write();
    match target {
        Target::Stash => w.set_collected(item, value),
        Target::Container(id) => w.set_container_item(id, item, value),
    }
}

/// Adjust the quantity of `item` in `target` by `delta`, clamped at 0.
fn target_adjust_item(state: &Arc<RwLock<AppState>>, target: &Target, item: &ItemId, delta: i64) {
    let mut w = state.write();
    match target {
        Target::Stash => w.adjust_collected(item, delta),
        Target::Container(id) => w.adjust_container_item(id, item, delta),
    }
}

/// Read a snapshot of a target's contents as (item id, name, icon, qty).
fn target_contents(s: &AppState, target: &Target) -> Vec<(ItemId, String, String, u32)> {
    let map: Option<&HashMap<ItemId, u32>> = match target {
        Target::Stash => Some(&s.collected),
        Target::Container(id) => s
            .containers
            .iter()
            .find(|c| &c.id == id)
            .map(|c| &c.contents),
    };
    let Some(map) = map else {
        return Vec::new();
    };
    map.iter()
        .map(|(iid, &qty)| {
            let (name, icon) = s
                .index
                .items_by_id
                .get(iid)
                .map(|it| (it.name.clone(), it.icon_path.clone()))
                .unwrap_or_else(|| (iid.clone(), String::new()));
            (iid.clone(), name, icon, qty)
        })
        .collect()
}

/// The target's items, one row each: icon + name + `[-] qty [+]` stepper + a
/// remove (×). Mutations bump `version`, so the save loop and VR overlay pick
/// them up like any other edit.
fn contents_editor(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    target: &Target,
) {
    let mut items = target_contents(&state.read(), target);
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

    let remove_tip = if target.is_stash() {
        "Remove from the stash"
    } else {
        "Remove from this container"
    };

    // Comfortable hit targets for the stepper + remove controls.
    const BTN: egui::Vec2 = egui::vec2(30.0, 28.0);
    const QTY_W: f32 = 46.0;

    for (item_id, name, icon, qty) in &items {
        ui.horizontal(|ui| {
            ui.add_space(LEAD); // indent contents under the row's name column
            if !icon.is_empty() {
                if let Some(tex) = icons.get(ui.ctx(), icon) {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(28.0, 28.0)));
                }
            }
            ui.add(egui::Label::new(name).wrap_mode(egui::TextWrapMode::Truncate));

            // Controls hug the right edge. In a right-to-left layout the
            // first-added widget sits rightmost, so add ×, +, qty, − to read
            // left→right as "[−] qty [+]   ×".
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized(BTN, egui::Button::new(egui::RichText::new("×").strong()))
                    .on_hover_text(remove_tip)
                    .clicked()
                {
                    target_set_item(state, target, item_id, 0);
                    notify(state, save_tx);
                }
                ui.add_space(8.0);
                if ui
                    .add_sized(BTN, egui::Button::new(egui::RichText::new("+").strong()))
                    .on_hover_text("Add one")
                    .clicked()
                {
                    target_adjust_item(state, target, item_id, 1);
                    notify(state, save_tx);
                }
                let mut q = *qty;
                if ui
                    .add_sized(
                        egui::vec2(QTY_W, BTN.y),
                        egui::DragValue::new(&mut q).range(0..=9999).speed(0.1),
                    )
                    .on_hover_text("Drag or type a quantity")
                    .changed()
                {
                    target_set_item(state, target, item_id, q);
                    notify(state, save_tx);
                }
                if ui
                    .add_enabled(
                        *qty > 0,
                        egui::Button::new(egui::RichText::new("−").strong()).min_size(BTN),
                    )
                    .on_hover_text("Remove one")
                    .clicked()
                {
                    target_adjust_item(state, target, item_id, -1);
                    notify(state, save_tx);
                }
            });
        });
        ui.add_space(2.0);
    }
}

/// Centered modal: search bar + scrollable tile grid of all catalog items.
/// Clicking a tile adds one of that item to the active target (so clicking N
/// times sets qty N); the modal stays open for bulk entry until the user closes
/// it. Reuses the hideout picker's item list + tile rendering.
fn item_picker_modal(
    ctx: &egui::Context,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let Some(active) = active_picker(ctx) else {
        return;
    };
    // Resolve the title; a container could have been deleted while the picker
    // was open, in which case we bail. The stash always exists.
    let title_name = match &active {
        Target::Stash => "Stash".to_string(),
        Target::Container(id) => match state.read().containers.iter().find(|c| &c.id == id) {
            Some(c) => c.name.clone(),
            None => {
                set_active_picker(ctx, None);
                return;
            }
        },
    };

    let mut open = true;
    let mut filter = picker_filter(ctx);
    let mut chosen: Option<ItemId> = None;

    egui::Window::new(format!("Add items to {title_name}"))
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
        target_adjust_item(state, &active, &item_id, 1);
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

fn active_picker(ctx: &egui::Context) -> Option<Target> {
    ctx.data(|d| d.get_temp::<Target>(egui::Id::new("ctr-picker-active")))
}
fn set_active_picker(ctx: &egui::Context, v: Option<Target>) {
    let key = egui::Id::new("ctr-picker-active");
    match v {
        Some(t) => ctx.data_mut(|d| d.insert_temp(key, t)),
        None => ctx.data_mut(|d| d.remove::<Target>(key)),
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
