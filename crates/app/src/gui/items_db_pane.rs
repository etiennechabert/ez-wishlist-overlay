//! Left tab: item database, with a name filter, a category filter, and
//! click-to-sort columns. Read-only reference view — no wishlist mutation here.
//! Covers both item families the rest of the app tracks: barter goods
//! (`category == "misc"`) and gunsmith **gun parts** (`category == "gunsmith"`);
//! the "Category" picker narrows to one. Most gun parts carry a weight but no
//! vendor price, so their Value / Total val cells read "—".
//!
//! Quantity / surplus / total weight / total value columns reflect the
//! *combined* owned total across the stash (`AppState.collected`, the map OCR
//! captures and the wishlist UI mutates) and every secondary container
//! (`AppState.containers`) — see [`crate::state::AppState::owned_total`]. The
//! Containers column attributes that total to where it lives. Sorting by Total
//! Value makes this tab a "what's my whole inventory worth" view; sorting by
//! Total wt while scoped (via the Container picker) to a weight-capped container
//! — the gunsmith's 30 kg gun-parts storage, a Collection Box — surfaces the
//! heaviest things in it, i.e. what to pull first to get back under the cap.

use crate::data::Item;
use crate::gui::IconCache;
use crate::state::{AppState, ContainerId, NeedHorizon};
use egui_extras::{Column, TableBuilder};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Subcategory,
    Weight,
    Price,
    Rarity,
    Quantity,
    Containers,
    Surplus,
    TotalWeight,
    TotalValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Which container the list is scoped to via the quick "Container" picker.
/// `All` applies no location filter; `Stash` keeps only items held in the
/// primary stash; `Container(id)` keeps only items held in that secondary
/// container. Keyed by id (not name) so it survives renames and stays
/// unambiguous when two containers happen to share a name.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ContainerFilter {
    #[default]
    All,
    Stash,
    Container(ContainerId),
}

/// Which item family the list is scoped to via the "Category" picker. The
/// catalog holds three — barter goods (`misc`), gun parts (`gunsmith`), and
/// medical consumables (`medical`) — and the gun-part family is ~4× larger, so
/// a one-click narrow keeps the table legible. `All` shows every family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CategoryFilter {
    #[default]
    All,
    Barter,
    GunParts,
    Medical,
}

impl CategoryFilter {
    /// The `Item::category` string this filter keeps, or `None` for `All`.
    fn category_key(self) -> Option<&'static str> {
        match self {
            CategoryFilter::All => None,
            CategoryFilter::Barter => Some("misc"),
            CategoryFilter::GunParts => Some("gunsmith"),
            CategoryFilter::Medical => Some("medical"),
        }
    }
}

pub struct ItemsDbState {
    pub filter: String,
    pub sort_by: SortColumn,
    pub sort_dir: SortDir,
    /// When true, hide rows whose item id isn't currently required by
    /// any tracked upgrade (i.e. anything not in
    /// `AppState::active_items()`). Useful when the user wants to see
    /// only what's relevant to their current goals.
    pub tracked_only: bool,
    /// Which upgrade scope the surplus calc treats as "needed" — drives the
    /// Surplus column and the `redundant_only` filter. See
    /// [`crate::state::NeedHorizon`].
    pub horizon: NeedHorizon,
    /// When true, keep only redundant rows — items held in excess of what
    /// `horizon` needs (`surplus > 0`), i.e. safe to spend or sell.
    pub redundant_only: bool,
    /// Quick location filter from the "Container" picker — restricts the list
    /// to a single container (or the stash). See [`ContainerFilter`].
    pub container_filter: ContainerFilter,
    /// Quick type filter from the "Category" picker — barter goods vs gun
    /// parts. See [`CategoryFilter`].
    pub category_filter: CategoryFilter,
}

