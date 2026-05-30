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
    /// Hash of the bundled recipe at the moment this correction was made
    /// ([`recipe_hash`]). When a later app update changes the official recipe
    /// for this upgrade — whether filling in a previously-empty "missing"
    /// upgrade or fixing a wrong one — the stored hash no longer matches, and
    /// the correction is discarded on load so the authoritative data wins. The
    /// user was correcting a recipe we've since changed; their fix may now be
    /// stale or already incorporated. `None` for overrides written before this
    /// field existed: we can't know their original base, so we keep them.
    #[serde(default)]
    pub base_hash: Option<u64>,
}

impl RecipeOverride {
    /// Construct a correction from a 4-slot view. `base_hash` starts `None`;
    /// [`crate::state::AppState::set_recipe_override`] stamps the real hash of
    /// the bundled recipe when the correction is committed.
    pub fn new(slots: [Option<Requirement>; RECIPE_SLOTS]) -> Self {
        Self {
            slots,
            base_hash: None,
        }
    }

    /// True iff every slot matches the bundled recipe — useful for collapsing
    /// "user edited then put it all back" into a clean unoverridden state.
    pub fn matches_base(&self, base: &Upgrade) -> bool {
        let base_slots = base_slots(base);
        slots_equal(&self.slots, &base_slots)
    }
}

/// Stable hash of an upgrade's bundled recipe, used to detect when an app
/// update has changed the official recipe out from under a user's correction.
///
/// Hand-rolled FNV-1a over a canonical encoding of the 4-slot base view, *not*
/// `std::hash::DefaultHasher`: the std hasher's output isn't guaranteed stable
/// across Rust versions, and this value is persisted in `overrides.json` and
/// compared on a later run (possibly built with a newer toolchain). Encoding
/// the fixed 4-slot shape — with a per-slot present/empty tag and an explicit
/// separator between item id and quantity — keeps positional meaning and
/// avoids field-boundary collisions.
pub fn recipe_hash(upgrade: &Upgrade) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    fn mix(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h ^= b as u64;
            *h = h.wrapping_mul(FNV_PRIME);
        }
    }
    let mut h = FNV_OFFSET;
    for slot in base_slots(upgrade).iter() {
        match slot {
            None => mix(&mut h, &[0x00]),
            Some(req) => {
                mix(&mut h, &[0x01]);
                mix(&mut h, req.item_id.as_bytes());
                mix(&mut h, &[0x1f]); // unit separator: ("ab",x)+("c",y) ≠ ("a",x)+("bc",y)
                mix(&mut h, &req.quantity.to_le_bytes());
            }
        }
    }
    h
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
    /// Top-level upstream category, e.g. `"misc"`, `"weapons"`, `"medical"`.
    /// Optional so older `data.json` files (pre-DB-view) still load.
    #[serde(default)]
    pub category: Option<String>,
    /// Upstream subcategory, e.g. `"HighValue"`, `"5.45x39mm"`.
    #[serde(default)]
    pub subcategory: Option<String>,
    /// Item weight in kg. Optional — not every upstream entry carries stats.
    #[serde(default)]
    pub weight: Option<f32>,
    /// Vendor price in roubles. Optional for the same reason.
    #[serde(default)]
    pub price: Option<u64>,
    /// Upstream rarity tier, e.g. `"Rare"`, `"Ultimate"`.
    #[serde(default)]
    pub rarity: Option<String>,
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
