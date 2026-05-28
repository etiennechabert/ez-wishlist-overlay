//! Left tab: MISC item database, with name filter and click-to-sort columns.
//! Read-only reference view — no wishlist mutation here. Scoped to MISC for
//! now; a category selector can be re-added once we expand beyond that.

use crate::data::Item;
use crate::gui::IconCache;
use crate::state::AppState;
use egui_extras::{Column, TableBuilder};
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Subcategory,
    Weight,
    Price,
    Rarity,
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
}

impl Default for ItemsDbState {
    fn default() -> Self {
        Self {
            filter: String::new(),
            sort_by: SortColumn::Name,
            sort_dir: SortDir::Asc,
        }
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    db: &mut ItemsDbState,
) {
    let data = state.read().data.clone();

    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.text_edit_singleline(&mut db.filter);
        if ui.button("✕").clicked() {
            db.filter.clear();
        }
    });
    ui.separator();

    let filter_lc = db.filter.to_lowercase();
    let mut rows: Vec<&Item> = data
        .items
        .iter()
        .filter(|item| item.category.as_deref() == Some("misc"))
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

    sort_rows(&mut rows, db.sort_by, db.sort_dir);

    ui.label(
        egui::RichText::new(format!("{} item(s)", rows.len()))
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);

    let row_h = 36.0;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(40.0)) // icon
        .column(Column::initial(240.0).at_least(140.0).clip(true)) // name
        .column(Column::initial(150.0).at_least(80.0).clip(true)) // subcategory
        .column(Column::initial(80.0).at_least(50.0)) // weight
        .column(Column::initial(110.0).at_least(70.0)) // price
        .column(Column::remainder().at_least(70.0)) // rarity
        .header(22.0, |mut header| {
            header.col(|ui| {
                ui.strong("Icon");
            });
            header.col(|ui| sort_header(ui, db, SortColumn::Name, "Name"));
            header.col(|ui| sort_header(ui, db, SortColumn::Subcategory, "Subcategory"));
            header.col(|ui| sort_header(ui, db, SortColumn::Weight, "Weight"));
            header.col(|ui| sort_header(ui, db, SortColumn::Price, "Value"));
            header.col(|ui| sort_header(ui, db, SortColumn::Rarity, "Rarity"));
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let item = rows[row.index()];
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
            db.sort_dir = SortDir::Asc;
        }
    }
}

fn sort_rows(rows: &mut [&Item], col: SortColumn, dir: SortDir) {
    rows.sort_by(|a, b| {
        let ord = match col {
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::Subcategory => {
                opt_str_cmp(a.subcategory.as_deref(), b.subcategory.as_deref())
            }
            SortColumn::Weight => opt_f32_cmp(a.weight, b.weight),
            SortColumn::Price => opt_u64_cmp(a.price, b.price),
            SortColumn::Rarity => opt_rarity_cmp(a.rarity.as_deref(), b.rarity.as_deref()),
        };
        match dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    });
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