impl Default for ItemsDbState {
    fn default() -> Self {
        Self {
            filter: String::new(),
            sort_by: SortColumn::Name,
            sort_dir: SortDir::Asc,
            tracked_only: false,
            // Default to the middle horizon: protects the next buildable level
            // of every module without hoarding for deep future levels.
            // Aggressive (Tracked) and cautious (All future) are a click away.
            horizon: NeedHorizon::AllNatural,
            redundant_only: false,
            container_filter: ContainerFilter::All,
            category_filter: CategoryFilter::All,
        }
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    db: &mut ItemsDbState,
) {
    // Snapshot everything we need from state in one read, so the table
    // body below isn't repeatedly grabbing the RwLock per row.
    let (data, owned, stash, containers_by_item, container_list, tracked_ids) = {
        let s = state.read();
        // Stash counts on their own — drive the "Stash ×N" segment in the
        // Containers cell and the `Stash` quick filter.
        let stash: HashMap<String, u32> = s.collected.clone();
        // Combined owned total (stash + every container). Every quantity-driven
        // column below — Qty, Surplus, Total wt/val — reads this, so they all
        // count the whole inventory.
        let mut owned = stash.clone();
        // Reverse index: item id → which secondary containers hold it, as
        // (container id, name, qty). The id backs the per-container quick
        // filter; the name + qty render the cell. Built once here so the
        // virtualized body never scans containers.
        let mut containers_by_item: HashMap<String, Vec<(ContainerId, String, u32)>> =
            HashMap::new();
        for c in &s.containers {
            for (item_id, &qty) in &c.contents {
                *owned.entry(item_id.clone()).or_insert(0) += qty;
                containers_by_item
                    .entry(item_id.clone())
                    .or_default()
                    .push((c.id.clone(), c.name.clone(), qty));
            }
        }
        // Stable display order within a cell, independent of HashMap iteration:
        // by name, then id so same-named containers stay deterministic.
        for parts in containers_by_item.values_mut() {
            parts.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        }
        // (id, name) of every secondary container, for the quick-filter picker —
        // same name-then-id order as the cell segments.
        let mut container_list: Vec<(ContainerId, String)> = s
            .containers
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        container_list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let tracked_ids: HashSet<String> =
            s.active_items().into_iter().map(|a| a.item_id).collect();
        (
            s.data.clone(),
            owned,
            stash,
            containers_by_item,
            container_list,
            tracked_ids,
        )
    };

    // A filter pointing at a since-deleted container would match nothing and
    // read as "this container is empty" — fall back to All instead.
    let stale_container = matches!(
        &db.container_filter,
        ContainerFilter::Container(id) if !container_list.iter().any(|(cid, _)| cid == id)
    );
    if stale_container {
        db.container_filter = ContainerFilter::All;
    }

    ui.horizontal(|ui| {
        ui.label("Category:");
        ui.selectable_value(&mut db.category_filter, CategoryFilter::All, "All")
            .on_hover_text("Barter goods, gun parts, and medical items.");
        ui.selectable_value(&mut db.category_filter, CategoryFilter::Barter, "Barter")
            .on_hover_text("Hideout barter items — the misc catalog.");
        ui.selectable_value(
            &mut db.category_filter,
            CategoryFilter::GunParts,
            "Gun parts",
        )
        .on_hover_text(
            "Gunsmith gun parts — research samples and gun-parts-storage \
                 contents. Most carry a weight but no vendor price.",
        );
        ui.selectable_value(&mut db.category_filter, CategoryFilter::Medical, "Medical")
            .on_hover_text("Medical consumables — bandages, painkillers, syringes, stims.");
    });
    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.text_edit_singleline(&mut db.filter);
        if ui.button("✕").clicked() {
            db.filter.clear();
        }
        ui.add_space(12.0);
        ui.label("Container:");
        let selected_text = match &db.container_filter {
            ContainerFilter::All => "All containers".to_owned(),
            ContainerFilter::Stash => "Stash".to_owned(),
            ContainerFilter::Container(id) => container_list
                .iter()
                .find(|(cid, _)| cid == id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "All containers".to_owned()),
        };
        egui::ComboBox::from_id_salt("itemsdb-container-filter")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut db.container_filter,
                    ContainerFilter::All,
                    "All containers",
                );
                ui.selectable_value(&mut db.container_filter, ContainerFilter::Stash, "Stash");
                for (id, name) in &container_list {
                    ui.selectable_value(
                        &mut db.container_filter,
                        ContainerFilter::Container(id.clone()),
                        name,
                    );
                }
            })
            .response
            .on_hover_text(
                "Show only items held in the chosen container. \"Stash\" is the \
                 primary container; the rest are your secondary containers from \
                 the Containers tab.",
            );
        ui.add_space(12.0);
        ui.checkbox(&mut db.tracked_only, "Tracked only")
            .on_hover_text(
                "Show only items required by an upgrade you're \
                 currently tracking.",
            );
    });
    ui.horizontal(|ui| {
        ui.label("Surplus vs:");
        ui.selectable_value(&mut db.horizon, NeedHorizon::TrackedOnly, "Tracked")
            .on_hover_text(
                "Needed = upgrades you're tracking. Most aggressive — \
                 everything else counts as redundant.",
            );
        ui.selectable_value(&mut db.horizon, NeedHorizon::AllNatural, "Natural")
            .on_hover_text(
                "Needed = the next buildable level of every module. \
                 Balanced default.",
            );
        ui.selectable_value(&mut db.horizon, NeedHorizon::AllFuture, "All future")
            .on_hover_text(
                "Needed = every remaining level of every module. \
                 Most conservative.",
            );
        ui.add_space(12.0);
        ui.checkbox(&mut db.redundant_only, "Redundant only")
            .on_hover_text(
                "Show only items you hold more of than the selected scope \
                 needs (surplus > 0) — safe to spend, turn in, or sell.",
            );
    });
    ui.separator();

    // Per-item "needed" totals for the chosen horizon. Read after the controls
    // so a horizon change this frame takes effect immediately.
    let needs: HashMap<String, u32> = state.read().needed_by_id(db.horizon);

    let mut rows = visible_rows(
        &data.items,
        db,
        &owned,
        &stash,
        &containers_by_item,
        &needs,
        &tracked_ids,
    );

    sort_rows(
        &mut rows,
        db.sort_by,
        db.sort_dir,
        &owned,
        &stash,
        &needs,
        &containers_by_item,
    );

    // Footer summary. Computed *after* filtering so the tracked-only toggle +
    // search box scope the totals to whatever subset is visible. The headline
    // KPIs describe what you OWN (qty > 0) — the row count next to weight/value
    // would otherwise read as "you own 150 items" when it's really the catalog
    // size, so owned-count and the catalog row count are shown separately.
    let (visible_total_weight, visible_total_value) = visible_totals(&rows, &owned);
    let owned_count = rows
        .iter()
        .filter(|item| owned.get(&item.id).copied().unwrap_or(0) > 0)
        .count();
    let (surplus_count, surplus_weight, surplus_value) = surplus_totals(&rows, &owned, &needs);
    ui.horizontal(|ui| {
        // Prominent owned-inventory KPI line (not dimmed/small).
        ui.label(
            egui::RichText::new(format!(
                "Owned: {owned_count} item(s)   ·   {visible_total_weight:.2} kg   ·   {} RUB",
                format_price(visible_total_value),
            ))
            .strong()
            .size(15.0),
        )
        .on_hover_text(
            "Items you hold (qty > 0) across the stash and every container, \
             with their combined weight and value.",
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(format!("({} shown)", rows.len()))
                .small()
                .color(ui.visuals().weak_text_color()),
        )
        .on_hover_text("Catalog rows currently visible after filters.");
    });
    ui.label(
        egui::RichText::new(format!(
            "Surplus: {surplus_count} item(s)  ·  {surplus_weight:.2} kg  ·  {} RUB",
            format_price(surplus_value),
        ))
        .small()
        .color(ui.visuals().weak_text_color()),
    )
    .on_hover_text(
        "What you could clear from the visible rows without falling short \
         of the selected upgrade scope.",
    );
    ui.add_space(4.0);

    let row_h = 36.0;
    let weak = ui.visuals().weak_text_color();
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(40.0)) // icon
        .column(Column::initial(220.0).at_least(140.0).clip(true)) // name
        .column(Column::initial(130.0).at_least(80.0).clip(true)) // subcategory
        .column(Column::initial(70.0).at_least(50.0)) // unit weight
        .column(Column::initial(100.0).at_least(70.0)) // unit price
        .column(Column::initial(60.0).at_least(50.0)) // rarity
        .column(Column::initial(60.0).at_least(50.0)) // quantity
        .column(Column::initial(160.0).at_least(90.0).clip(true)) // containers
        .column(Column::initial(70.0).at_least(50.0)) // surplus
        .column(Column::initial(90.0).at_least(70.0)) // total weight
        .column(Column::remainder().at_least(90.0)) // total value
        .header(22.0, |mut header| {
            header.col(|ui| {
                ui.strong("Icon");
            });
            header.col(|ui| sort_header(ui, db, SortColumn::Name, "Name"));
            header.col(|ui| sort_header(ui, db, SortColumn::Subcategory, "Subcategory"));
            header.col(|ui| sort_header(ui, db, SortColumn::Weight, "Weight"));
            header.col(|ui| sort_header(ui, db, SortColumn::Price, "Value"));
            header.col(|ui| sort_header(ui, db, SortColumn::Rarity, "Rarity"));
            header.col(|ui| sort_header(ui, db, SortColumn::Quantity, "Qty"));
            header.col(|ui| sort_header(ui, db, SortColumn::Containers, "Containers"));
            header.col(|ui| sort_header(ui, db, SortColumn::Surplus, "Surplus"));
            header.col(|ui| sort_header(ui, db, SortColumn::TotalWeight, "Total wt"));
            header.col(|ui| sort_header(ui, db, SortColumn::TotalValue, "Total val"));
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let item = rows[row.index()];
                let qty = owned.get(&item.id).copied().unwrap_or(0);
                row.col(|ui| {
                    if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                        let size = egui::vec2(28.0, 28.0);
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size));
                    }
                });
                row.col(|ui| {
                    ui.label(&item.name)
                        .on_hover_text(format!("id: {}", item.id));
                });
                row.col(|ui| {
                    ui.label(item.subcategory.as_deref().unwrap_or("—"));
                });
                row.col(|ui| match item.weight {
                    Some(w) => {
                        ui.label(format!("{w:.2} kg"));
                    }
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| match item.price {
                    Some(p) => {
                        ui.label(format_price(p));
                    }
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| {
                    ui.label(item.rarity.as_deref().unwrap_or("—"));
                });
                row.col(|ui| {
                    // Combined owned total (stash + every container). Zero
                    // counts get dimmed so a row with qty=0 doesn't read as
                    // "I have something" — an item the user has never gathered
                    // sits at 0 across the whole inventory.
                    let text = egui::RichText::new(qty.to_string());
                    let text = if qty == 0 { text.color(weak) } else { text };
                    ui.label(text);
                });
                row.col(|ui| {
                    // Where this item lives: "Stash ×N · Container ×M · …".
                    // Items held nowhere (qty 0 across the whole inventory)
                    // show a dim "—". The hover repeats the full text for when
                    // the cell clips.
                    let label = container_cell_label(
                        stash.get(&item.id).copied().unwrap_or(0),
                        containers_by_item.get(&item.id),
                    );
                    if label.is_empty() {
                        ui.label(egui::RichText::new("—").color(weak));
                    } else {
                        ui.add(
                            egui::Label::new(label.clone()).wrap_mode(egui::TextWrapMode::Truncate),
                        )
                        .on_hover_text(label);
                    }
                });
                row.col(|ui| {
                    // Surplus = held minus what the selected horizon needs.
                    // Dim zero so only genuinely redundant rows stand out.
                    let surplus = surplus_amount(item, &owned, &needs);
                    let text = egui::RichText::new(surplus.to_string());
                    let text = if surplus == 0 { text.color(weak) } else { text };
                    ui.label(text);
                });
                row.col(|ui| match total_weight(item, qty) {
                    Some(tw) => {
                        ui.label(format!("{tw:.2} kg"));
                    }
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| match total_value(item, qty) {
                    Some(tv) => {
                        ui.label(format_price(tv));
                    }
                    None => {
                        ui.label("—");
                    }
                });
            });
        });
}

