//! Left tab: manage the stash + secondary containers (backpacks, item cases).
//!
//! The stash (`AppState::collected`) is the *primary* container — also edited
//! from the preview pane / VR / OCR. This tab pins it on top and lets you
//! manage *secondary* containers below it. Every container's contents sum into
//! owned totals via [`crate::state::AppState::owned_total`] — so adding an item
//! here can flip a hideout upgrade to "ready" exactly like collecting it in the
//! stash, and it feeds the Items DB Quantity / Surplus columns too. Bags and
//! shelves are entered by hand; "Case"-type containers can also be filled by
//! scanning their in-game contents screen — a series of scrolling screenshots
//! stitched into one item list (see [`crate::ocr::box_scan`]).
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
use crate::ocr::box_scan::ScanRow;
use crate::ocr::{BoxCommand, BoxScanStatus, BoxScanUpdate, ScanTarget};
use crate::state::{AppState, ContainerId, ContainerKind};
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

/// Case icons offered when the container's type is `Case`. Only the two
/// Collection Boxes are listed: they're the boxes that store MISC items — what
/// this tracker cares about — whereas the other in-game boxes (mag/attachment,
/// medical, paint) hold specific non-misc categories. Generated flat-3D crate
/// art (upstream has no images for these). Order here is the picker-grid order.
const CASE_ICONS: &[&str] = &["box_collection", "box_collection_small"];
/// Default icon for a newly-created Case container.
const DEFAULT_CASE_ICON: &str = "box_collection";

/// Shelf icons (declarative furniture). One for now.
const SHELF_ICONS: &[&str] = &["shelf_basic"];
/// Default icon for a Shelf container.
const DEFAULT_SHELF_ICON: &str = "shelf_basic";

/// The default icon key for a container of the given kind.
fn default_icon_for(kind: ContainerKind) -> &'static str {
    match kind {
        ContainerKind::Case => DEFAULT_CASE_ICON,
        ContainerKind::Shelf => DEFAULT_SHELF_ICON,
        ContainerKind::Bag => DEFAULT_CONTAINER_ICON,
    }
}

/// The icon set offered in the picker for a container of the given kind.
fn icon_set_for(kind: ContainerKind) -> &'static [&'static str] {
    match kind {
        ContainerKind::Case => CASE_ICONS,
        ContainerKind::Shelf => SHELF_ICONS,
        ContainerKind::Bag => CONTAINER_ICONS,
    }
}

/// Short display name for an icon key, shown under each tile in the picker so
/// the user can tell them apart. Case names match the in-game boxes.
fn icon_label(key: &str) -> &'static str {
    match key {
        // Cases (the two MISC-storing Collection Boxes).
        "box_collection" => "Collection",
        "box_collection_small" => "Collection small",
        // Shelf.
        "shelf_basic" => "Shelf",
        // Bags.
        "backpack_3drt" => "3DRT",
        "backpack_eliteops" => "Elite Ops",
        "backpack_eliteops_green" => "Elite Ops (grn)",
        "backpack_6sh118" => "6Sh118",
        "backpack_robinson" => "Robinson",
        "backpack_hypertec" => "Hypertec",
        "backpack_gnjbackpack" => "GNJ",
        "backpack_rucksack" => "Rucksack",
        "backpack_sportbag" => "Sport bag",
        "backpack_odldos_black" => "Odldos",
        "backpack_odldos_flower" => "Odldos (fl.)",
        _ => "",
    }
}

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

// Fill + label colors for the two-step delete "Confirm" button. The label
// color is pinned rather than left to the theme on purpose: egui derives a
// button's text color from the theme's widget stroke, which is *dark* in light
// mode and renders near-invisibly on this red fill (the readability regression
// issue #103 calls out). White clears WCAG AA against the fill, and because
// both colors are explicit the contrast holds identically in light and dark.
// `delete_confirm_button_text_is_readable_on_its_fill` in `mod tests` guards it.
const DELETE_CONFIRM_FILL: egui::Color32 = egui::Color32::DARK_RED;
const DELETE_CONFIRM_TEXT: egui::Color32 = egui::Color32::WHITE;

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

/// GUI-side state of a box-scan session. Owned by `App`, threaded into this
/// pane; `None` when no scan is running. Only one runs at a time.
pub enum BoxScanUi {
    /// Capturing scroll shots. The worker stitches each into `latest` as it
    /// arrives over the update channel.
    Scanning {
        target: ScanTarget,
        target_name: String,
        latest: Option<BoxScanUpdate>,
    },
    /// Capturing finished; the confirm/preview modal is up, awaiting Apply. Holds
    /// the captured rows (not a flat tally) so the user can drop a bad row in
    /// review — the tally/diff are recomputed from whatever rows survive.
    Reviewing {
        target: ScanTarget,
        target_name: String,
        rows: Vec<ScanRow>,
        observed_weight: Option<f32>,
    },
}

/// The handles this pane needs to drive a box-scan session: the VR runtime (to
/// toggle box-scan mode), the worker command channel, and the session state.
struct ScanCtx<'a> {
    vr: &'a Arc<crate::vr::Runtime>,
    cmd_tx: &'a Sender<BoxCommand>,
    ui_state: &'a mut Option<BoxScanUi>,
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
    /// Bag vs box — partitions Declarative (bags) from Primary (boxes). The
    /// stash is rendered separately in Primary and ignores this.
    kind: ContainerKind,
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

