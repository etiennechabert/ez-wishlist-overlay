//! Parse upstream item catalogs (`public/data/<category>.json`).
//!
//! Each catalog is a JSON array of item records. We only need the fields used
//! to build our `Item` (id, name, icon path).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::model::{Item, ItemId};

/// Filenames inside `public/data/` that we try to read. Missing files are
/// non-fatal — just logged.
const CATALOGS: &[&str] = &[
    "ammunition.json",
    "armor.json",
    "attachments.json",
    "backpacks.json",
    "face-shields.json",
    "grenades.json",
    "helmets.json",
    "holsters.json",
    "keys.json",
    "magazines.json",
    "medical.json",
    "misc.json",
    "provisions.json",
    "task-items.json",
    "weapons.json",
];

#[derive(Debug, Deserialize)]
struct UpstreamItem {
    id: String,
    name: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    subcategory: Option<String>,
    #[serde(default)]
    images: Option<UpstreamItemImages>,
    #[serde(default)]
    stats: Option<UpstreamItemStats>,
}

#[derive(Debug, Deserialize)]
struct UpstreamItemImages {
    icon: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UpstreamItemStats {
    #[serde(default)]
    price: Option<u64>,
    #[serde(default)]
    weight: Option<f32>,
    #[serde(default)]
    rarity: Option<String>,
}

pub struct ItemCatalog {
    /// All items, keyed by upstream id. The id is also our `ItemId` (we
    /// preserve upstream slugs 1:1).
    pub items: HashMap<ItemId, ItemRecord>,
}

#[derive(Debug, Clone)]
pub struct ItemRecord {
    pub id: ItemId,
    pub name: String,
    /// Upstream path, e.g. `/images/items/misc/foo.webp`.
    pub upstream_icon: Option<String>,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub price: Option<u64>,
    pub weight: Option<f32>,
    pub rarity: Option<String>,
}

impl ItemCatalog {
    pub fn from_upstream(public_data: &Path) -> Result<Self> {
        let mut items = HashMap::new();
        let mut missing = Vec::new();

        for &fname in CATALOGS {
            let path = public_data.join(fname);
            if !path.exists() {
                missing.push(fname);
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let raw: Vec<UpstreamItem> = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            for u in raw {
                let icon = u.images.and_then(|i| i.icon);
                let stats = u.stats.unwrap_or_default();
                let rec = ItemRecord {
                    id: u.id.clone(),
                    name: u.name.clone(),
                    upstream_icon: icon,
                    category: u.category,
                    subcategory: u.subcategory,
                    price: stats.price,
                    weight: stats.weight,
                    rarity: stats.rarity,
                };
                items.insert(u.id, rec);
            }
        }

        if !missing.is_empty() {
            tracing::warn!("missing item catalogs: {missing:?}");
        }
        tracing::info!(count = items.len(), "loaded item catalog");

        Ok(Self { items })
    }

    /// Build the `Item` list for our `data.json`, scoped to upstream `misc`
    /// items only — that's the current scope of the in-app Items DB view.
    /// When we expand to more categories, this filter is the only thing
    /// that needs to grow.
    pub fn build_output_items_misc(&self, icon_dir_name: &str) -> Vec<Item> {
        let mut out: Vec<Item> = self
            .items
            .values()
            .filter(|rec| rec.category.as_deref() == Some("misc"))
            .map(|rec| Item {
                id: rec.id.clone(),
                name: rec.name.clone(),
                icon_path: format!("{icon_dir_name}/{}.png", rec.id),
                category: rec.category.clone(),
                subcategory: rec.subcategory.clone(),
                weight: rec.weight,
                price: rec.price,
                rarity: rec.rarity.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}