/// Apply every active filter (category, container, tracked-only, redundant-only,
/// text) to the catalog and return the surviving rows, unsorted. Extracted from
/// [`ui`] so the filter logic — in particular the category/container scoping that
/// now spans barter goods, gun parts, and medical items — is unit-testable
/// without driving the whole table. Only the real catalog families (`misc`,
/// `gunsmith`, `medical`) are ever eligible; anything else stays out until it
/// has a column story.
fn visible_rows<'a>(
    items: &'a [Item],
    db: &ItemsDbState,
    owned: &HashMap<String, u32>,
    stash: &HashMap<String, u32>,
    containers_by_item: &HashMap<String, Vec<(ContainerId, String, u32)>>,
    needs: &HashMap<String, u32>,
    tracked_ids: &HashSet<String>,
) -> Vec<&'a Item> {
    let filter_lc = db.filter.to_lowercase();
    items
        .iter()
        .filter(|item| {
            matches!(
                item.category.as_deref(),
                Some("misc") | Some("gunsmith") | Some("medical")
            )
        })
        .filter(|item| match db.category_filter.category_key() {
            None => true,
            Some(cat) => item.category.as_deref() == Some(cat),
        })
        .filter(|item| !db.tracked_only || tracked_ids.contains(&item.id))
        .filter(|item| match &db.container_filter {
            ContainerFilter::All => true,
            ContainerFilter::Stash => stash.get(&item.id).copied().unwrap_or(0) > 0,
            ContainerFilter::Container(id) => containers_by_item
                .get(&item.id)
                .is_some_and(|parts| parts.iter().any(|(cid, _, _)| cid == id)),
        })
        .filter(|item| !db.redundant_only || surplus_amount(item, owned, needs) > 0)
        .filter(|item| {
            if filter_lc.is_empty() {
                return true;
            }
            item.name.to_lowercase().contains(&filter_lc)
                || item.id.to_lowercase().contains(&filter_lc)
                || item
                    .subcategory
                    .as_deref()
                    .map(|s| s.to_lowercase().contains(&filter_lc))
                    .unwrap_or(false)
        })
        .collect()
}

