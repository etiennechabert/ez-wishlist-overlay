//! Parse `src/data/hideout-upgrades.ts` into `(HideoutModule, Upgrade)` records.
//!
//! Upstream shape: `export const hideoutUpgrades = { "<AreaId>Lv<level>": {...} }`
//! Fields we read: `areaId`, `level`, `upgradeName`, `upgradeDesc`, `exchange`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::model::{HideoutModule, ModuleId, Requirement, Upgrade, UpgradeId};
use crate::parse_ts;

#[derive(Debug, Deserialize)]
struct UpstreamUpgrade {
    #[serde(rename = "areaId")]
    area_id: String,
    level: u32,
    #[serde(rename = "upgradeName")]
    upgrade_name: String,
    #[serde(default)]
    exchange: BTreeMap<String, u32>,
}

pub struct HideoutResult {
    pub modules: Vec<HideoutModule>,
    /// All item IDs referenced by any upgrade's exchange map.
    pub referenced_items: std::collections::HashSet<String>,
}

pub fn parse(hideout_ts_path: &Path) -> Result<HideoutResult> {
    let source = std::fs::read_to_string(hideout_ts_path)
        .with_context(|| format!("reading {}", hideout_ts_path.display()))?;
    let raw = parse_ts::extract_object(&source, "hideoutUpgrades")
        .context("extract hideoutUpgrades literal")?;

    let map: BTreeMap<String, UpstreamUpgrade> =
        serde_json::from_value(raw).context("deserialize hideoutUpgrades")?;

    let mut by_module: BTreeMap<ModuleId, Vec<Upgrade>> = BTreeMap::new();
    let mut referenced_items = std::collections::HashSet::new();
    let mut module_names: BTreeMap<ModuleId, String> = BTreeMap::new();

    for (key, u) in map {
        let upgrade_id: UpgradeId = key;
        let module_id = u.area_id.clone();
        // Prefer the human name from the first upgrade we see for this area.
        module_names
            .entry(module_id.clone())
            .or_insert_with(|| u.upgrade_name.clone());

        let requirements: Vec<Requirement> = u
            .exchange
            .into_iter()
            .map(|(item_id, quantity)| {
                referenced_items.insert(item_id.clone());
                Requirement { item_id, quantity }
            })
            .collect();

        by_module.entry(module_id).or_default().push(Upgrade {
            id: upgrade_id,
            name: u.upgrade_name,
            level: u.level,
            requirements,
        });
    }

    let mut modules: Vec<HideoutModule> = by_module
        .into_iter()
        .map(|(id, mut upgrades)| {
            upgrades.sort_by_key(|up| up.level);
            let name = module_names.get(&id).cloned().unwrap_or_else(|| id.clone());
            HideoutModule { id, name, upgrades }
        })
        .collect();
    modules.sort_by(|a, b| a.id.cmp(&b.id));

    tracing::info!(
        modules = modules.len(),
        upgrades = modules.iter().map(|m| m.upgrades.len()).sum::<usize>(),
        items = referenced_items.len(),
        "parsed hideout upgrades",
    );

    Ok(HideoutResult {
        modules,
        referenced_items,
    })
}
