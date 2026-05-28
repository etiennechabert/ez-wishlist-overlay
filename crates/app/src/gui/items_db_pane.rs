//! Left tab: MISC item database, with name filter and click-to-sort columns.
//! Read-only reference view — no wishlist mutation here. Scoped to MISC for
//! now; a category selector can be re-added once we expand beyond that.
//!
//! Quantity / total weight / total value columns reflect the
//! cross-cutting `AppState.collected` map (the same map OCR captures
//! and the wishlist UI mutates). Sorting by Total Value makes this
//! tab a "what's in my stash worth" view.

use crate::data::Item;
use crate::gui::IconCache;
use crate::state::AppState;
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
    TotalWeight,
    TotalValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

pub struct ItemsDbState {
    pub filter: String,
    pub sort_by: SortColumn,
    pub sort_dir: SortDir,
    /// When true, hide rows whose item id isn't currently required by
    /// any tracked upgrade or task (i.e. anything not in
    /// `AppState::active_items()`). Useful when the user wants to see
    /// only what's relevant to their current goals.
    pub tracked_only: bool,
}

impl Default for ItemsDbState {
    fn default() -> Self {
        Self {
            filter: String::new(),
            sort_by: SortColumn::Name,
            sort_dir: SortDir::Asc,
            tracked_only: false,
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
    let (data, collected, tracked_ids) = {
        let s = state.read();
        let collected: HashMap<String, u32> = s.collected.clone();
        let tracked_ids: HashSet<String> =
            s.active_items().into_iter().map(|a| a.item_id).collect();
        (s.data.clone(), collected, tracked_ids)
    };

    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.text_edit_singleline(&mut db.filter);
        if ui.button("✕").clicked() {
            db.filter.clear();
        }
        ui.add_space(12.0);
        ui.checkbox(&mut db.tracked_only, "Tracked only")
            .on_hover_text(
                "Show only items required by an upgrade or task you're \
                 currently tracking.",
            );
    });
    ui.separator();

    let filter_lc = db.filter.to_lowercase();
    let mut rows: Vec<&Item> = data
        .items
        .iter()
        .filter(|item| item.category.as_deref() == Some("misc"))
        .filter(|item| !db.tracked_only || tracked_ids.contains(&item.id))
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
        .collect();

    sort_rows(&mut rows, db.sort_by, db.sort_dir, &collected);

    // Footer summary: count + grand totals across the visible rows.
    // Computed *after* filtering so the user can use the tracked-only
    // toggle + the search box to scope the "stash value" question to
    // whatever subset they're looking at.
    let (visible_total_weight, visible_total_value) = visible_totals(&rows, &collected);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} item(s)", rows.len()))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(format!(
                "Total weight: {:.2} kg   ·   Total value: {} ₽",
                visible_total_weight,
                format_price(visible_total_value),
            ))
            .small()
            .color(ui.visuals().weak_text_color()),
        );
    });
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
            header.col(|ui| sort_header(ui, db, SortColumn::TotalWeight, "Total wt"));
            header.col(|ui| sort_header(ui, db, SortColumn::TotalValue, "Total val"));
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let item = rows[row.index()];
                let qty = collected.get(&item.id).copied().unwrap_or(0);
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
                    // Zero counts get dimmed so a stash row with
                    // qty=0 doesn't read as "I have something"
                    // — the same item also has a 0 in `collected`
                    // when OCR hasn't seen it yet.
                    let text = egui::RichText::new(qty.to_string());
                    let text = if qty == 0 { text.color(weak) } else { text };
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
                SortColumn::Quantity | SortColumn::TotalWeight | SortColumn::TotalValue => {
                    SortDir::Desc
                }
                _ => SortDir::Asc,
            };
        }
    }
}

fn sort_rows(rows: &mut [&Item], col: SortColumn, dir: SortDir, collected: &HashMap<String, u32>) {
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
fn rarity_rank(r: &str) -> u8 {
    match r {
        "Common" => 0,
        "Uncommon" => 1,
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

fn format_price(p: u64) -> String {
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