fn sort_header(ui: &mut egui::Ui, db: &mut ItemsDbState, col: SortColumn, label: &str) {
    let marker = if db.sort_by == col {
        match db.sort_dir {
            SortDir::Asc => " ▲",
            SortDir::Desc => " ▼",
        }
    } else {
        ""
    };
    let resp = ui.add(
        egui::Label::new(egui::RichText::new(format!("{label}{marker}")).strong())
            .sense(egui::Sense::click()),
    );
    if resp.clicked() {
        if db.sort_by == col {
            db.sort_dir = match db.sort_dir {
                SortDir::Asc => SortDir::Desc,
                SortDir::Desc => SortDir::Asc,
            };
        } else {
            db.sort_by = col;
            // Default to descending for value/quantity columns so the
            // user's first click on "Total val" surfaces their most
            // valuable holdings at the top — that's the obviously
            // wanted ordering. Catalog-property columns (Name,
            // Subcategory, Rarity, etc.) keep the ascending default.
            db.sort_dir = match col {
                SortColumn::Quantity
                | SortColumn::Containers
                | SortColumn::Surplus
                | SortColumn::TotalWeight
                | SortColumn::TotalValue => SortDir::Desc,
                _ => SortDir::Asc,
            };
        }
    }
}

fn sort_rows(
    rows: &mut [&Item],
    col: SortColumn,
    dir: SortDir,
    collected: &HashMap<String, u32>,
    stash: &HashMap<String, u32>,
    needs: &HashMap<String, u32>,
    containers_by_item: &HashMap<String, Vec<(ContainerId, String, u32)>>,
) {
    rows.sort_by(|a, b| {
        let ord = match col {
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::Subcategory => {
                opt_str_cmp(a.subcategory.as_deref(), b.subcategory.as_deref())
            }
            SortColumn::Weight => opt_f32_cmp(a.weight, b.weight),
            SortColumn::Price => opt_u64_cmp(a.price, b.price),
            SortColumn::Rarity => opt_rarity_cmp(a.rarity.as_deref(), b.rarity.as_deref()),
            SortColumn::Quantity => {
                let qa = collected.get(&a.id).copied().unwrap_or(0);
                let qb = collected.get(&b.id).copied().unwrap_or(0);
                qa.cmp(&qb)
            }
            SortColumn::Containers => {
                // By how many locations hold the item — the stash plus each
                // secondary container — most-spread-out first (on the
                // descending default click). Counting the stash keeps this in
                // step with the "Stash ×N" segment now shown in the cell.
                let na = location_count(
                    stash.get(&a.id).copied().unwrap_or(0),
                    containers_by_item.get(&a.id),
                );
                let nb = location_count(
                    stash.get(&b.id).copied().unwrap_or(0),
                    containers_by_item.get(&b.id),
                );
                na.cmp(&nb)
            }
            SortColumn::Surplus => {
                surplus_amount(a, collected, needs).cmp(&surplus_amount(b, collected, needs))
            }
            SortColumn::TotalWeight => {
                let qa = collected.get(&a.id).copied().unwrap_or(0);
                let qb = collected.get(&b.id).copied().unwrap_or(0);
                opt_f32_cmp(total_weight(a, qa), total_weight(b, qb))
            }
            SortColumn::TotalValue => {
                let qa = collected.get(&a.id).copied().unwrap_or(0);
                let qb = collected.get(&b.id).copied().unwrap_or(0);
                opt_u64_cmp(total_value(a, qa), total_value(b, qb))
            }
        };
        match dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    });
}

