//! Left tab: MISC item database, with name filter and click-to-sort columns.
//! Read-only reference view — no wishlist mutation here. Scoped to MISC for
//! now; a category selector can be re-added once we expand beyond that.
//!
//! Quantity / surplus / total weight / total value columns reflect the
//! *combined* owned total across the stash (`AppState.collected`, the map OCR
//! captures and the wishlist UI mutates) and every secondary container
//! (`AppState.containers`) — see [`crate::state::AppState::owned_total`]. The
//! Containers column attributes that total to where it lives. Sorting by Total
//! Value makes this tab a "what's my whole inventory worth" view.

use crate::data::Item;
use crate::gui::IconCache;
use crate::state::{AppState, NeedHorizon};
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
    let (data, owned, stash, containers_by_item, tracked_ids) = {
        let s = state.read();
        // Stash counts on their own — used only to attribute the stash portion
        // in the per-item location tooltip.
        let stash: HashMap<String, u32> = s.collected.clone();
        // Combined owned total (stash + every container). Every quantity-driven
        // column below — Qty, Surplus, Total wt/val — reads this, so they all
        // count the whole inventory.
        let mut owned = stash.clone();
        // Reverse index: item id → which secondary containers hold it, and how
        // many. Built once here so the virtualized body never scans containers.
        let mut containers_by_item: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for c in &s.containers {
            for (item_id, &qty) in &c.contents {
                *owned.entry(item_id.clone()).or_insert(0) += qty;
                containers_by_item
                    .entry(item_id.clone())
                    .or_default()
                    .push((c.name.clone(), qty));
            }
        }
        // Stable display order within a cell, independent of HashMap iteration.
        for parts in containers_by_item.values_mut() {
            parts.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let tracked_ids: HashSet<String> =
            s.active_items().into_iter().map(|a| a.item_id).collect();
        (
            s.data.clone(),
            owned,
            stash,
            containers_by_item,
            tracked_ids,
        )
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

    let filter_lc = db.filter.to_lowercase();
    let mut rows: Vec<&Item> = data
        .items
        .iter()
        .filter(|item| item.category.as_deref() == Some("misc"))
        .filter(|item| !db.tracked_only || tracked_ids.contains(&item.id))
        .filter(|item| !db.redundant_only || surplus_amount(item, &owned, &needs) > 0)
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

    sort_rows(
        &mut rows,
        db.sort_by,
        db.sort_dir,
        &owned,
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
                    // Which secondary containers hold this item (the new
                    // info). Stash-only items show a dim "—"; the full
                    // breakdown incl. the stash portion is in the hover.
                    let label = container_cell_label(containers_by_item.get(&item.id));
                    if label.is_empty() {
                        ui.label(egui::RichText::new("—").color(weak));
                    } else {
                        ui.add(egui::Label::new(label).wrap_mode(egui::TextWrapMode::Truncate))
                            .on_hover_text(container_tooltip(
                                stash.get(&item.id).copied().unwrap_or(0),
                                containers_by_item.get(&item.id),
                            ));
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
    needs: &HashMap<String, u32>,
    containers_by_item: &HashMap<String, Vec<(String, u32)>>,
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
                // By how many distinct secondary containers hold the item —
                // most-spread-out first (on the descending default click).
                let na = containers_by_item.get(&a.id).map_or(0, |v| v.len());
                let nb = containers_by_item.get(&b.id).map_or(0, |v| v.len());
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

/// The Containers cell text: secondary-container holdings as
/// "Name ×qty · Name ×qty". Empty when the item lives only in the stash (or
/// nowhere) — those rows render a dim "—" instead, so the column only draws
/// attention when there's genuinely container info to show.
fn container_cell_label(parts: Option<&Vec<(String, u32)>>) -> String {
    match parts {
        Some(parts) if !parts.is_empty() => parts
            .iter()
            .map(|(name, qty)| format!("{name} ×{qty}"))
            .collect::<Vec<_>>()
            .join(" · "),
        _ => String::new(),
    }
}

/// Full where-is-it breakdown for the cell's hover tooltip, including the stash
/// portion so the user sees the complete split (e.g. "Stash ×2 · Backpack ×3")
/// even though the cell itself shows only the secondary containers.
fn container_tooltip(stash: u32, parts: Option<&Vec<(String, u32)>>) -> String {
    let mut segs: Vec<String> = Vec::new();
    if stash > 0 {
        segs.push(format!("Stash ×{stash}"));
    }
    if let Some(parts) = parts {
        for (name, qty) in parts {
            segs.push(format!("{name} ×{qty}"));
        }
    }
    if segs.is_empty() {
        "Not stored anywhere yet".to_string()
    } else {
        segs.join(" · ")
    }
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
