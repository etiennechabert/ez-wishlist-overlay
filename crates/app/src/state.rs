//! Application state — what the user has enabled, completed, and collected.
//!
//! `AppState` lives behind an `Arc<RwLock<_>>`. Both the GUI thread and the
//! (future) VR thread mutate it; both hold the write lock only for the
//! duration of the mutation.

use crate::data::{DataIndex, GameData, ItemId, TaskId, UpgradeId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub const STATE_SCHEMA_VERSION: u32 = 1;

pub struct AppState {
    pub data: Arc<GameData>,
    pub index: Arc<DataIndex>,
    pub tracked_upgrades: HashSet<UpgradeId>,
    pub completed_upgrades: HashSet<UpgradeId>,
    pub tracked_tasks: HashSet<TaskId>,
    pub completed_tasks: HashSet<TaskId>,
    pub collected: HashMap<ItemId, u32>,
    /// Tentative collected counts produced by VR-overlay clicks while
    /// "raid mode" is on (see `VrSettings::tentative_overlay_edits`). An
    /// entry here means the displayed value is `pending[id]` rather than
    /// `collected[id]`. Committed via `commit_pending` (survived the
    /// raid) or wiped via `discard_pending` (died — lost what was on you).
    pub pending: HashMap<ItemId, u32>,
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
            pending: HashMap::new(),
            version: 0,
            load_warning: None,
        }
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
        // Desktop edits are explicit and take precedence over any tentative
        // overlay click — otherwise the pending value would keep shadowing
        // the count the user just typed.
        self.pending.remove(item);
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

    /// Effective count: the tentative `pending` value if the overlay touched
    /// this item this raid, otherwise the committed `collected` value.
    /// Used by the GUI and the VR renderer so both show the same number
    /// the player just clicked.
    pub fn effective_collected(&self, item: &ItemId) -> u32 {
        if let Some(v) = self.pending.get(item) {
            return *v;
        }
        *self.collected.get(item).unwrap_or(&0)
    }

    /// Same cycle semantics as [`Self::cycle_collected`] but the new value
    /// is written into `pending` instead of `collected`. Cycling starts
    /// from the effective value, so consecutive clicks add up the way the
    /// player expects regardless of whether anything was committed before.
    ///
    /// If the new value equals the committed value, the pending entry is
    /// removed (clicking back to the original is the same as never having
    /// clicked).
    pub fn cycle_pending(&mut self, item: &ItemId, target: u32) -> (u32, bool) {
        let cur = self.effective_collected(item);
        let (next, was_reset) = if target == 0 {
            (cur + 1, false)
        } else if cur >= target {
            (0, true)
        } else {
            (cur + 1, false)
        };
        self.set_pending(item, next);
        (next, was_reset)
    }

    /// Set the tentative value directly. If it matches the committed value
    /// the pending entry is cleared — there is nothing to commit or revert.
    fn set_pending(&mut self, item: &ItemId, value: u32) {
        let committed = *self.collected.get(item).unwrap_or(&0);
        if value == committed {
            self.pending.remove(item);
        } else {
            self.pending.insert(item.clone(), value);
        }
        self.bump();
    }

    /// Move every pending value into `collected` and clear `pending`.
    /// Returns the number of entries committed. No-op (no bump) when
    /// there's nothing pending, so the save loop doesn't churn on
    /// every Commit click in an unchanged state.
    pub fn commit_pending(&mut self) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let n = self.pending.len();
        for (id, value) in self.pending.drain() {
            if value == 0 {
                self.collected.remove(&id);
            } else {
                self.collected.insert(id, value);
            }
        }
        self.bump();
        n
    }

    /// Drop every pending value, leaving `collected` untouched. Returns the
    /// number of entries discarded.
    pub fn discard_pending(&mut self) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        let n = self.pending.len();
        self.pending.clear();
        self.bump();
        n
    }

    /// Snapshot of every pending change, sorted by item name for stable UI.
    /// Each entry is `(item_id, name, committed, pending)` where the two
    /// counts differ; same-value entries are filtered out by `set_pending`.
    pub fn pending_diffs(&self) -> Vec<PendingDiff> {
        let mut out: Vec<PendingDiff> = self
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                let name = self.index.items_by_id.get(id)?.name.clone();
                let committed = *self.collected.get(id).unwrap_or(&0);
                Some(PendingDiff {
                    item_id: id.clone(),
                    name,
                    committed,
                    pending: *pending,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn reset_all(&mut self) {
        self.tracked_upgrades.clear();
        self.completed_upgrades.clear();
        self.tracked_tasks.clear();
        self.completed_tasks.clear();
        self.collected.clear();
        self.pending.clear();
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
            for req in &uref.upgrade.requirements {
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
                let committed = *self.collected.get(&item_id).unwrap_or(&0);
                let pending = self.pending.get(&item_id).copied();
                // `collected` on ActiveItem is the value to display — i.e.
                // the effective (tentative-or-committed) one. `pending`
                // exposes whether that came from an uncommitted click so
                // the GUI / VR renderer can draw a tentative marker.
                let collected = pending.unwrap_or(committed);
                Some(ActiveItem {
                    item_id,
                    name: item.name.clone(),
                    icon_path: item.icon_path.clone(),
                    needed,
                    collected,
                    pending,
                    sources,
                })
            })
            .collect();

        out.sort_by(|a, b| {
            let a_done = a.collected >= a.needed;
            let b_done = b.collected >= b.needed;
            a_done.cmp(&b_done).then_with(|| a.name.cmp(&b.name))
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
    /// Effective collected count — tentative if `pending` is set, otherwise
    /// the committed value. Use this when rendering the cell; cross-reference
    /// `pending` if you need to know whether it's confirmed.
    pub collected: u32,
    /// `Some(v)` when the VR overlay has tentatively bumped this item this
    /// raid and the user hasn't yet committed or discarded. `None` when
    /// `collected` is the committed source of truth.
    pub pending: Option<u32>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PendingDiff {
    /// Kept for symmetry with `ActiveItem` and in case a future UI wants
    /// per-row actions (e.g. commit a single pending diff).
    #[allow(dead_code)]
    pub item_id: ItemId,
    pub name: String,
    pub committed: u32,
    pub pending: u32,
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
    /// Tentative counts from a raid that hasn't been committed or
    /// discarded yet. Survives app restarts so the player can decide
    /// later. `default` keeps state files from older versions readable.
    #[serde(default)]
    pub pending: HashMap<ItemId, u32>,
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
            pending: state.pending.clone(),
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
        // Restore in-flight tentative counts so a crash or restart mid-raid
        // doesn't silently commit them.
        state.pending = self.pending;
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
            pending: HashMap::new(),
        };
        let warn = persisted.merge_into(&mut s).expect("should warn");
        assert!(warn.contains("Game data updated"));
        assert!(warn.contains("Dropped 1 tracked upgrade"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 7);
    }

    #[test]
    fn cycle_pending_starts_from_effective_value() {
        // Committed=2; one tentative click should land on 3 in pending,
        // leaving collected untouched.
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_collected(&bolts, 2);

        let (v, reset) = s.cycle_pending(&bolts, 5);
        assert_eq!(v, 3);
        assert!(!reset);
        assert_eq!(*s.collected.get(&bolts).unwrap(), 2);
        assert_eq!(*s.pending.get(&bolts).unwrap(), 3);

        // Another tentative click chains off the pending value, not the
        // committed one.
        let (v, _) = s.cycle_pending(&bolts, 5);
        assert_eq!(v, 4);
        assert_eq!(*s.pending.get(&bolts).unwrap(), 4);
    }

    #[test]
    fn cycle_pending_resets_at_target() {
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_collected(&bolts, 5);
        // Effective=5, target=5 → next click cycles to 0 in pending.
        let (v, reset) = s.cycle_pending(&bolts, 5);
        assert_eq!(v, 0);
        assert!(reset);
        assert_eq!(*s.pending.get(&bolts).unwrap(), 0);
        assert_eq!(*s.collected.get(&bolts).unwrap(), 5);
    }

    #[test]
    fn cycle_pending_back_to_committed_clears_entry() {
        // Player clicks once (committed 2 → pending 3) then clicks back
        // until pending equals collected — at that point we should drop
        // the pending entry, there's nothing to commit or revert.
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_collected(&bolts, 2);
        // Cycle pending up past the committed value, wrap to 0 at target,
        // then walk back up until pending matches the committed value —
        // that's the branch where the entry should be removed entirely.
        s.cycle_pending(&bolts, 5);
        s.cycle_pending(&bolts, 5);
        s.cycle_pending(&bolts, 4);
        s.cycle_pending(&bolts, 4);
        s.cycle_pending(&bolts, 4);
        assert!(!s.pending.contains_key(&bolts));
    }

    #[test]
    fn commit_pending_moves_into_collected() {
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_collected(&bolts, 1);
        s.cycle_pending(&bolts, 5); // pending=2
        s.cycle_pending(&bolts, 5); // pending=3

        let n = s.commit_pending();
        assert_eq!(n, 1);
        assert_eq!(*s.collected.get(&bolts).unwrap(), 3);
        assert!(s.pending.is_empty());
    }

    #[test]
    fn discard_pending_leaves_collected_intact() {
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_collected(&bolts, 1);
        s.cycle_pending(&bolts, 5); // pending=2

        let n = s.discard_pending();
        assert_eq!(n, 1);
        assert_eq!(*s.collected.get(&bolts).unwrap(), 1);
        assert!(s.pending.is_empty());
    }

    #[test]
    fn active_items_uses_effective_value() {
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_collected(&bolts, 1);
        s.cycle_pending(&bolts, 5); // effective=2

        let active = s.active_items();
        let row = active.iter().find(|a| a.item_id == bolts).unwrap();
        assert_eq!(row.collected, 2);
        assert_eq!(row.pending, Some(2));
    }

    #[test]
    fn pending_persists_round_trip() {
        let mut s = AppState::new(fixture());
        let bolts = "bolts".to_string();
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_collected(&bolts, 1);
        s.cycle_pending(&bolts, 5); // pending=2

        let json = serde_json::to_string(&PersistedState::from_app(&s)).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();

        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert_eq!(*s2.pending.get(&bolts).unwrap(), 2);
        assert_eq!(*s2.collected.get(&bolts).unwrap(), 1);
    }
}