/// `qty * unit_weight` when the unit weight is known; `None` otherwise.
/// Returning `None` (rather than 0.0) for missing weights keeps the
/// "—" placeholder consistent with the unit-weight column and stops
/// missing-weight items from being treated as 0 kg in totals.
fn total_weight(item: &Item, qty: u32) -> Option<f32> {
    item.weight.map(|w| w * qty as f32)
}

fn total_value(item: &Item, qty: u32) -> Option<u64> {
    item.price.map(|p| p * qty as u64)
}

/// Sum of `total_weight` / `total_value` across the visible rows.
/// Items with missing weight/price are silently skipped — the footer
/// summary is a best-effort "what's the stash worth?" estimate, not a
/// strict total. (If those items had a known price we'd include them;
/// we don't punish the user by zeroing the total when the dataset is
/// incomplete.)
fn visible_totals(rows: &[&Item], collected: &HashMap<String, u32>) -> (f32, u64) {
    let mut weight = 0.0f32;
    let mut value = 0u64;
    for item in rows {
        let qty = collected.get(&item.id).copied().unwrap_or(0);
        if qty == 0 {
            continue;
        }
        if let Some(w) = total_weight(item, qty) {
            weight += w;
        }
        if let Some(v) = total_value(item, qty) {
            value = value.saturating_add(v);
        }
    }
    (weight, value)
}

