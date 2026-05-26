//! Application state — what the user has enabled, completed, and collected.
//!
//! `AppState` lives behind an `Arc<RwLock<_>>`. Both the GUI thread and the
//! (future) VR thread mutate it; both hold the write lock only for the
//! duration of the mutation.

use crate::data::{
    base_slots, DataIndex, GameData, ItemId, RecipeOverride, Requirement, TaskId, UpgradeId,
    RECIPE_SLOTS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const OVERRIDES_SCHEMA_VERSION: u32 = 1;

pub struct AppState {
    pub data: Arc<GameData>,
    pub index: Arc<DataIndex>,
    pub tracked_upgrades: HashSet<UpgradeId>,
    pub completed_upgrades: HashSet<UpgradeId>,
    pub tracked_tasks: HashSet<TaskId>,
    pub completed_tasks: HashSet<TaskId>,
    pub collected: HashMap<ItemId, u32>,
    /// User-supplied recipe corrections, keyed by upgrade id. Present entries
    /// fully replace the bundled recipe; absence falls through to `data.json`.
    pub overrides: HashMap<UpgradeId, RecipeOverride>,
    /// Bumped on every mutation. The VR thread reads this to decide whether
    /// to re-render the overlay texture.
    pub version: u64,
    /// Warning to surface in the GUI when load found a problem (corrupt
    /// state, schema mismatch, orphaned IDs after a data-version bump).
    pub load_warning: Option<String>,
}

impl AppState {
    pub fn new(data: Arc<GameData>) -> Self {
        let index = Arc::new(DataIndex::build(&data));
        Self {
            data,
            index,
            tracked_upgrades: HashSet::new(),
            completed_upgrades: HashSet::new(),
            tracked_tasks: HashSet::new(),
            completed_tasks: HashSet::new(),
            collected: HashMap::new(),
            overrides: HashMap::new(),
            version: 0,
            load_warning: None,
        }
    }

    /// 4-slot view of a recipe with overrides applied. Slots not yet populated
    /// (either by base recipe or override) come back as `None`. Empties stay
    /// in position so the edit panel doesn't shuffle when a slot is cleared.
    pub fn effective_slots(&self, upgrade_id: &UpgradeId) -> [Option<Requirement>; RECIPE_SLOTS] {
        if let Some(o) = self.overrides.get(upgrade_id) {
            return o.slots.clone();
        }
        if let Some(uref) = self.index.upgrades_by_id.get(upgrade_id) {
            return base_slots(&uref.upgrade);
        }
        std::array::from_fn(|_| None)
    }

    /// Flat list of the recipe's non-empty slots, post-override. This is the
    /// shape the preview pane and overlay aggregations consume.
    pub fn effective_requirements(&self, upgrade_id: &UpgradeId) -> Vec<Requirement> {
        self.effective_slots(upgrade_id)
            .into_iter()
            .flatten()
            .collect()
    }

    /// Persist a 4-slot override. If the slots match the bundled recipe we
    /// drop the entry instead — keeps `overrides.json` minimal and the
    /// "modified" indicator honest.
    pub fn set_recipe_override(&mut self, upgrade_id: &UpgradeId, new_override: RecipeOverride) {
        if let Some(uref) = self.index.upgrades_by_id.get(upgrade_id) {
            if new_override.matches_base(&uref.upgrade) {
                self.overrides.remove(upgrade_id);
                self.bump();
                return;
            }
        }
        self.overrides.insert(upgrade_id.clone(), new_override);
        self.bump();
    }

    pub fn clear_recipe_override(&mut self, upgrade_id: &UpgradeId) {
        if self.overrides.remove(upgrade_id).is_some() {
            self.bump();
        }
    }

    pub fn is_overridden(&self, upgrade_id: &UpgradeId) -> bool {
        self.overrides.contains_key(upgrade_id)
    }

    pub fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn set_tracked_upgrade(&mut self, id: &UpgradeId, on: bool) {
        if on {
            self.tracked_upgrades.insert(id.clone());
            self.completed_upgrades.remove(id);
        } else {
            self.tracked_upgrades.remove(id);
        }
        self.bump();
    }

    pub fn set_completed_upgrade(&mut self, id: &UpgradeId, on: bool) {
        if on {
            self.completed_upgrades.insert(id.clone());
            self.tracked_upgrades.remove(id);
        } else {
            self.completed_upgrades.remove(id);
        }
        self.bump();
    }

    pub fn set_tracked_task(&mut self, id: &TaskId, on: bool) {
        if on {
            self.tracked_tasks.insert(id.clone());
            self.completed_tasks.remove(id);
        } else {
            self.tracked_tasks.remove(id);
        }
        self.bump();
    }

    pub fn set_completed_task(&mut self, id: &TaskId, on: bool) {
        if on {
            self.completed_tasks.insert(id.clone());
            self.tracked_tasks.remove(id);
        } else {
            self.completed_tasks.remove(id);
        }
        self.bump();
    }

    pub fn set_collected(&mut self, item: &ItemId, value: u32) {
        if value == 0 {
            self.collected.remove(item);
        } else {
            self.collected.insert(item.clone(), value);
        }
        self.bump();
    }

    pub fn adjust_collected(&mut self, item: &ItemId, delta: i64) {
        let cur = *self.collected.get(item).unwrap_or(&0) as i64;
        let next = (cur + delta).max(0) as u32;
        self.set_collected(item, next);
    }

    /// VR-click cycle: increment, or reset to 0 when at/above the target.
    /// Returns the new value and whether this was a reset.
    #[allow(dead_code)] // Used by the future VR input handler.
    pub fn cycle_collected(&mut self, item: &ItemId, target: u32) -> (u32, bool) {
        let cur = *self.collected.get(item).unwrap_or(&0);
        if target == 0 {
            // No goal; still let user count up.
            self.set_collected(item, cur + 1);
            return (cur + 1, false);
        }
        if cur >= target {
            self.set_collected(item, 0);
            (0, true)
        } else {
            self.set_collected(item, cur + 1);
            (cur + 1, false)
        }
    }

    pub fn reset_all(&mut self) {
        self.tracked_upgrades.clear();
        self.completed_upgrades.clear();
        self.tracked_tasks.clear();
        self.completed_tasks.clear();
        self.collected.clear();
        self.bump();
    }

    /// Aggregate every tracked-but-not-completed upgrade and task into a
    /// flat per-item view.
    pub fn active_items(&self) -> Vec<ActiveItem> {
        let mut totals: BTreeMap<ItemId, (u32, Vec<String>)> = BTreeMap::new();

        for id in self.tracked_upgrades.difference(&self.completed_upgrades) {
            let Some(uref) = self.index.upgrades_by_id.get(id) else {
                continue;
            };
            let label = if uref.upgrade.name == uref.module_name {
                format!("{} L{}", uref.module_name, uref.upgrade.level)
            } else {
                format!(
                    "{} L{} ({})",
                    uref.module_name, uref.upgrade.level, uref.upgrade.name
                )
            };
            for req in self.effective_requirements(id) {
                let entry = totals
                    .entry(req.item_id.clone())
                    .or_insert_with(|| (0, Vec::new()));
                entry.0 += req.quantity;
                entry.1.push(label.clone());
            }
        }

        for id in self.tracked_tasks.difference(&self.completed_tasks) {
            let Some(tref) = self.index.tasks_by_id.get(id) else {
                continue;
            };
            let label = format!("Task: {} ({})", tref.task.name, tref.vendor_name);
            for req in &tref.task.requirements {
                let entry = totals
                    .entry(req.item_id.clone())
                    .or_insert_with(|| (0, Vec::new()));
                entry.0 += req.quantity;
                entry.1.push(label.clone());
            }
        }

        let mut out: Vec<ActiveItem> = totals
            .into_iter()
            .filter_map(|(item_id, (needed, sources))| {
                let item = self.index.items_by_id.get(&item_id)?;
                let collected = *self.collected.get(&item_id).unwrap_or(&0);
                Some(ActiveItem {
                    item_id,
                    name: item.name.clone(),
                    icon_path: item.icon_path.clone(),
                    needed,
                    collected,
                    sources,
                })
            })
            .collect();

        // Order:
        //   1. Active items first; completed items last so the user's eye
        //      lands on stuff they still need to gather.
        //   2. Within the active group, sort by descending REMAINING
        //      (`needed - collected`) so the most outstanding work ends up
        //      top-left of the overlay grid, items close to completion drift
        //      toward bottom-right.
        //   3. Tiebreak alphabetically by name for a stable view across
        //      ticks where two items have identical remaining counts.
        out.sort_by(|a, b| {
            let a_done = a.collected >= a.needed;
            let b_done = b.collected >= b.needed;
            let a_remaining = a.needed.saturating_sub(a.collected);
            let b_remaining = b.needed.saturating_sub(b.collected);
            a_done
                .cmp(&b_done)
                .then_with(|| b_remaining.cmp(&a_remaining)) // descending
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }
}

#[derive(Debug, Clone)]
pub struct ActiveItem {
    pub item_id: ItemId,
    pub name: String,
    pub icon_path: String,
    pub needed: u32,
    pub collected: u32,
    pub sources: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PersistedState {
    pub schema_version: u32,
    pub data_version: String,
    #[serde(default)]
    pub tracked_upgrades: HashSet<UpgradeId>,
    #[serde(default)]
    pub completed_upgrades: HashSet<UpgradeId>,
    #[serde(default)]
    pub tracked_tasks: HashSet<TaskId>,
    #[serde(default)]
    pub completed_tasks: HashSet<TaskId>,
    #[serde(default)]
    pub collected: HashMap<ItemId, u32>,
}

impl PersistedState {
    pub fn from_app(state: &AppState) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            data_version: state.data.data_version.clone(),
            tracked_upgrades: state.tracked_upgrades.clone(),
            completed_upgrades: state.completed_upgrades.clone(),
            tracked_tasks: state.tracked_tasks.clone(),
            completed_tasks: state.completed_tasks.clone(),
            collected: state.collected.clone(),
        }
    }

    /// Merge into a fresh `AppState`, dropping IDs that don't resolve in the
    /// current data version. Returns a human-readable warning if anything
    /// was dropped or if the data version changed.
    pub fn merge_into(self, state: &mut AppState) -> Option<String> {
        let mut warnings = Vec::new();

        if self.data_version != state.data.data_version {
            warnings.push(format!(
                "Game data updated ({} → {}); rechecking tracked items.",
                self.data_version, state.data.data_version
            ));
        }

        let upgrades_index = &state.index.upgrades_by_id;
        let tasks_index = &state.index.tasks_by_id;

        let (kept_upgrades, dropped_upgrades) =
            filter_known(self.tracked_upgrades, |id| upgrades_index.contains_key(id));
        let (kept_done_upgrades, _) = filter_known(self.completed_upgrades, |id| {
            upgrades_index.contains_key(id)
        });
        let (kept_tasks, dropped_tasks) =
            filter_known(self.tracked_tasks, |id| tasks_index.contains_key(id));
        let (kept_done_tasks, _) =
            filter_known(self.completed_tasks, |id| tasks_index.contains_key(id));

        if dropped_upgrades > 0 {
            warnings.push(format!(
                "Dropped {dropped_upgrades} tracked upgrade(s) no longer present in data."
            ));
        }
        if dropped_tasks > 0 {
            warnings.push(format!(
                "Dropped {dropped_tasks} tracked task(s) no longer present in data."
            ));
        }

        state.tracked_upgrades = kept_upgrades;
        state.completed_upgrades = kept_done_upgrades;
        state.tracked_tasks = kept_tasks;
        state.completed_tasks = kept_done_tasks;
        // Keep collected counts as-is — items rarely get renamed and we don't
        // want a wipe to nuke the user's effort.
        state.collected = self.collected;
        state.bump();

        if warnings.is_empty() {
            None
        } else {
            Some(warnings.join(" "))
        }
    }
}

fn filter_known<F>(set: HashSet<String>, known: F) -> (HashSet<String>, usize)
where
    F: Fn(&str) -> bool,
{
    let total = set.len();
    let kept: HashSet<String> = set.into_iter().filter(|id| known(id)).collect();
    let dropped = total - kept.len();
    (kept, dropped)
}

/// On-disk shape for `overrides.json`. Lives next to `state.json` so a
/// corrupt overrides file never takes user progress down with it.
#[derive(Serialize, Deserialize)]
pub struct PersistedOverrides {
    pub schema_version: u32,
    pub data_version: String,
    #[serde(default)]
    pub overrides: HashMap<UpgradeId, RecipeOverride>,
}

impl PersistedOverrides {
    pub fn from_app(state: &AppState) -> Self {
        Self {
            schema_version: OVERRIDES_SCHEMA_VERSION,
            data_version: state.data.data_version.clone(),
            overrides: state.overrides.clone(),
        }
    }

    /// Drop entries whose upgrade IDs no longer resolve, but keep the rest —
    /// renames between data versions will surface as orphans here, identical
    /// to how `PersistedState` handles tracked-but-missing upgrades.
    pub fn merge_into(self, state: &mut AppState) -> Option<String> {
        let mut warnings = Vec::new();
        if self.data_version != state.data.data_version {
            warnings.push(format!(
                "Recipe overrides were saved against data {} (now {}); rechecking.",
                self.data_version, state.data.data_version
            ));
        }

        let mut kept = HashMap::new();
        let mut dropped = 0usize;
        for (id, ov) in self.overrides {
            if state.index.upgrades_by_id.contains_key(&id) {
                kept.insert(id, ov);
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            warnings.push(format!(
                "Dropped {dropped} recipe override(s) for upgrade(s) no longer present."
            ));
        }

        state.overrides = kept;
        state.bump();

        if warnings.is_empty() {
            None
        } else {
            Some(warnings.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{HideoutModule, Item, Requirement, Upgrade};

    fn fixture() -> Arc<GameData> {
        Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: vec![HideoutModule {
                id: "workbench".into(),
                name: "Workbench".into(),
                upgrades: vec![
                    Upgrade {
                        id: "workbench_lv1".into(),
                        name: "Workbench".into(),
                        level: 1,
                        description: String::new(),
                        requirements: vec![
                            Requirement {
                                item_id: "bolts".into(),
                                quantity: 5,
                            },
                            Requirement {
                                item_id: "screws".into(),
                                quantity: 3,
                            },
                        ],
                    },
                    Upgrade {
                        id: "workbench_lv2".into(),
                        name: "Workbench".into(),
                        level: 2,
                        description: String::new(),
                        requirements: vec![Requirement {
                            item_id: "bolts".into(),
                            quantity: 7,
                        }],
                    },
                ],
            }],
            vendors: vec![],
            items: vec![
                Item {
                    id: "bolts".into(),
                    name: "Bolts".into(),
                    icon_path: "icons/bolts.png".into(),
                },
                Item {
                    id: "screws".into(),
                    name: "Screws".into(),
                    icon_path: "icons/screws.png".into(),
                },
            ],
        })
    }

    #[test]
    fn active_items_aggregates_across_tracked_upgrades() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        let active = s.active_items();
        let bolts = active.iter().find(|a| a.item_id == "bolts").unwrap();
        assert_eq!(bolts.needed, 12);
        assert_eq!(bolts.sources.len(), 2);
    }

    #[test]
    fn completed_upgrade_drops_from_view() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        assert!(s.active_items().is_empty());
    }

    #[test]
    fn active_items_sorted_by_remaining_descending() {
        // Both upgrades tracked → bolts needs 12, screws needs 4.
        // Collecting 11 bolts pushes its remaining to 1, less than screws'
        // remaining of 4 — so screws should sort BEFORE bolts now.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);

        // Baseline (no collection yet): bolts (needed 12) > screws (needed 4).
        let active = s.active_items();
        assert_eq!(active[0].item_id, "bolts");
        assert_eq!(active[1].item_id, "screws");

        // After collecting 11 bolts, screws (remaining 4) > bolts (remaining 1).
        s.set_collected(&"bolts".to_string(), 11);
        let active = s.active_items();
        assert_eq!(
            active[0].item_id, "screws",
            "screws should lead once bolts is nearly done"
        );
        assert_eq!(active[1].item_id, "bolts");
    }

    #[test]
    fn cycle_resets_at_target() {
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 4);
        let (v, reset) = s.cycle_collected(&"bolts".to_string(), 5);
        assert_eq!(v, 5);
        assert!(!reset);
        let (v, reset) = s.cycle_collected(&"bolts".to_string(), 5);
        assert_eq!(v, 0);
        assert!(reset);
    }

    #[test]
    fn track_and_done_are_mutually_exclusive() {
        let mut s = AppState::new(fixture());
        let id = "workbench_lv1".to_string();
        s.set_tracked_upgrade(&id, true);
        assert!(s.tracked_upgrades.contains(&id));
        s.set_completed_upgrade(&id, true);
        assert!(!s.tracked_upgrades.contains(&id));
        assert!(s.completed_upgrades.contains(&id));
    }

    #[test]
    fn persistence_round_trip() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_collected(&"bolts".to_string(), 3);
        let persisted = PersistedState::from_app(&s);
        let json = serde_json::to_string(&persisted).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();

        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert!(s2.tracked_upgrades.contains("workbench_lv1"));
        assert_eq!(*s2.collected.get("bolts").unwrap(), 3);
    }

    #[test]
    fn effective_requirements_falls_back_to_base_when_no_override() {
        let s = AppState::new(fixture());
        let reqs = s.effective_requirements(&"workbench_lv1".to_string());
        // Fixture defines bolts × 5 and screws × 3 for workbench_lv1.
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().any(|r| r.item_id == "bolts" && r.quantity == 5));
        assert!(reqs
            .iter()
            .any(|r| r.item_id == "screws" && r.quantity == 3));
    }

    #[test]
    fn override_drives_active_items_aggregation() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);

        // User says: workbench_lv1 actually needs bolts × 10 (not 5) and no screws.
        let mut slots: [Option<crate::data::Requirement>; RECIPE_SLOTS] =
            std::array::from_fn(|_| None);
        slots[0] = Some(crate::data::Requirement {
            item_id: "bolts".into(),
            quantity: 10,
        });
        s.set_recipe_override(
            &"workbench_lv1".to_string(),
            crate::data::RecipeOverride { slots },
        );

        let active = s.active_items();
        let bolts = active.iter().find(|a| a.item_id == "bolts").unwrap();
        assert_eq!(
            bolts.needed, 10,
            "override quantity should flow into aggregation"
        );
        assert!(
            !active.iter().any(|a| a.item_id == "screws"),
            "removed item should disappear from the wishlist"
        );
    }

    #[test]
    fn set_recipe_override_clears_when_matches_base() {
        let mut s = AppState::new(fixture());
        // Build a "fake override" that exactly mirrors the bundled recipe.
        let mut slots: [Option<crate::data::Requirement>; RECIPE_SLOTS] =
            std::array::from_fn(|_| None);
        slots[0] = Some(crate::data::Requirement {
            item_id: "bolts".into(),
            quantity: 5,
        });
        slots[1] = Some(crate::data::Requirement {
            item_id: "screws".into(),
            quantity: 3,
        });
        s.set_recipe_override(
            &"workbench_lv1".to_string(),
            crate::data::RecipeOverride { slots },
        );

        assert!(
            !s.is_overridden(&"workbench_lv1".to_string()),
            "an override identical to the bundled recipe should collapse to no override"
        );
    }

    #[test]
    fn persisted_overrides_drop_unknown_upgrades() {
        let mut s = AppState::new(fixture());
        let mut slots: [Option<crate::data::Requirement>; RECIPE_SLOTS] =
            std::array::from_fn(|_| None);
        slots[0] = Some(crate::data::Requirement {
            item_id: "bolts".into(),
            quantity: 7,
        });
        let persisted = PersistedOverrides {
            schema_version: OVERRIDES_SCHEMA_VERSION,
            data_version: "older".into(),
            overrides: HashMap::from([
                (
                    "workbench_lv1".to_string(),
                    crate::data::RecipeOverride {
                        slots: slots.clone(),
                    },
                ),
                (
                    "ghost_upgrade".to_string(),
                    crate::data::RecipeOverride { slots },
                ),
            ]),
        };
        let warn = persisted
            .merge_into(&mut s)
            .expect("data-version drift should warn");
        assert!(warn.contains("Dropped 1 recipe override"));
        assert!(s.overrides.contains_key("workbench_lv1"));
        assert!(!s.overrides.contains_key("ghost_upgrade"));
    }

    #[test]
    fn data_version_mismatch_warns_but_keeps_collected() {
        let mut s = AppState::new(fixture());
        let persisted = PersistedState {
            schema_version: 1,
            data_version: "older".into(),
            tracked_upgrades: HashSet::from(["nonexistent_upgrade".to_string()]),
            completed_upgrades: HashSet::new(),
            tracked_tasks: HashSet::new(),
            completed_tasks: HashSet::new(),
            collected: HashMap::from([("bolts".to_string(), 7)]),
        };
        let warn = persisted.merge_into(&mut s).expect("should warn");
        assert!(warn.contains("Game data updated"));
        assert!(warn.contains("Dropped 1 tracked upgrade"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 7);
    }
}