#[allow(clippy::too_many_arguments)]
pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    vr: &Arc<crate::vr::Runtime>,
    box_cmd_tx: &Sender<BoxCommand>,
    box_scan: &mut Option<BoxScanUi>,
) {
    let mut scan = ScanCtx {
        vr,
        cmd_tx: box_cmd_tx,
        ui_state: box_scan,
    };
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
        .color(ui.visuals().text_color()),
    );
    ui.add_space(8.0);

    // Snapshot the stash row + every container row in one read lock.
    let (stash_row, rows) = {
        let s = state.read();
        let (sc, sw, sv) = compute_kpis(&s, &s.collected);
        let stash_row = Row {
            target: Target::Stash,
            name: "Stash".to_string(),
            display_icon: STASH_ICON.to_string(),
            icon: None,
            kind: ContainerKind::Bag,
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
                    kind: c.kind,
                    item_count: count,
                    weight,
                    value,
                }
            })
            .collect();
        (stash_row, rows)
    };

    // Group secondary containers by kind into their own sections. Primary is
    // the stash alone — the store hideout upgrades consume from — then Cases
    // (scannable), Shelves, and Bags (both declarative/manual). Empty
    // secondary sections are hidden; the "New container" button adds any kind.
    let sort = sort_state(ui.ctx());
    let mut case_rows: Vec<Row> = Vec::new();
    let mut bag_rows: Vec<Row> = Vec::new();
    let mut shelf_rows: Vec<Row> = Vec::new();
    for row in rows {
        match row.kind {
            ContainerKind::Case => case_rows.push(row),
            ContainerKind::Shelf => shelf_rows.push(row),
            ContainerKind::Bag => bag_rows.push(row),
        }
    }
    sort_rows(&mut case_rows, sort);
    sort_rows(&mut bag_rows, sort);
    sort_rows(&mut shelf_rows, sort);

    // --- Primary: the stash. ---
    ui.add_space(2.0);
    section_title(ui, "Primary storage");
    section_caption(
        ui,
        "The stash — where items must be for hideout upgrades. Scannable.",
    );
    column_header(ui, true);
    container_row(ui, state, icons, save_tx, &mut scan, &stash_row);

    // --- Secondary sections, each shown only when it has containers. Order:
    // Cases (scannable) first, then the manual kinds Shelves and Bags. ---
    for (title, caption, section_rows) in [
        (
            "Cases",
            "Contents can be filled by scanning their screen.",
            &case_rows,
        ),
        ("Shelves", "Hideout shelves — entered by hand.", &shelf_rows),
        ("Bags", "Entered by hand — no screen to scan.", &bag_rows),
    ] {
        if section_rows.is_empty() {
            continue;
        }
        ui.add_space(18.0);
        section_title(ui, title);
        section_caption(ui, caption);
        column_header(ui, true);
        for row in section_rows {
            container_row(ui, state, icons, save_tx, &mut scan, row);
        }
    }

    // "New container" adds either kind (type chosen in the modal); sits below
    // both sections.
    ui.add_space(6.0);
    if ui
        .add(egui::Button::new(
            egui::RichText::new("➕  New container").strong(),
        ))
        .on_hover_text(
            "Create a case (scannable) or a shelf/bag (manual): name it, pick a type + icon",
        )
        .clicked()
    {
        open_new_container_modal(ui.ctx());
    }

    // The add-item picker (one target at a time, keyed in egui memory).
    item_picker_modal(ui.ctx(), state, icons, save_tx);

    // The "New container" modal (name + icon grid), when open.
    new_container_modal(ui.ctx(), state, icons, save_tx);

    // The centered live-scan window, while a session is capturing.
    box_scan_live_window(ui.ctx(), state, &mut scan);

    // The box-scan confirm/preview modal, when a session is in its review phase.
    box_review_modal(ui.ctx(), state, save_tx, &mut scan);
}

// --- "New container" modal -------------------------------------------------

const NEW_NAME_KEY: &str = "ctr-new-name";
const NEW_ICON_KEY: &str = "ctr-new-icon";
const NEW_OPEN_KEY: &str = "ctr-new-open";
const NEW_FOCUS_KEY: &str = "ctr-new-focus";
/// When present, the modal is editing this existing container (Save writes to
/// it) rather than creating a new one.
const NEW_EDIT_KEY: &str = "ctr-new-edit-id";
/// Chosen [`ContainerKind`] in the create/edit modal.
const NEW_KIND_KEY: &str = "ctr-new-kind";

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
        d.insert_temp(egui::Id::new(NEW_KIND_KEY), ContainerKind::default());
        d.insert_temp(egui::Id::new(NEW_FOCUS_KEY), true);
    });
}

/// Open the modal in *edit* mode: pre-fill the given container's name, icon, and
/// kind; Save writes back to it.
fn open_edit_container_modal(
    ctx: &egui::Context,
    id: &ContainerId,
    name: &str,
    icon: &str,
    kind: ContainerKind,
) {
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_OPEN_KEY), true);
        d.insert_temp(egui::Id::new(NEW_EDIT_KEY), id.clone());
        d.insert_temp(egui::Id::new(NEW_NAME_KEY), name.to_string());
        d.insert_temp(egui::Id::new(NEW_ICON_KEY), icon.to_string());
        d.insert_temp(egui::Id::new(NEW_KIND_KEY), kind);
        d.insert_temp(egui::Id::new(NEW_FOCUS_KEY), true);
    });
}