/// Held quantity minus what the selected need-horizon requires, clamped at 0
/// — the "safe to spend / redundant" amount for one item.
fn surplus_amount(
    item: &Item,
    collected: &HashMap<String, u32>,
    needs: &HashMap<String, u32>,
) -> u32 {
    let have = collected.get(&item.id).copied().unwrap_or(0);
    let need = needs.get(&item.id).copied().unwrap_or(0);
    have.saturating_sub(need)
}

/// The Containers cell text: every location holding the item, stash first, as
/// "Stash ×qty · Name ×qty · …". The stash leads because it's the primary
/// container (and the target of the quick +/- edits). Empty only when the item
/// is held nowhere (qty 0 across the whole inventory) — those rows render a dim
/// "—" instead, so the column draws attention only when there's something to
/// show.
fn container_cell_label(stash: u32, parts: Option<&Vec<(ContainerId, String, u32)>>) -> String {
    let mut segs: Vec<String> = Vec::new();
    if stash > 0 {
        segs.push(format!("Stash ×{stash}"));
    }
    if let Some(parts) = parts {
        for (_id, name, qty) in parts {
            segs.push(format!("{name} ×{qty}"));
        }
    }
    segs.join(" · ")
}

/// How many distinct locations hold the item — the stash (when non-zero) plus
/// each secondary container. Backs the "Containers" column sort so it matches
/// the segments shown by [`container_cell_label`].
fn location_count(stash: u32, parts: Option<&Vec<(ContainerId, String, u32)>>) -> usize {
    usize::from(stash > 0) + parts.map_or(0, |v| v.len())
}

/// Count / weight / value reclaimable by clearing the surplus across the
/// visible rows. Weight and value use the *surplus* quantity (not full
/// holdings), so the readout answers "what does clearing the redundant stock
/// free up?". Items with unknown weight/price are skipped, matching
/// [`visible_totals`].
fn surplus_totals(
    rows: &[&Item],
    collected: &HashMap<String, u32>,
    needs: &HashMap<String, u32>,
) -> (u32, f32, u64) {
    let mut count = 0u32;
    let mut weight = 0.0f32;
    let mut value = 0u64;
    for item in rows {
        let s = surplus_amount(item, collected, needs);
        if s == 0 {
            continue;
        }
        count = count.saturating_add(s);
        if let Some(w) = item.weight {
            weight += w * s as f32;
        }
        if let Some(p) = item.price {
            value = value.saturating_add(p * s as u64);
        }
    }
    (count, weight, value)
}

