//! Loaded, immutable game data. Deserialized once at startup from the embedded
//! `data.json` produced by the scraper.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type UpgradeId = String;
pub type TaskId = String;
pub type ItemId = String;
pub type ModuleId = String;
pub type VendorId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameData {
    pub data_version: String,
    pub scraped_at: String,
    pub source_repo: String,
    pub source_commit: String,
    pub modules: Vec<HideoutModule>,
    pub vendors: Vec<Vendor>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HideoutModule {
    pub id: ModuleId,
    pub name: String,
    pub upgrades: Vec<Upgrade>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upgrade {
    pub id: UpgradeId,
    pub name: String,
    pub level: u32,
    #[serde(default)]
    pub description: String,
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vendor {
    pub id: VendorId,
    pub name: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub vendor_id: VendorId,
    pub prerequisites: Vec<TaskId>,
    pub requirements: Vec<Requirement>,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub item_id: ItemId,
    pub quantity: u32,
}

/// The game caps every hideout recipe at four item slots. We carry the full
/// 4-slot shape (with empties as `None`) in overrides so slot indices stay
/// stable as the user clears or repopulates positions.
pub const RECIPE_SLOTS: usize = 4;

/// A user-supplied correction for a single upgrade's recipe. Replaces the
/// official recipe in its entirety once present — slot-level merging would be
/// surprising when upstream data shifts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeOverride {
    pub slots: [Option<Requirement>; RECIPE_SLOTS],
}

impl RecipeOverride {
    /// True iff every slot matches the bundled recipe — useful for collapsing
    /// "user edited then put it all back" into a clean unoverridden state.
    pub fn matches_base(&self, base: &Upgrade) -> bool {
        let base_slots = base_slots(base);
        slots_equal(&self.slots, &base_slots)
    }
}

/// Lift a recipe out of its `Vec<Requirement>` shape into a fixed 4-slot view,
/// trailing entries padded with `None`.
pub fn base_slots(upgrade: &Upgrade) -> [Option<Requirement>; RECIPE_SLOTS] {
    let mut out: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
    for (i, req) in upgrade.requirements.iter().take(RECIPE_SLOTS).enumerate() {
        out[i] = Some(req.clone());
    }
    out
}

fn slots_equal(
    a: &[Option<Requirement>; RECIPE_SLOTS],
    b: &[Option<Requirement>; RECIPE_SLOTS],
) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.item_id == y.item_id && x.quantity == y.quantity,
        _ => false,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub name: String,
    pub icon_path: String,
}

/// Lookup tables built once from a `GameData`.
pub struct DataIndex {
    pub items_by_id: HashMap<ItemId, Item>,
    pub upgrades_by_id: HashMap<UpgradeId, UpgradeRef>,
    pub tasks_by_id: HashMap<TaskId, TaskRef>,
}

#[derive(Clone)]
pub struct UpgradeRef {
    #[allow(dead_code)] // Used by the future VR overlay renderer.
    pub module_id: ModuleId,
    pub module_name: String,
    pub upgrade: Upgrade,
}

#[derive(Clone)]
pub struct TaskRef {
    #[allow(dead_code)] // Used by the future VR overlay renderer.
    pub vendor_id: VendorId,
    pub vendor_name: String,
    pub task: Task,
}

impl DataIndex {
    pub fn build(data: &GameData) -> Self {
        let items_by_id = data
            .items
            .iter()
            .map(|i| (i.id.clone(), i.clone()))
            .collect();

        let mut upgrades_by_id = HashMap::new();
        for module in &data.modules {
            for upgrade in &module.upgrades {
                upgrades_by_id.insert(
                    upgrade.id.clone(),
                    UpgradeRef {
                        module_id: module.id.clone(),
                        module_name: module.name.clone(),
                        upgrade: upgrade.clone(),
                    },
                );
            }
        }

        let mut tasks_by_id = HashMap::new();
        for vendor in &data.vendors {
            for task in &vendor.tasks {
                tasks_by_id.insert(
                    task.id.clone(),
                    TaskRef {
                        vendor_id: vendor.id.clone(),
                        vendor_name: vendor.name.clone(),
                        task: task.clone(),
                    },
                );
            }
        }

        Self {
            items_by_id,
            upgrades_by_id,
            tasks_by_id,
        }
    }
}