fn close_new_container_modal(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.insert_temp(egui::Id::new(NEW_OPEN_KEY), false);
        d.remove::<ContainerId>(egui::Id::new(NEW_EDIT_KEY));
        d.remove::<String>(egui::Id::new(NEW_NAME_KEY));
        d.remove::<String>(egui::Id::new(NEW_ICON_KEY));
        d.remove::<ContainerKind>(egui::Id::new(NEW_KIND_KEY));
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
    let mut kind = ctx
        .data(|d| d.get_temp::<ContainerKind>(egui::Id::new(NEW_KIND_KEY)))
        .unwrap_or_default();
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
            ui.horizontal(|ui| {
                ui.label("Type:");
                ui.selectable_value(&mut kind, ContainerKind::Case, "Case")
                    .on_hover_text(
                        "An item case with a contents screen — fill it by scanning screenshots",
                    );
                ui.selectable_value(&mut kind, ContainerKind::Shelf, "Shelf")
                    .on_hover_text("A shelf — manual entry, its own category");
                ui.selectable_value(&mut kind, ContainerKind::Bag, "Bag")
                    .on_hover_text("A backpack or pouch — contents entered by hand");
            });

            // Icon set follows the (possibly just-changed) type. If the current
            // selection isn't valid for the new set, snap to that set's default.
            let icon_set = icon_set_for(kind);
            if !icon_set.contains(&chosen_icon.as_str()) {
                chosen_icon = default_icon_for(kind).to_string();
            }
            ui.add_space(10.0);
            ui.label("Icon:");
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for &key in icon_set {
                    ui.allocate_ui_with_layout(
                        egui::vec2(104.0, 126.0),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            if icon_choice(ui, icons, key, key == chosen_icon, 88.0) {
                                chosen_icon = key.to_string();
                            }
                            ui.label(egui::RichText::new(icon_label(key)).small());
                        },
                    );
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
        d.insert_temp(egui::Id::new(NEW_KIND_KEY), kind);
    });

    if do_create && !name.trim().is_empty() {
        let trimmed = name.trim().to_string();
        match &edit_id {
            Some(id) => {
                // Edit mode: write back to the existing container.
                let mut w = state.write();
                w.rename_container(id, trimmed);
                w.set_container_icon(id, Some(chosen_icon));
                w.set_container_kind(id, kind);
            }
            None => {
                let id = state.write().create_container(trimmed);
                let mut w = state.write();
                w.set_container_icon(&id, Some(chosen_icon));
                w.set_container_kind(&id, kind);
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

/// One-line explanatory caption under a section title.
fn section_caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(ui.visuals().weak_text_color()),
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
    scan: &mut ScanCtx,
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
                row.kind,
            );
        }
    }

    if toggle_resp.clicked() {
        collapse.toggle(ui);
    }
    collapse.store(ui.ctx());

    // Body: the item list + add-item, only while expanded.
    if is_open {
        container_body(ui, state, icons, save_tx, scan, row);
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
    kind: ContainerKind,
) {
    if ui
        .button("Edit")
        .on_hover_text("Rename, change the icon, or switch type")
        .clicked()
    {
        open_edit_container_modal(ui.ctx(), id, name, icon_key, kind);
    }
    if pending_delete(ui.ctx()).as_deref() == Some(id.as_str()) {
        if ui
            .add(
                egui::Button::new(egui::RichText::new("Confirm").color(DELETE_CONFIRM_TEXT))
                    .fill(DELETE_CONFIRM_FILL),
            )
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
    scan: &mut ScanCtx,
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
        // The stash is primary, scannable storage too.
        box_scan_section(ui, ScanTarget::Stash, "Stash", scan);
        ui.add_space(4.0);
    }

    // Box-scan controls — cases are scannable storage; bags/shelves are manual.
    if let Target::Container(id) = &row.target {
        if row.kind == ContainerKind::Case {
            box_scan_section(ui, ScanTarget::Container(id.clone()), &row.name, scan);
            ui.add_space(4.0);
        }
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

/// The target's items, one row each: icon + name on the left, then a
/// `[−] qty [+]  ×` control cluster sitting in the same actions column as the
/// container's Edit/Delete buttons (not hugging the window's right edge). Each
/// row gets a hover highlight so it's obvious which item a control acts on.
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
        ui.horizontal(|ui| {
            ui.add_space(LEAD);
            ui.label(
                egui::RichText::new("Empty — add items below.")
                    .small()
                    .italics()
                    .color(ui.visuals().weak_text_color()),
            );
        });
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
    const ITEM_ROW_H: f32 = 32.0;

    for (item_id, name, icon, qty) in &items {
        let full_w = ui.available_width();
        let (row_rect, row_resp) =
            ui.allocate_exact_size(egui::vec2(full_w, ITEM_ROW_H), egui::Sense::hover());
        let c = cols(row_rect);
        if !ui.is_rect_visible(row_rect) {
            continue;
        }

        // Per-row hover highlight: makes it unambiguous which item the
        // controls on this line belong to.
        if row_resp.hovered() {
            ui.painter()
                .rect_filled(row_rect, 3.0, theme::row_hover(ui.visuals().dark_mode));
        }

        // Icon (in the icon column) + name (spanning name..value, truncated).
        if !icon.is_empty() {
            if let Some(tex) = icons.get(ui.ctx(), icon) {
                let icon_rect =
                    egui::Rect::from_center_size(c.icon.center(), egui::vec2(24.0, 24.0));
                egui::Image::new(tex).paint_at(ui, icon_rect);
            }
        }
        let name_rect = egui::Rect::from_min_max(c.name.left_top(), c.value.right_bottom());
        put_left(ui, name_rect, egui::RichText::new(name));

        // Controls in the actions column, aligned under Edit/Delete and read
        // left→right as "[−] qty [+]   ×".
        let mut a = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(c.actions.shrink2(egui::vec2(CELL_PAD, 2.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        if a.add_enabled(
            *qty > 0,
            egui::Button::new(egui::RichText::new("−").strong()).min_size(BTN),
        )
        .on_hover_text("Remove one")
        .clicked()
        {
            target_adjust_item(state, target, item_id, -1);
            notify(state, save_tx);
        }
        let mut q = *qty;
        if a.add_sized(
            egui::vec2(QTY_W, BTN.y),
            egui::DragValue::new(&mut q).range(0..=9999).speed(0.1),
        )
        .on_hover_text("Drag or type a quantity")
        .changed()
        {
            target_set_item(state, target, item_id, q);
            notify(state, save_tx);
        }
        if a.add_sized(BTN, egui::Button::new(egui::RichText::new("+").strong()))
            .on_hover_text("Add one")
            .clicked()
        {
            target_adjust_item(state, target, item_id, 1);
            notify(state, save_tx);
        }
        a.add_space(8.0);
        if a.add_sized(BTN, egui::Button::new(egui::RichText::new("×").strong()))
            .on_hover_text(remove_tip)
            .clicked()
        {
            target_set_item(state, target, item_id, 0);
            notify(state, save_tx);
        }
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

// --- Box-scan session UI ---------------------------------------------------

const SCAN_HINT: &str = "Open the box's screen in-game and scroll slowly — it captures \
                         automatically as new rows come into view. Press SPACE to grab one by hand.";
const WARN_COL: egui::Color32 = egui::Color32::from_rgb(200, 140, 0);
/// Red used for the "this REPLACES current contents" warning and the removed
/// rows in the review diff. Matches the diff's `removed` swatch (210,90,90).
const DANGER_COL: egui::Color32 = egui::Color32::from_rgb(210, 90, 90);

/// The one-line caution shown both during the live scan and atop the review
/// modal: applying a scan is a full replace, not a merge. `{name}` is the
/// target store ("Stash" or the container name).
fn flush_warning(name: &str) -> String {
    format!(
        "Applying will REPLACE everything in “{name}” with the scan — items not seen are removed."
    )
}

/// Sum of `Item.weight × count` over a tally — the "computed" side of the
/// box-screen weight checksum.
fn computed_weight(s: &AppState, tally: &HashMap<ItemId, u32>) -> f32 {
    tally
        .iter()
        .map(|(id, &n)| {
            s.index
                .items_by_id
                .get(id)
                .and_then(|it| it.weight)
                .unwrap_or(0.0)
                * n as f32
        })
        .sum()
}

/// One captured row rendered as its tile item names, left→right; `—` marks an
/// unrecognized tile.
fn row_label(s: &AppState, row: &ScanRow) -> String {
    row.tiles
        .iter()
        .map(|t| match t {
            Some(id) => s
                .index
                .items_by_id
                .get(id)
                .map(|it| it.name.clone())
                .unwrap_or_else(|| id.clone()),
            None => "—".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Render a scan's captured rows "as captured", one labelled line each. With a
/// `drop_sink`, every row gets a ✕ that pushes its id into the sink (the caller
/// removes it after the frame); without one the list is read-only. Used
/// read-only in the live scan window and with drop enabled in the review modal.
fn rows_view(
    ui: &mut egui::Ui,
    s: &AppState,
    rows: &[ScanRow],
    mut drop_sink: Option<&mut Vec<u64>>,
    id_salt: &str,
) {
    ui.label(
        egui::RichText::new(format!("Rows captured ({})", rows.len()))
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .id_salt(id_salt)
        .show(ui, |ui| {
            for row in rows {
                ui.horizontal(|ui| {
                    if let Some(sink) = drop_sink.as_deref_mut() {
                        if ui
                            .add(egui::Button::new("Drop").small())
                            .on_hover_text("Drop this row from the scan")
                            .clicked()
                        {
                            sink.push(row.id);
                        }
                    }
                    ui.label(row_label(s, row));
                });
            }
        });
}

/// `(id, name, count)` rows for a tally, names resolved from the data index.
fn tally_rows(s: &AppState, tally: &HashMap<ItemId, u32>) -> Vec<(ItemId, String, u32)> {
    tally
        .iter()
        .map(|(id, &n)| {
            let name = s
                .index
                .items_by_id
                .get(id)
                .map(|it| it.name.clone())
                .unwrap_or_else(|| id.clone());
            (id.clone(), name, n)
        })
        .collect()
}

fn warn_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(WARN_COL));
}

/// Like [`warn_label`] but in the stronger [`DANGER_COL`] red and bold — for
/// the destructive REPLACE/flush caution, not a soft recapture hint.
fn warn_label_danger(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(DANGER_COL).strong());
}

/// Inline box-scan control for one store's row: the "Scan from screen" start
/// button, or — while *this* store is the active scan — a compact stub that
/// points at the centered live window (rendered by [`box_scan_live_window`]).
/// The full live panel deliberately lives in that floating window, not here, so
/// it can't scroll off-screen inside the container table.
fn box_scan_section(ui: &mut egui::Ui, target: ScanTarget, target_name: &str, scan: &mut ScanCtx) {
    let scanning_this = matches!(
        scan.ui_state.as_ref(),
        Some(BoxScanUi::Scanning { target: t, .. }) if *t == target
    );
    let busy_elsewhere = scan.ui_state.is_some() && !scanning_this;

    ui.horizontal(|ui| {
        ui.add_space(LEAD);
        if scanning_this {
            // The controls live in the centered window; just signpost it here.
            ui.label(
                egui::RichText::new("● Scanning… (see the scan window)")
                    .color(WARN_COL)
                    .strong(),
            );
            return;
        }
        let btn = egui::Button::new("Scan from screen");
        if busy_elsewhere {
            ui.add_enabled(false, btn)
                .on_hover_text("Finish the active scan first");
        } else if ui
            .add(btn)
            .on_hover_text("Read this store's contents screen across several scroll captures")
            .clicked()
        {
            scan.vr.set_box_scan_mode(true);
            let _ = scan.cmd_tx.send(BoxCommand::Start {
                target: target.clone(),
            });
            *scan.ui_state = Some(BoxScanUi::Scanning {
                target: target.clone(),
                target_name: target_name.to_string(),
                latest: None,
            });
        }
    });
}

/// The centered, always-visible live-scan window, shown while a box scan is in
/// its capture phase. Rendered once at the top level (only one scan is ever
/// active) so it floats above the container table instead of scrolling with a
/// row. Shows the running tally, status, weight checksum, the up-front
/// REPLACE/flush warning, and the Finish / Cancel controls.
fn box_scan_live_window(ctx: &egui::Context, state: &Arc<RwLock<AppState>>, scan: &mut ScanCtx) {
    let (target_name, latest) = match scan.ui_state.as_ref() {
        Some(BoxScanUi::Scanning {
            target_name,
            latest,
            ..
        }) => (target_name.clone(), latest.clone()),
        _ => return,
    };

    let mut finish = false;
    let mut cancel = false;
    // The scan window's hands-free toggle (default on, re-armed on each scan
    // start). Mirrors the finish/cancel pattern: read current state, let the
    // checkbox edit a local, push the change to the runtime after the window.
    let mut auto_capture = scan.vr.box_auto_capture_enabled();
    let mut auto_changed = false;

    egui::Window::new(format!("Box scan — {target_name}"))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(SCAN_HINT)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(4.0);
            if ui
                .checkbox(&mut auto_capture, "Auto-capture while scrolling")
                .on_hover_text(
                    "On: captures continuously as you scroll (recommended). \
                     Off: capture each scroll position yourself with SPACE.",
                )
                .changed()
            {
                auto_changed = true;
            }
            // Up-front flush caution: the user should know this is a replace
            // before they invest several captures, not only at the review step.
            ui.add_space(4.0);
            warn_label_danger(ui, &flush_warning(&target_name));
            ui.separator();

            match &latest {
                None => {
                    ui.label(egui::RichText::new("Waiting for the first capture…").italics());
                }
                Some(u) => {
                    let total: u32 = u.tally.values().sum();
                    let mut line = format!("{} capture(s) · {} item(s)", u.captures, total);
                    if u.unrecognized > 0 {
                        line.push_str(&format!(" · {} unrecognized", u.unrecognized));
                    }
                    ui.label(line);

                    // What the most recent shot contributed, distinct from the
                    // cumulative line above (the in-headset card shows the same).
                    let mut last_line = match u.status {
                        BoxScanStatus::Ok => format!(
                            "This capture: +{} rows ({} already had)",
                            u.last_rows_added, u.last_rows_duplicate
                        ),
                        BoxScanStatus::NoTiles => "This capture: no items seen".to_string(),
                    };
                    if u.last_unrecognized > 0 {
                        last_line.push_str(&format!(" · {} unrecognized", u.last_unrecognized));
                    }
                    ui.label(
                        egui::RichText::new(last_line)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );

                    if u.status == BoxScanStatus::NoTiles {
                        warn_label(
                            ui,
                            "Last shot saw no items — make sure the box screen is visible.",
                        );
                    }

                    if let Some(observed) = u.observed_weight {
                        let computed = computed_weight(&state.read(), &u.tally);
                        let close = (computed - observed).abs() <= (observed * 0.1).max(0.5);
                        let col = if close {
                            egui::Color32::from_rgb(80, 170, 90)
                        } else {
                            WARN_COL
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "weight: computed {computed:.1} / observed {observed:.1} kg"
                            ))
                            .small()
                            .color(col),
                        );
                    }

                    let mut item_rows = tally_rows(&state.read(), &u.tally);
                    item_rows.sort_by(|a, b| {
                        b.2.cmp(&a.2)
                            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                    });
                    if !item_rows.is_empty() {
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .id_salt("box-scan-tally")
                            .show(ui, |ui| {
                                for (_, name, qty) in &item_rows {
                                    ui.label(format!("  {qty} × {name}"));
                                }
                            });
                    }

                    // The same rows shown read-only "as captured"; the user can
                    // drop a bad one in the Finish & review step.
                    if !u.rows.is_empty() {
                        ui.separator();
                        rows_view(ui, &state.read(), &u.rows, None, "box-scan-rows-live");
                    }
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Finish & review").strong(),
                    ))
                    .clicked()
                {
                    finish = true;
                }
                if ui.button("Cancel scan").clicked() {
                    cancel = true;
                }
            });
        });

    if auto_changed {
        scan.vr.set_box_auto_capture(auto_capture);
    }
    if finish {
        if let Some(BoxScanUi::Scanning {
            target,
            target_name,
            latest,
        }) = scan.ui_state.take()
        {
            scan.vr.set_box_scan_mode(false);
            let _ = scan.cmd_tx.send(BoxCommand::Finish);
            let (rows, observed_weight) = latest
                .map(|u| (u.rows, u.observed_weight))
                .unwrap_or_default();
            *scan.ui_state = Some(BoxScanUi::Reviewing {
                target,
                target_name,
                rows,
                observed_weight,
            });
        }
    } else if cancel {
        scan.vr.set_box_scan_mode(false);
        let _ = scan.cmd_tx.send(BoxCommand::Cancel);
        *scan.ui_state = None;
    }
}

/// The Finish confirm/preview modal: a diff of the scanned tally vs the
/// container's current contents (new green, changed amber, removed red), a
/// weight checksum, and Apply / Discard. Apply does a REPLACE — scanned counts
/// overwrite, and items absent from the scan are removed.
fn box_review_modal(
    ctx: &egui::Context,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    scan: &mut ScanCtx,
) {
    // Own a snapshot, ending the borrow on `scan.ui_state` before we mutate it.
    let (target, target_name, rows, observed_weight) = match scan.ui_state.as_ref() {
        Some(BoxScanUi::Reviewing {
            target,
            target_name,
            rows,
            observed_weight,
        }) => (
            target.clone(),
            target_name.clone(),
            rows.clone(),
            *observed_weight,
        ),
        _ => return,
    };
    // The tally/diff are derived from the surviving rows, so dropping a row below
    // immediately re-prices the review next frame.
    let (tally, unrecognized) = crate::ocr::box_scan::tally_rows(&rows);

    let mut open = true;
    let mut apply = false;
    let mut discard = false;
    // Rows the user clicked ✕ on this frame; applied to the Reviewing state after
    // the window closes (immediate-mode: can't mutate `scan.ui_state` mid-render).
    let mut dropped: Vec<u64> = Vec::new();

    egui::Window::new(format!("Apply box scan to {target_name}?"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let s = state.read();
            let current: HashMap<ItemId, u32> = match &target {
                ScanTarget::Stash => s.collected.clone(),
                ScanTarget::Container(cid) => s
                    .containers
                    .iter()
                    .find(|c| &c.id == cid)
                    .map(|c| c.contents.clone())
                    .unwrap_or_default(),
            };

            let total: u32 = tally.values().sum();
            ui.label(format!(
                "Replace contents with the scan: {} item(s), {} type(s).",
                total,
                tally.len()
            ));
            warn_label_danger(ui, &flush_warning(&target_name));
            if unrecognized > 0 {
                warn_label(
                    ui,
                    &format!("{unrecognized} tile(s) weren't recognized and are left out."),
                );
            }
            if let Some(observed) = observed_weight {
                let computed = computed_weight(&s, &tally);
                ui.label(
                    egui::RichText::new(format!(
                        "weight: computed {computed:.1} / observed {observed:.1} kg"
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            }
            ui.separator();

            // Captured rows, with a ✕ to drop a bad one (a clipped half-row,
            // mis-merge, or hallucinated row). Dropping re-prices the diff below
            // next frame.
            rows_view(ui, &s, &rows, Some(&mut dropped), "box-review-rows");
            ui.separator();

            // Diff over the union of current + scanned ids.
            let mut ids: Vec<ItemId> = current.keys().chain(tally.keys()).cloned().collect();
            ids.sort();
            ids.dedup();
            let mut rows: Vec<(String, u32, u32)> = ids
                .iter()
                .map(|id| {
                    let name = s
                        .index
                        .items_by_id
                        .get(id)
                        .map(|it| it.name.clone())
                        .unwrap_or_else(|| id.clone());
                    (
                        name,
                        *current.get(id).unwrap_or(&0),
                        *tally.get(id).unwrap_or(&0),
                    )
                })
                .collect();
            rows.sort_by_key(|r| r.0.to_lowercase());

            egui::ScrollArea::vertical()
                .max_height(300.0)
                .id_salt("box-review")
                .show(ui, |ui| {
                    egui::Grid::new("box-review-grid")
                        .num_columns(3)
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Item").strong());
                            ui.label(egui::RichText::new("Now").strong());
                            ui.label(egui::RichText::new("After").strong());
                            ui.end_row();
                            for (name, now, after) in &rows {
                                let col = if *after == 0 && *now > 0 {
                                    Some(egui::Color32::from_rgb(210, 90, 90)) // removed
                                } else if *now == 0 && *after > 0 {
                                    Some(egui::Color32::from_rgb(80, 170, 90)) // new
                                } else if now != after {
                                    Some(egui::Color32::from_rgb(205, 165, 60)) // changed
                                } else {
                                    None
                                };
                                let paint = |t: String| match col {
                                    Some(c) => egui::RichText::new(t).color(c),
                                    None => egui::RichText::new(t),
                                };
                                ui.label(paint(name.clone()));
                                ui.label(paint(now.to_string()));
                                ui.label(paint(after.to_string()));
                                ui.end_row();
                            }
                        });
                });

            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Apply (replace contents)").strong())
                            .fill(egui::Color32::from_rgb(60, 120, 70)),
                    )
                    .clicked()
                {
                    apply = true;
                }
                if ui.button("Discard").clicked() {
                    discard = true;
                }
            });
        });

    if apply {
        // REPLACE: scanned counts overwrite, and items present before but absent
        // from the scan are removed — for the stash or a container alike.
        {
            let mut w = state.write();
            match &target {
                ScanTarget::Stash => {
                    let current_ids: Vec<ItemId> = w.collected.keys().cloned().collect();
                    for (id, &n) in &tally {
                        w.set_collected(id, n);
                    }
                    for id in &current_ids {
                        if !tally.contains_key(id) {
                            w.set_collected(id, 0);
                        }
                    }
                }
                ScanTarget::Container(cid) => {
                    let current_ids: Vec<ItemId> = w
                        .containers
                        .iter()
                        .find(|c| &c.id == cid)
                        .map(|c| c.contents.keys().cloned().collect())
                        .unwrap_or_default();
                    for (id, &n) in &tally {
                        w.set_container_item(cid, id, n);
                    }
                    for id in &current_ids {
                        if !tally.contains_key(id) {
                            w.set_container_item(cid, id, 0);
                        }
                    }
                }
            }
        }
        notify(state, save_tx);
        *scan.ui_state = None;
    } else if discard || !open {
        *scan.ui_state = None;
    } else if !dropped.is_empty() {
        // Apply this frame's row drops to the live Reviewing state; the tally and
        // diff recompute from the survivors next frame.
        if let Some(BoxScanUi::Reviewing { rows, .. }) = scan.ui_state.as_mut() {
            rows.retain(|r| !dropped.contains(&r.id));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Headless GUI tests for the Containers pane, driving `ui` through
    //! `egui_kittest` (issue #103). Two regression classes are covered without
    //! a real window:
    //!   * **interaction** — clicking Delete → Confirm actually removes the
    //!     right container (and Cancel backs out), proving the wiring and the
    //!     two-step guard;
    //!   * **theme readability** — the colored Confirm button's pinned label
    //!     color clears a WCAG contrast ratio against its fill, so it can't
    //!     regress to dark-on-dark in light mode.
    use super::*;
    use crate::data::GameData;
    use crate::gui::theme::contrast::contrast_ratio;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    /// Minimal game data: the pane only consults the item catalog to sum KPI
    /// weight/value, so an empty catalog (and no hideout modules) is enough to
    /// render the rows and their Edit/Delete actions.
    fn game_data() -> Arc<GameData> {
        Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: Vec::new(),
            items: Vec::new(),
        })
    }

    /// Build a headless harness driving [`ui`] over `state`. The closure owns a
    /// clone of the `Arc`, its own icon cache, and a dead-end save channel (the
    /// pane ignores send errors), so the harness is `'static` and the caller
    /// keeps `state` for assertions. Sized wide and tall enough that every row
    /// and its action buttons are laid out — and therefore reachable by the
    /// AccessKit queries — rather than clipped.
    fn harness(state: &Arc<RwLock<AppState>>) -> Harness<'static> {
        let ui_state = Arc::clone(state);
        let (save_tx, _save_rx) = crossbeam_channel::unbounded::<SaveTick>();
        // The pane now takes a VR runtime + box-scan plumbing for the
        // contents-scan feature. None of the delete/contrast cases below start a
        // scan, so a worker-less runtime, a drained command channel, and an
        // empty session are enough to satisfy the signature.
        let (box_cmd_tx, _box_cmd_rx) = crossbeam_channel::unbounded::<BoxCommand>();
        let vr = Arc::new(crate::vr::Runtime::disconnected_for_test());
        let mut icons = IconCache::new();
        let mut box_scan: Option<BoxScanUi> = None;
        Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui(move |ui| {
                super::ui(
                    ui,
                    &ui_state,
                    &mut icons,
                    &save_tx,
                    &vr,
                    &box_cmd_tx,
                    &mut box_scan,
                )
            })
    }

    #[test]
    fn delete_confirm_removes_the_container() {
        let state = Arc::new(RwLock::new(AppState::new(game_data())));
        state.write().create_container("Backpack".into());
        let mut h = harness(&state);
        h.run();

        // Two-step delete: the first click only arms the confirm, the second
        // commits it.
        h.get_by_label("Delete").click();
        h.run();
        h.get_by_label("Confirm").click();
        h.run();

        assert!(
            state.read().containers.is_empty(),
            "Delete → Confirm should remove the container"
        );
    }

    #[test]
    fn delete_confirm_keeps_other_containers() {
        let state = Arc::new(RwLock::new(AppState::new(game_data())));
        let alpha = state.write().create_container("Alpha".into());
        let beta = state.write().create_container("Beta".into());
        let mut h = harness(&state);
        // Pin a deterministic row order so the first "Delete" is always Alpha's.
        // (The default sort is value-descending; both containers are empty, so
        // the value tiebreak would put them in a non-obvious order.)
        set_sort_state(
            &h.ctx,
            Sort {
                col: SortCol::Name,
                desc: false,
            },
        );
        h.run();

        // Both rows render a "Delete"; the first belongs to Alpha.
        h.get_all_by_label("Delete")
            .next()
            .expect("a Delete button per secondary container")
            .click();
        h.run();
        h.get_by_label("Confirm").click();
        h.run();

        let s = state.read();
        assert_eq!(s.containers.len(), 1, "exactly one container removed");
        assert_eq!(s.containers[0].id, beta, "the untouched sibling remains");
        assert!(
            !s.containers.iter().any(|c| c.id == alpha),
            "the targeted container is gone"
        );
    }

    #[test]
    fn delete_cancel_keeps_the_container() {
        let state = Arc::new(RwLock::new(AppState::new(game_data())));
        state.write().create_container("Backpack".into());
        let mut h = harness(&state);
        h.run();

        h.get_by_label("Delete").click();
        h.run();
        // The guard's whole point: backing out leaves the container untouched.
        h.get_by_label("Cancel").click();
        h.run();

        assert_eq!(
            state.read().containers.len(),
            1,
            "Delete → Cancel must not remove anything"
        );
        // The row is back to offering Delete, not stuck mid-confirm.
        assert!(h.query_by_label("Delete").is_some());
        assert!(h.query_by_label("Confirm").is_none());
    }

    #[test]
    fn delete_confirm_button_text_is_readable_on_its_fill() {
        // The colored Confirm button pins its label color instead of inheriting
        // the theme's (dark) button stroke, which would be near-invisible on the
        // red fill in light mode. White must clear WCAG AA (4.5:1) against the
        // fill; both colors are explicit, so the ratio holds in either theme.
        let ratio = contrast_ratio(DELETE_CONFIRM_TEXT, DELETE_CONFIRM_FILL);
        assert!(
            ratio >= 4.5,
            "Confirm label/fill contrast {ratio:.2}:1 is below WCAG AA 4.5:1 — \
             a colored button must pin a readable label color"
        );
    }

    /// Like [`harness`] but shares the box-scan UI state with the caller, so a
    /// test can seed a `Reviewing` session and read back what survives a row drop
    /// (the plain `harness` owns `box_scan` privately inside the closure).
    fn review_harness(
        state: &Arc<RwLock<AppState>>,
        box_scan: &Arc<RwLock<Option<BoxScanUi>>>,
    ) -> Harness<'static> {
        let ui_state = Arc::clone(state);
        let box_scan = Arc::clone(box_scan);
        let (save_tx, _save_rx) = crossbeam_channel::unbounded::<SaveTick>();
        let (box_cmd_tx, _box_cmd_rx) = crossbeam_channel::unbounded::<BoxCommand>();
        let vr = Arc::new(crate::vr::Runtime::disconnected_for_test());
        let mut icons = IconCache::new();
        Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui(move |ui| {
                let mut guard = box_scan.write();
                super::ui(
                    ui,
                    &ui_state,
                    &mut icons,
                    &save_tx,
                    &vr,
                    &box_cmd_tx,
                    &mut guard,
                );
            })
    }

    #[test]
    fn review_drop_removes_a_row_from_the_scan() {
        let state = Arc::new(RwLock::new(AppState::new(game_data())));
        let row = |id: u64, ids: &[&str]| ScanRow {
            id,
            tiles: ids.iter().map(|s| Some(s.to_string())).collect(),
        };
        // A two-row Reviewing session, awaiting Apply.
        let box_scan = Arc::new(RwLock::new(Some(BoxScanUi::Reviewing {
            target: ScanTarget::Stash,
            target_name: "Stash".into(),
            rows: vec![row(10, &["a", "b", "c"]), row(11, &["d", "e", "f"])],
            observed_weight: None,
        })));

        let mut h = review_harness(&state, &box_scan);
        h.run();

        // One "Drop" per captured row; drop the first (id 10).
        h.get_all_by_label("Drop")
            .next()
            .expect("a Drop button per captured row")
            .click();
        h.run();

        let guard = box_scan.read();
        let Some(BoxScanUi::Reviewing { rows, .. }) = &*guard else {
            panic!("should still be reviewing after a row drop");
        };
        assert_eq!(rows.len(), 1, "dropping a row must remove exactly one");
        assert_eq!(rows[0].id, 11, "the first row (id 10) is the one dropped");
    }
}