/// Keep `None` consistently at the bottom regardless of direction.
fn opt_str_cmp(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn opt_f32_cmp(a: Option<f32>, b: Option<f32>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn opt_u64_cmp(a: Option<u64>, b: Option<u64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Order rarity by the upstream tier scale (Common → Ultimate) so sorting by
/// "Rarity" isn't just alphabetical. Unknown strings sort after Ultimate.
/// "Unusual" is the game's own word for the tier the upstream catalog calls
/// "Uncommon" (gunsmith items are sourced from in-game panes, not upstream).
fn rarity_rank(r: &str) -> u8 {
    match r {
        "Common" => 0,
        "Uncommon" | "Unusual" => 1,
        "Rare" => 2,
        "Epic" => 3,
        "Legendary" => 4,
        "Ultimate" => 5,
        _ => 6,
    }
}

fn opt_rarity_cmp(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => rarity_rank(x).cmp(&rarity_rank(y)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub(crate) fn format_price(p: u64) -> String {
    // Thousands separator with spaces; matches the upstream site's style for
    // rouble values without dragging in a localization crate.
    let s = p.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(items: &[(&str, &str, u32)]) -> Vec<(ContainerId, String, u32)> {
        items
            .iter()
            .map(|(id, name, qty)| (id.to_string(), name.to_string(), *qty))
            .collect()
    }

    #[test]
    fn cell_label_empty_when_held_nowhere() {
        // qty 0 across the whole inventory → no segments → the cell renders a
        // dim "—" (the empty-string branch in the body).
        assert_eq!(container_cell_label(0, None), "");
        assert_eq!(container_cell_label(0, Some(&parts(&[]))), "");
    }

    #[test]
    fn cell_label_lists_stash_first_then_containers() {
        let p = parts(&[("ctr-1", "Big", 4), ("ctr-2", "small2", 3)]);
        assert_eq!(
            container_cell_label(1, Some(&p)),
            "Stash ×1 · Big ×4 · small2 ×3"
        );
        // Single-location cases render just their one segment.
        assert_eq!(container_cell_label(3, None), "Stash ×3");
        assert_eq!(
            container_cell_label(0, Some(&parts(&[("ctr-1", "Big", 4)]))),
            "Big ×4"
        );
    }

    #[test]
    fn location_count_counts_stash_as_one_location() {
        assert_eq!(location_count(0, None), 0);
        assert_eq!(location_count(2, None), 1);
        let p = parts(&[("ctr-1", "Big", 4), ("ctr-2", "small2", 3)]);
        assert_eq!(location_count(0, Some(&p)), 2);
        assert_eq!(location_count(5, Some(&p)), 3);
    }

    fn mk_item(id: &str, name: &str, category: &str) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            icon_path: String::new(),
            category: Some(category.into()),
            subcategory: None,
            weight: Some(1.0),
            price: None,
            rarity: None,
            scan_alias: None,
        }
    }

    #[test]
    fn db_lists_gun_parts_and_filters_scope_them() {
        let items = vec![
            mk_item("misc_bolts", "Bolts", "misc"),
            mk_item("gunsmith_barrel", "Heavy Barrel", "gunsmith"),
            // A category the DB doesn't model — must never appear, even on "All".
            mk_item("weapon_ak", "Some Gun", "weapons"),
        ];
        let owned = HashMap::new();
        let stash = HashMap::new();
        let mut containers_by_item: HashMap<String, Vec<(ContainerId, String, u32)>> =
            HashMap::new();
        containers_by_item.insert(
            "gunsmith_barrel".into(),
            parts(&[("gunsmith-storage", "Gunsmith storage", 2)]),
        );
        let needs = HashMap::new();
        let tracked = HashSet::new();

        let ids = |db: &ItemsDbState| -> Vec<String> {
            visible_rows(
                &items,
                db,
                &owned,
                &stash,
                &containers_by_item,
                &needs,
                &tracked,
            )
            .iter()
            .map(|i| i.id.clone())
            .collect()
        };

        // "All" shows both real families and excludes anything else.
        let mut db = ItemsDbState::default();
        let all = ids(&db);
        assert!(all.contains(&"misc_bolts".to_string()));
        assert!(all.contains(&"gunsmith_barrel".to_string()));
        assert!(!all.contains(&"weapon_ak".to_string()));

        // The Category picker narrows to one family.
        db.category_filter = CategoryFilter::GunParts;
        assert_eq!(ids(&db), vec!["gunsmith_barrel".to_string()]);
        db.category_filter = CategoryFilter::Barter;
        assert_eq!(ids(&db), vec!["misc_bolts".to_string()]);

        // Scoping to the gunsmith storage now keeps its gun part — impossible
        // while the DB was misc-only.
        db.category_filter = CategoryFilter::All;
        db.container_filter = ContainerFilter::Container("gunsmith-storage".into());
        assert_eq!(ids(&db), vec!["gunsmith_barrel".to_string()]);
    }
}
