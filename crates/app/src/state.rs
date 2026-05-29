//! Application state — what the user has enabled, completed, and collected.
//!
//! `AppState` lives behind an `Arc<RwLock<_>>`. Both the GUI thread and the
//! (future) VR thread mutate it; both hold the write lock only for the
//! duration of the mutation.

use crate::data::{
    base_slots, DataIndex, GameData, ItemId, ModuleId, RecipeOverride, Requirement, TaskId,
    UpgradeId, RECIPE_SLOTS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const OVERRIDES_SCHEMA_VERSION: u32 = 1;

pub type ContainerId = String;

/// A user-defined secondary container — a backpack, item case, etc. Its
/// contents count toward owned totals for upgrade readiness, progress,
/// surplus, and the owned-items list exactly like the stash, but it's named
/// and managed separately on the Containers tab. The stash itself stays
/// modeled as [`AppState::collected`] — the implicit "primary container" — so
/// the quick +/- and VR-click edits keep targeting it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: ContainerId,
    pub name: String,
    #[serde(default)]
    pub contents: HashMap<ItemId, u32>,
}

pub struct AppState {
    pub data: Arc<GameData>,
    pub index: Arc<DataIndex>,
    pub tracked_upgrades: HashSet<UpgradeId>,
    pub completed_upgrades: HashSet<UpgradeId>,
    pub tracked_tasks: HashSet<TaskId>,
    pub completed_tasks: HashSet<TaskId>,
    pub collected: HashMap<ItemId, u32>,
    /// User-defined secondary containers (backpacks, item cases). Their
    /// contents sum with `collected` via [`AppState::owned_total`] for every
    /// "how many do I own?" computation. `collected` stays the implicit
    /// primary container and the target of the quick +/- / VR-click edits.
    pub containers: Vec<Container>,
    /// Monotonic id source for `containers`; persisted so a deleted
    /// container's id is never reused while anything still references it.
    pub next_container_seq: u64,
    /// Modules the user has marked as unavailable (e.g. quest-locked). Their
    /// tracked upgrades stay tracked but don't contribute to `active_items`,
    /// so the wishlist hides items the user can't act on yet.
    pub disabled_modules: HashSet<ModuleId>,
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

/// How far ahead the surplus / "redundant items" calculation looks when
/// deciding what counts as still-needed. Selected per-view in the Items DB
/// (see [`AppState::needed_by_id`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedHorizon {
    /// Only upgrades the user is actively tracking (minus completed). Most
    /// aggressive — anything not tracked reads as redundant.
    TrackedOnly,
    /// The lowest not-completed level of every non-disabled module — the next
    /// thing buildable there. One step ahead everywhere.
    AllNatural,
    /// Every not-completed level of every non-disabled module, summed across
    /// levels. Most conservative — flags the least as redundant.
    AllFuture,
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
            containers: Vec::new(),
            next_container_seq: 0,
            disabled_modules: HashSet::new(),
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
            // Natural progression: completing level N pulls the same module's
            // next level into the tracked set, so the user always has a live
            // target without having to remember to manually advance. We don't
            // touch already-completed or already-tracked entries (lets the
            // user skip a tier manually without us re-adding it).
            if let Some(next_id) = self.next_level_upgrade(id) {
                if !self.completed_upgrades.contains(&next_id)
                    && !self.tracked_upgrades.contains(&next_id)
                {
                    self.tracked_upgrades.insert(next_id);
                }
            }
        } else {
            self.completed_upgrades.remove(id);
        }
        self.bump();
    }

    /// Promote an upgrade based on an OCR sighting of its panel.
    ///
    /// **Always**: marks every lower-level upgrade in the same module as
    /// completed. The game only displays the Facility Upgrade panel
    /// for an upgrade once every prior level has been claimed, so
    /// seeing Lv N on screen is proof that Lv (N-1) is done — even
    /// if the user forgot to tick the box in the app. Prior
    /// completion is independent of intent: it's a state inference,
    /// not a workflow choice.
    ///
    /// **Conditional on `auto_track`**: adds the OCR'd upgrade itself
    /// to the tracked set (the in-headset wishlist overlay). When
    /// `auto_track` is `false` the OCR'd upgrade is NOT tracked —
    /// even if `set_completed_upgrade`'s built-in next-level
    /// auto-track would otherwise have pulled it in as a side effect
    /// of completing Lv (N-1). Lets users bulk-OCR panels to refresh
    /// inventory counts without polluting their "what I'm working
    /// on" list.
    ///
    /// Returns a per-change record so callers can surface what changed
    /// (the OCR overlay shows it, the worker logs it). Caller is
    /// responsible for issuing a single `SaveTick` after — the underlying
    /// `set_*` helpers already bump `version` once each.
    pub fn apply_ocr_progression(
        &mut self,
        upgrade_id: &UpgradeId,
        auto_track: bool,
    ) -> OcrProgression {
        let mut out = OcrProgression::default();
        let Some(uref) = self.index.upgrades_by_id.get(upgrade_id).cloned() else {
            return out;
        };
        let module_id = uref.module_id.clone();
        let current_level = uref.upgrade.level;

        // Snapshot tracked-state for `tracked_self` semantics + the
        // auto-track-off undo path below.
        let was_tracked = self.tracked_upgrades.contains(upgrade_id);
        let was_completed = self.completed_upgrades.contains(upgrade_id);

        // Prior-level completions. Collect ids first so we don't hold a
        // borrow into `data` across the mutating loop.
        let prior_ids: Vec<UpgradeId> = self
            .data
            .modules
            .iter()
            .find(|m| m.id == module_id)
            .map(|m| {
                m.upgrades
                    .iter()
                    .filter(|u| u.level < current_level)
                    .map(|u| u.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for prior_id in &prior_ids {
            if !self.completed_upgrades.contains(prior_id) {
                self.set_completed_upgrade(prior_id, true);
                out.completed_priors.push(prior_id.clone());
            }
        }

        if auto_track {
            // Track the OCR'd upgrade unless the user already marked it
            // completed — re-opening the panel of a completed upgrade
            // shouldn't un-complete it.
            if !self.completed_upgrades.contains(upgrade_id)
                && !self.tracked_upgrades.contains(upgrade_id)
            {
                self.set_tracked_upgrade(upgrade_id, true);
            }
        } else if !was_tracked && self.tracked_upgrades.contains(upgrade_id) {
            // Auto-track is disabled but `set_completed_upgrade`'s
            // natural cascade pulled the OCR'd upgrade into the
            // tracked set as a side effect of completing Lv (N-1).
            // Undo that — the user explicitly opted out. Skip the
            // undo if the user had it tracked beforehand (we mustn't
            // erase their existing choice).
            self.set_tracked_upgrade(upgrade_id, false);
        }
        let is_tracked = self.tracked_upgrades.contains(upgrade_id);
        out.tracked_self = !was_tracked && !was_completed && is_tracked;
        out
    }

    /// Look up the next-higher-level upgrade in the same module (e.g. given
    /// `GeneratorLv1`, return `GeneratorLv2`'s id if it exists).
    fn next_level_upgrade(&self, id: &UpgradeId) -> Option<UpgradeId> {
        let uref = self.index.upgrades_by_id.get(id)?;
        let next_level = uref.upgrade.level + 1;
        let module = self.data.modules.iter().find(|m| m.id == uref.module_id)?;
        module
            .upgrades
            .iter()
            .find(|u| u.level == next_level)
            .map(|u| u.id.clone())
    }

    pub fn set_module_disabled(&mut self, id: &ModuleId, on: bool) {
        let changed = if on {
            self.disabled_modules.insert(id.clone())
        } else {
            self.disabled_modules.remove(id)
        };
        if changed {
            self.bump();
        }
    }

    /// True if `module_id` is either directly disabled OR its in-game parent
    /// area (host module like Kitchen Area, or a synthetic category like
    /// Storage Zone) is disabled. Used by the wishlist aggregation so that
    /// disabling a parent cascades through every child — and so that
    /// re-enabling a parent restores the children to their individual state
    /// rather than forcing all of them on.
    /// True if this upgrade is tracked, not yet completed, and the user has
    /// collected enough of every required item to claim it. Ignores whether
    /// the collected counts are also needed for sibling upgrades — the goal
    /// here is to surface "you've got the materials for this one" cues in
    /// the desktop UI, not gate aggregation. Empty requirements lists are
    /// NOT ready: every hideout upgrade in this game costs something, so an
    /// empty recipe means "we don't know what it costs yet" (a placeholder
    /// added because we know the level exists from a screenshot but don't
    /// have its requirements). Flashing such a cell green would read as
    /// "buy it now" when the user actually needs to fill in the recipe
    /// first via the Edit panel.
    pub fn is_upgrade_ready(&self, upgrade_id: &UpgradeId) -> bool {
        if !self.tracked_upgrades.contains(upgrade_id)
            || self.completed_upgrades.contains(upgrade_id)
        {
            return false;
        }
        let reqs = self.effective_requirements(upgrade_id);
        if reqs.is_empty() {
            return false;
        }
        reqs.iter()
            .all(|r| self.owned_total(&r.item_id) >= r.quantity)
    }

    /// Collected-vs-needed rollup for one upgrade's *effective* recipe
    /// (post-override, non-empty slots). Per item we cap the contribution at
    /// that item's required quantity — over-collecting bolts because three
    /// other tracked upgrades also want them must not push *this* upgrade past
    /// its own 100%. Pure read; mutates nothing.
    pub fn upgrade_progress(&self, upgrade_id: &UpgradeId) -> UpgradeProgress {
        let mut collected = 0;
        let mut needed = 0;
        for req in self.effective_requirements(upgrade_id) {
            let have = self.owned_total(&req.item_id);
            collected += have.min(req.quantity);
            needed += req.quantity;
        }
        UpgradeProgress { collected, needed }
    }

    /// How much we trust an upgrade's recipe (drives the confidence badge in
    /// the Hideout views). `Assumed` wins whenever the effective recipe is
    /// empty — "we don't know the cost" is the dominant signal, the same
    /// reason `is_upgrade_ready` refuses to flash empty recipes green.
    pub fn recipe_knowledge(&self, upgrade_id: &UpgradeId) -> RecipeKnowledge {
        if self.effective_requirements(upgrade_id).is_empty() {
            RecipeKnowledge::Assumed
        } else if self.is_overridden(upgrade_id) {
            RecipeKnowledge::Edited
        } else {
            RecipeKnowledge::Bundled
        }
    }

    /// Tracked, not-completed, not-(effectively-)disabled upgrades ordered for
    /// "what should I claim or grind next?". The filter mirrors `active_items`
    /// exactly (difference against completed, skip disabled modules) so this
    /// per-upgrade list and the per-item wishlist always agree on what counts
    /// as "active". Ordering buckets:
    ///   0. Ready — every material collected, recipe known. Float to the top.
    ///   1. In progress (some collected), descending by fraction so the
    ///      nearest-to-done sits highest.
    ///   2. Known recipe, nothing collected yet.
    ///   3. Assumed (empty recipe) — parked last and flagged, because its
    ///      fraction is a meaningless 0 and the action there is "fill in the
    ///      recipe", not "go grind".
    ///
    /// Tiebreak within a bucket: module name, then level, for a stable order
    /// that doesn't reshuffle on unrelated clicks.
    pub fn hideout_progress_rows(&self) -> Vec<UpgradeProgressRow> {
        let mut rows: Vec<UpgradeProgressRow> = self
            .tracked_upgrades
            .difference(&self.completed_upgrades)
            .filter_map(|id| {
                let uref = self.index.upgrades_by_id.get(id)?;
                if self.is_module_effectively_disabled(&uref.module_id) {
                    return None;
                }
                Some(UpgradeProgressRow {
                    upgrade_id: id.clone(),
                    module_name: uref.module_name.clone(),
                    level: uref.upgrade.level,
                    progress: self.upgrade_progress(id),
                    knowledge: self.recipe_knowledge(id),
                    ready: self.is_upgrade_ready(id),
                })
            })
            .collect();

        // Lower rank sorts first. Keying on an explicit bucket (rather than
        // raw fraction) keeps "ready" and "assumed" from interleaving with the
        // in-progress band — an assumed recipe and a known-but-untouched one
        // both have fraction 0, but they belong at opposite ends of the list.
        fn rank(r: &UpgradeProgressRow) -> u8 {
            if matches!(r.knowledge, RecipeKnowledge::Assumed) {
                3
            } else if r.ready {
                0
            } else if r.progress.collected > 0 {
                1
            } else {
                2
            }
        }

        rows.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                // Within the in-progress band, higher fraction first. total_cmp
                // keeps it panic-free on the f32 (matches house caution).
                .then_with(|| b.progress.fraction().total_cmp(&a.progress.fraction()))
                .then_with(|| a.module_name.cmp(&b.module_name))
                .then_with(|| a.level.cmp(&b.level))
        });
        rows
    }

    pub fn is_module_effectively_disabled(&self, module_id: &str) -> bool {
        if self.disabled_modules.contains(module_id) {
            return true;
        }
        if let Some(parent) = crate::hierarchy::parent_disable_id(module_id) {
            if self.disabled_modules.contains(&parent) {
                return true;
            }
        }
        false
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

    // --- Secondary containers ----------------------------------------------

    /// Total owned quantity of `item_id` across the stash (`collected`) AND
    /// every secondary container. This is the figure that drives upgrade
    /// readiness, progress, surplus, and the owned-items list — "do I have
    /// enough?" always counts the whole inventory. The quick +/- controls and
    /// VR-click cycling deliberately still target only the stash
    /// (`set_collected` / `adjust_collected` / `cycle_collected`); secondary
    /// containers are edited from the Containers tab.
    pub fn owned_total(&self, item_id: &ItemId) -> u32 {
        let mut total = self.collected.get(item_id).copied().unwrap_or(0);
        for c in &self.containers {
            total = total.saturating_add(c.contents.get(item_id).copied().unwrap_or(0));
        }
        total
    }

    /// Mint a fresh, collision-proof container id. The counter is persisted,
    /// so ids are never reused even after a container is deleted.
    fn mint_container_id(&mut self) -> ContainerId {
        self.next_container_seq += 1;
        format!("ctr-{}", self.next_container_seq)
    }

    /// Create a new secondary container, returning its freshly minted id.
    pub fn create_container(&mut self, name: String) -> ContainerId {
        let id = self.mint_container_id();
        self.containers.push(Container {
            id: id.clone(),
            name,
            contents: HashMap::new(),
        });
        self.bump();
        id
    }

    /// Rename a container. No-op if the id is unknown or the name is unchanged.
    pub fn rename_container(&mut self, id: &ContainerId, name: String) {
        if let Some(c) = self.containers.iter_mut().find(|c| &c.id == id) {
            if c.name != name {
                c.name = name;
                self.bump();
            }
        }
    }

    /// Delete a container and everything it held. No-op if the id is unknown.
    pub fn delete_container(&mut self, id: &ContainerId) {
        let before = self.containers.len();
        self.containers.retain(|c| &c.id != id);
        if self.containers.len() != before {
            self.bump();
        }
    }

    /// Set the quantity of `item` inside container `id`. Zero removes the
    /// entry (mirrors `set_collected`). No-op if the container id is unknown.
    pub fn set_container_item(&mut self, id: &ContainerId, item: &ItemId, value: u32) {
        if let Some(c) = self.containers.iter_mut().find(|c| &c.id == id) {
            if value == 0 {
                c.contents.remove(item);
            } else {
                c.contents.insert(item.clone(), value);
            }
            self.bump();
        }
    }

    /// Adjust the quantity of `item` inside container `id` by `delta`, clamped
    /// at 0. Mirrors `adjust_collected` but scoped to one container.
    pub fn adjust_container_item(&mut self, id: &ContainerId, item: &ItemId, delta: i64) {
        let cur = self
            .containers
            .iter()
            .find(|c| &c.id == id)
            .and_then(|c| c.contents.get(item).copied())
            .unwrap_or(0) as i64;
        let next = (cur + delta).max(0) as u32;
        self.set_container_item(id, item, next);
    }

    pub fn reset_all(&mut self) {
        self.tracked_upgrades.clear();
        self.completed_upgrades.clear();
        self.tracked_tasks.clear();
        self.completed_tasks.clear();
        self.collected.clear();
        // Clear secondary containers too — "Reset progress" wipes the whole
        // inventory. `next_container_seq` stays monotonic (not reset) so a
        // recreated container can never reuse a just-cleared id.
        self.containers.clear();
        self.disabled_modules.clear();
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
            if self.is_module_effectively_disabled(&uref.module_id) {
                continue;
            }
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
                let collected = self.owned_total(&item_id);
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
        //   2. Within the active group, sort by descending NEEDED quantity
        //      so the biggest grinds sit top-left. We deliberately key on
        //      `needed` rather than `needed - collected` — sorting on the
        //      remaining count makes the grid reshuffle on every click in
        //      the VR overlay, which is disorienting.
        //   3. Tiebreak alphabetically by name for stability when two
        //      items have the same target quantity.
        out.sort_by(|a, b| {
            let a_done = a.collected >= a.needed;
            let b_done = b.collected >= b.needed;
            a_done
                .cmp(&b_done)
                .then_with(|| b.needed.cmp(&a.needed)) // descending
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Set of upgrade ids whose requirements count as "needed" for `horizon`.
    /// Always excludes completed upgrades and upgrades whose module is
    /// effectively disabled — the surplus view must never tell you to keep
    /// items for a module you've shelved.
    fn upgrades_for_horizon(&self, horizon: NeedHorizon) -> Vec<UpgradeId> {
        match horizon {
            NeedHorizon::TrackedOnly => {
                self.tracked_upgrades
                    .difference(&self.completed_upgrades)
                    .filter(|id| {
                        self.index.upgrades_by_id.get(*id).is_some_and(|uref| {
                            !self.is_module_effectively_disabled(&uref.module_id)
                        })
                    })
                    .cloned()
                    .collect()
            }
            NeedHorizon::AllNatural => self
                .data
                .modules
                .iter()
                .filter(|m| !self.is_module_effectively_disabled(&m.id))
                .filter_map(|m| {
                    m.upgrades
                        .iter()
                        .filter(|u| !self.completed_upgrades.contains(&u.id))
                        .min_by_key(|u| u.level)
                        .map(|u| u.id.clone())
                })
                .collect(),
            NeedHorizon::AllFuture => self
                .data
                .modules
                .iter()
                .filter(|m| !self.is_module_effectively_disabled(&m.id))
                .flat_map(|m| {
                    m.upgrades
                        .iter()
                        .filter(|u| !self.completed_upgrades.contains(&u.id))
                        .map(|u| u.id.clone())
                })
                .collect(),
        }
    }

    /// Total quantity of each item required across the upgrade set implied by
    /// `horizon`. The single source of truth for the surplus / redundant-items
    /// view in the Items DB. Applies recipe overrides via
    /// [`Self::effective_requirements`]; hideout-only (never consults tasks).
    pub fn needed_by_id(&self, horizon: NeedHorizon) -> HashMap<ItemId, u32> {
        let mut totals: HashMap<ItemId, u32> = HashMap::new();
        for upgrade_id in self.upgrades_for_horizon(horizon) {
            for req in self.effective_requirements(&upgrade_id) {
                *totals.entry(req.item_id).or_insert(0) += req.quantity;
            }
        }
        totals
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

/// Collected-vs-needed rollup for a single upgrade's effective recipe.
/// `needed == 0` means the recipe is empty — we don't actually know what the
/// upgrade costs yet (a placeholder added because a screenshot proved the
/// level exists). `fraction()` guards that case so an unknown recipe never
/// reads as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeProgress {
    /// Σ min(collected, required) across the recipe — capped per item.
    pub collected: u32,
    /// Σ required across the recipe. Zero ⇒ unknown/placeholder recipe.
    pub needed: u32,
}

impl UpgradeProgress {
    /// 0.0..=1.0. An empty recipe (`needed == 0`) returns 0.0: unknown cost
    /// must not masquerade as 100% done.
    pub fn fraction(&self) -> f32 {
        if self.needed == 0 {
            0.0
        } else {
            (self.collected as f32 / self.needed as f32).clamp(0.0, 1.0)
        }
    }
}

/// How much we trust an upgrade's recipe — drives the confidence badge in the
/// Hideout views. Three-way so "we shipped a recipe the user then corrected"
/// reads differently from "we shipped it as-is".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeKnowledge {
    /// Effective recipe is empty — we don't know the cost. Parked + flagged.
    Assumed,
    /// Bundled recipe, used as shipped (no user override).
    Bundled,
    /// User has corrected the recipe via the Edit panel.
    Edited,
}

/// One row of the Hideout "By progress" list — a per-upgrade view, distinct
/// from `ActiveItem`'s per-item aggregation (which feeds the VR overlay and
/// must not change). Carries everything the pane needs so it never re-locks
/// per row.
#[derive(Debug, Clone)]
pub struct UpgradeProgressRow {
    pub upgrade_id: UpgradeId,
    pub module_name: String,
    pub level: u32,
    pub progress: UpgradeProgress,
    pub knowledge: RecipeKnowledge,
    /// `is_upgrade_ready`: tracked, materials complete, recipe known.
    pub ready: bool,
}

/// What [`AppState::apply_ocr_progression`] actually changed. Surfaces
/// the auto-complete + auto-track behavior in the OCR overlay so the
/// user sees exactly what was promoted, and feeds the tracing logs.
#[derive(Debug, Clone, Default)]
pub struct OcrProgression {
    /// Prior-level upgrade ids that were freshly marked completed.
    pub completed_priors: Vec<UpgradeId>,
    /// `true` when the OCR'd upgrade got newly added to the tracked set
    /// (it wasn't already tracked or completed).
    pub tracked_self: bool,
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
    #[serde(default)]
    pub disabled_modules: HashSet<ModuleId>,
    /// Secondary containers. `#[serde(default)]` means a `state.json` written
    /// before this feature (no `containers` key) loads as an empty list, so
    /// existing users see no behavior change.
    #[serde(default)]
    pub containers: Vec<Container>,
    #[serde(default)]
    pub next_container_seq: u64,
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
            disabled_modules: state.disabled_modules.clone(),
            containers: state.containers.clone(),
            next_container_seq: state.next_container_seq,
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
        // Drop disabled-module entries that no longer match any module — keeps
        // the set tidy across data-version bumps that rename modules.
        let known_modules: HashSet<&ModuleId> = state.data.modules.iter().map(|m| &m.id).collect();
        state.disabled_modules = self
            .disabled_modules
            .into_iter()
            .filter(|id| known_modules.contains(id))
            .collect();
        // Keep collected counts as-is — items rarely get renamed and we don't
        // want a wipe to nuke the user's effort.
        state.collected = self.collected;
        // Secondary containers carry over verbatim, same "never nuke the
        // user's effort" stance as `collected`. An item id that vanished from
        // the dataset simply sums into no recipe; it does no harm sitting in a
        // container. The id counter comes along so future mints can't collide
        // with a surviving container.
        state.containers = self.containers;
        state.next_container_seq = self.next_container_seq;
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
            modules: vec![
                HideoutModule {
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
                },
                // Second module whose sole upgrade carries an empty recipe — the
                // "we know this level exists but not its cost" placeholder. Lets
                // the progress/knowledge tests exercise the `Assumed` paths
                // without disturbing the workbench module's two-level shape (the
                // top-level-completion test relies on Lv2 being workbench's top).
                HideoutModule {
                    id: "placeholder".into(),
                    name: "Placeholder".into(),
                    upgrades: vec![Upgrade {
                        id: "placeholder_lv1".into(),
                        name: "Placeholder".into(),
                        level: 1,
                        description: String::new(),
                        requirements: vec![],
                    }],
                },
            ],
            vendors: vec![],
            items: vec![
                Item {
                    id: "bolts".into(),
                    name: "Bolts".into(),
                    icon_path: "icons/bolts.png".into(),
                    category: None,
                    subcategory: None,
                    weight: None,
                    price: None,
                    rarity: None,
                },
                Item {
                    id: "screws".into(),
                    name: "Screws".into(),
                    icon_path: "icons/screws.png".into(),
                    category: None,
                    subcategory: None,
                    weight: None,
                    price: None,
                    rarity: None,
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
        // Use the module's top level so completion has no next level to
        // auto-promote to — verifies the bare "completed → out of view"
        // path independent of natural-progression auto-tracking.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_completed_upgrade(&"workbench_lv2".to_string(), true);
        assert!(s.active_items().is_empty());
    }

    #[test]
    fn completing_a_level_auto_tracks_next_level() {
        // Natural progression: when level N is marked done, level N+1 in the
        // same module should be auto-tracked so the user always has a live
        // target without manually advancing tier-by-tier.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        assert!(
            s.tracked_upgrades.contains("workbench_lv2"),
            "lv2 should be auto-tracked once lv1 is done"
        );
    }

    #[test]
    fn completing_top_level_does_not_panic_or_re_add() {
        // Top-level completion has no level+1 to promote — make sure that
        // doesn't accidentally re-track the upgrade we just completed.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_completed_upgrade(&"workbench_lv2".to_string(), true);
        assert!(!s.tracked_upgrades.contains("workbench_lv2"));
        assert!(s.completed_upgrades.contains("workbench_lv2"));
    }

    #[test]
    fn disabled_module_hides_from_active_items() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        assert!(!s.active_items().is_empty());
        s.set_module_disabled(&"workbench".to_string(), true);
        assert!(
            s.active_items().is_empty(),
            "disabling the module should hide its tracked requirements"
        );
        s.set_module_disabled(&"workbench".to_string(), false);
        assert!(
            !s.active_items().is_empty(),
            "re-enabling restores the tracked requirements"
        );
    }

    #[test]
    fn active_items_sorted_by_needed_descending() {
        // Both upgrades tracked → bolts needs 12, screws needs 4.
        // Order must be stable across collection progress: keying on
        // `needed` (target) rather than `needed - collected` keeps the
        // VR grid from reshuffling on every click.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);

        // Baseline: bolts (needed 12) > screws (needed 4).
        let active = s.active_items();
        assert_eq!(active[0].item_id, "bolts");
        assert_eq!(active[1].item_id, "screws");

        // After collecting 11 bolts, order is unchanged — sort key is
        // `needed`, not `remaining`.
        s.set_collected(&"bolts".to_string(), 11);
        let active = s.active_items();
        assert_eq!(
            active[0].item_id, "bolts",
            "bolts stays first — sort key is `needed`, not `remaining`"
        );
        assert_eq!(active[1].item_id, "screws");
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
    fn apply_ocr_progression_marks_priors_completed_and_tracks_self() {
        // User starts with nothing tracked. OCR sees workbench Lv2 →
        // every prior level (Lv1) gets completed and Lv2 enters the
        // tracked set.
        let mut s = AppState::new(fixture());
        let prog = s.apply_ocr_progression(&"workbench_lv2".to_string(), true);
        assert_eq!(prog.completed_priors, vec!["workbench_lv1".to_string()]);
        assert!(prog.tracked_self);
        assert!(s.completed_upgrades.contains("workbench_lv1"));
        assert!(s.tracked_upgrades.contains("workbench_lv2"));
    }

    #[test]
    fn apply_ocr_progression_is_idempotent() {
        let mut s = AppState::new(fixture());
        s.apply_ocr_progression(&"workbench_lv2".to_string(), true);
        let second = s.apply_ocr_progression(&"workbench_lv2".to_string(), true);
        // Already done + already tracked → second pass changes nothing.
        assert!(second.completed_priors.is_empty());
        assert!(!second.tracked_self);
    }

    #[test]
    fn apply_ocr_progression_lv1_only_tracks_self() {
        let mut s = AppState::new(fixture());
        let prog = s.apply_ocr_progression(&"workbench_lv1".to_string(), true);
        assert!(prog.completed_priors.is_empty(), "no priors below Lv 1");
        assert!(prog.tracked_self);
        assert!(s.tracked_upgrades.contains("workbench_lv1"));
    }

    #[test]
    fn apply_ocr_progression_skips_when_already_completed() {
        // Re-OCRing a maxed-out panel shouldn't un-complete it.
        let mut s = AppState::new(fixture());
        s.set_completed_upgrade(&"workbench_lv2".to_string(), true);
        let prog = s.apply_ocr_progression(&"workbench_lv2".to_string(), true);
        assert!(!prog.tracked_self, "completed upgrades stay completed");
        assert!(s.completed_upgrades.contains("workbench_lv2"));
        assert!(!s.tracked_upgrades.contains("workbench_lv2"));
    }

    #[test]
    fn apply_ocr_progression_auto_track_off_completes_priors_but_no_tracking() {
        // `ocr_auto_track = false` workflow: user is bulk-OCR'ing many
        // panels to refresh inventory and explicitly DOESN'T want
        // their tracked list polluted. Prior completion must still
        // run (it's a state inference, not a workflow choice), and
        // the OCR'd upgrade must NOT end up in `tracked_upgrades`
        // — even though `set_completed_upgrade`'s natural cascade
        // would otherwise auto-track the next-level upgrade as a
        // side effect.
        let mut s = AppState::new(fixture());
        let prog = s.apply_ocr_progression(&"workbench_lv2".to_string(), false);
        assert_eq!(
            prog.completed_priors,
            vec!["workbench_lv1".to_string()],
            "priors still get completed regardless of auto_track",
        );
        assert!(
            !prog.tracked_self,
            "tracked_self reports the post-state — false when we didn't track",
        );
        assert!(
            s.completed_upgrades.contains("workbench_lv1"),
            "prior must still be marked completed",
        );
        assert!(
            !s.tracked_upgrades.contains("workbench_lv2"),
            "OCR'd upgrade must NOT be tracked when auto_track=false, \
             even after the prior-completion cascade",
        );
    }

    #[test]
    fn apply_ocr_progression_auto_track_off_preserves_existing_tracking() {
        // If the user had explicitly tracked the upgrade before the
        // OCR pass, the `auto_track=false` undo must NOT erase that
        // choice — the cascade-undo only applies to upgrades we just
        // pulled in as a side effect.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.apply_ocr_progression(&"workbench_lv2".to_string(), false);
        assert!(
            s.tracked_upgrades.contains("workbench_lv2"),
            "explicit user-tracked upgrades survive auto_track=false",
        );
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
            disabled_modules: HashSet::new(),
            containers: Vec::new(),
            next_container_seq: 0,
        };
        let warn = persisted.merge_into(&mut s).expect("should warn");
        assert!(warn.contains("Game data updated"));
        assert!(warn.contains("Dropped 1 tracked upgrade"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 7);
    }

    #[test]
    fn upgrade_progress_caps_each_item_at_its_target() {
        // Collecting more bolts than workbench_lv1 needs (because lv2 also
        // wants them) must not push lv1 past its own recipe: lv1 needs
        // bolts×5 + screws×3 = 8, so 10 bolts + 3 screws still reads 8/8.
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 10);
        s.set_collected(&"screws".to_string(), 3);
        let p = s.upgrade_progress(&"workbench_lv1".to_string());
        assert_eq!(p.needed, 8);
        assert_eq!(p.collected, 8, "per-item contribution caps at the target");
        assert_eq!(p.fraction(), 1.0);
    }

    #[test]
    fn upgrade_progress_partial_fraction() {
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 5); // 5 of 8 (screws still 0)
        let p = s.upgrade_progress(&"workbench_lv1".to_string());
        assert_eq!((p.collected, p.needed), (5, 8));
        assert!((p.fraction() - 0.625).abs() < 1e-6);
    }

    #[test]
    fn upgrade_progress_empty_recipe_is_zero_not_full() {
        // placeholder_lv1 has no requirements: unknown cost must read as 0/0,
        // fraction 0.0, NOT complete — never let a guess look claimable.
        let s = AppState::new(fixture());
        let p = s.upgrade_progress(&"placeholder_lv1".to_string());
        assert_eq!((p.collected, p.needed), (0, 0));
        assert_eq!(p.fraction(), 0.0);
    }

    #[test]
    fn recipe_knowledge_bundled_edited_assumed() {
        let mut s = AppState::new(fixture());
        assert_eq!(
            s.recipe_knowledge(&"workbench_lv1".to_string()),
            RecipeKnowledge::Bundled
        );
        // An override flips it to Edited.
        let mut slots: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
        slots[0] = Some(Requirement {
            item_id: "bolts".into(),
            quantity: 9,
        });
        s.set_recipe_override(&"workbench_lv1".to_string(), RecipeOverride { slots });
        assert_eq!(
            s.recipe_knowledge(&"workbench_lv1".to_string()),
            RecipeKnowledge::Edited
        );
        // Empty recipe = Assumed, regardless of tracking.
        assert_eq!(
            s.recipe_knowledge(&"placeholder_lv1".to_string()),
            RecipeKnowledge::Assumed
        );
    }

    #[test]
    fn hideout_progress_rows_floats_ready_then_by_fraction() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        // bolts=7 → lv2 fully stocked (7/7, ready); lv1 partial (5/8, no
        // screws yet).
        s.set_collected(&"bolts".to_string(), 7);
        let rows = s.hideout_progress_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].upgrade_id, "workbench_lv2");
        assert!(rows[0].ready, "ready upgrade floats to the top");
        assert_eq!(rows[1].upgrade_id, "workbench_lv1");
        assert!(!rows[1].ready);
    }

    #[test]
    fn hideout_progress_rows_parks_assumed_last() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true); // known, untouched
        s.set_tracked_upgrade(&"placeholder_lv1".to_string(), true); // assumed
        let rows = s.hideout_progress_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].upgrade_id, "workbench_lv1",
            "known not-started sorts above an assumed recipe"
        );
        assert_eq!(rows[1].upgrade_id, "placeholder_lv1");
        assert_eq!(rows[1].knowledge, RecipeKnowledge::Assumed);
    }

    #[test]
    fn hideout_progress_rows_excludes_completed_and_disabled() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        // Completing lv1 drops it from tracked and auto-tracks lv2.
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert!(rows.iter().all(|r| r.upgrade_id != "workbench_lv1"));
        assert!(rows.iter().any(|r| r.upgrade_id == "workbench_lv2"));
        // Disabling the module hides its tracked lv2 from the list entirely.
        s.set_module_disabled(&"workbench".to_string(), true);
        assert!(s.hideout_progress_rows().is_empty());
    }

    #[test]
    fn needed_by_id_tracked_only_sums_tracked_not_completed() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        let need = s.needed_by_id(NeedHorizon::TrackedOnly);
        assert_eq!(need.get("bolts"), Some(&7));
        assert_eq!(need.get("screws"), None);
    }

    #[test]
    fn needed_by_id_all_natural_picks_lowest_incomplete_level() {
        let s = AppState::new(fixture());
        // Nothing completed → the next buildable level is Lv1.
        let need = s.needed_by_id(NeedHorizon::AllNatural);
        assert_eq!(need.get("bolts"), Some(&5));
        assert_eq!(need.get("screws"), Some(&3));
    }

    #[test]
    fn needed_by_id_all_natural_advances_after_completion() {
        let mut s = AppState::new(fixture());
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        // Lv1 done → next buildable is Lv2 (bolts 7, no screws).
        let need = s.needed_by_id(NeedHorizon::AllNatural);
        assert_eq!(need.get("bolts"), Some(&7));
        assert_eq!(need.get("screws"), None);
    }

    #[test]
    fn needed_by_id_all_future_sums_every_remaining_level() {
        let s = AppState::new(fixture());
        let need = s.needed_by_id(NeedHorizon::AllFuture);
        assert_eq!(need.get("bolts"), Some(&12)); // Lv1 5 + Lv2 7
        assert_eq!(need.get("screws"), Some(&3));
    }

    #[test]
    fn needed_by_id_skips_disabled_modules() {
        let mut s = AppState::new(fixture());
        s.set_module_disabled(&"workbench".to_string(), true);
        assert!(s.needed_by_id(NeedHorizon::AllFuture).is_empty());
        assert!(s.needed_by_id(NeedHorizon::AllNatural).is_empty());
        // Even an explicitly tracked upgrade in a disabled module drops out.
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        assert!(s.needed_by_id(NeedHorizon::TrackedOnly).is_empty());
    }

    #[test]
    fn owned_total_sums_stash_and_containers() {
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 3);
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 4);
        assert_eq!(s.owned_total(&"bolts".to_string()), 7);
        // An item only in the stash is unaffected by container summing.
        s.set_collected(&"screws".to_string(), 2);
        assert_eq!(s.owned_total(&"screws".to_string()), 2);
        // Unknown item → 0, never panics.
        assert_eq!(s.owned_total(&"ghost".to_string()), 0);
    }

    #[test]
    fn upgrade_ready_counts_container_stock() {
        // workbench_lv2 needs bolts × 7. Split 4 in the stash + 3 in a
        // container — neither alone is enough, but together they satisfy it.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_collected(&"bolts".to_string(), 4);
        assert!(!s.is_upgrade_ready(&"workbench_lv2".to_string()));
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 3);
        assert!(
            s.is_upgrade_ready(&"workbench_lv2".to_string()),
            "stash + container together should satisfy the recipe"
        );
        // Deleting the container drops readiness back.
        s.delete_container(&c);
        assert!(!s.is_upgrade_ready(&"workbench_lv2".to_string()));
    }

    #[test]
    fn upgrade_progress_includes_containers() {
        // workbench_lv1 needs bolts×5 + screws×3 = 8, all stocked in one
        // container — progress must read 8/8 from container contents alone.
        let mut s = AppState::new(fixture());
        let c = s.create_container("Item case".into());
        s.set_container_item(&c, &"bolts".to_string(), 5);
        s.set_container_item(&c, &"screws".to_string(), 3);
        let p = s.upgrade_progress(&"workbench_lv1".to_string());
        assert_eq!((p.collected, p.needed), (8, 8));
        assert_eq!(p.fraction(), 1.0);
    }

    #[test]
    fn active_items_collected_includes_containers() {
        // The aggregated wishlist (and thus the VR overlay + preview pane,
        // which read ActiveItem.collected) must reflect the combined total.
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_collected(&"bolts".to_string(), 2);
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 3);
        let active = s.active_items();
        let bolts = active.iter().find(|a| a.item_id == "bolts").unwrap();
        assert_eq!(bolts.collected, 5, "combined stash + container total");
        assert_eq!(bolts.needed, 7);
    }

    #[test]
    fn adjust_container_item_clamps_at_zero_and_removes() {
        let mut s = AppState::new(fixture());
        let c = s.create_container("Backpack".into());
        s.adjust_container_item(&c, &"bolts".to_string(), 3);
        assert_eq!(s.owned_total(&"bolts".to_string()), 3);
        s.adjust_container_item(&c, &"bolts".to_string(), -10);
        assert_eq!(s.owned_total(&"bolts".to_string()), 0);
        // Zero removes the entry entirely (mirrors set_collected).
        assert!(!s.containers[0].contents.contains_key("bolts"));
    }

    #[test]
    fn rename_and_delete_container() {
        let mut s = AppState::new(fixture());
        let c = s.create_container("Bagpack".into());
        s.rename_container(&c, "Backpack".into());
        assert_eq!(s.containers[0].name, "Backpack");
        s.set_container_item(&c, &"bolts".to_string(), 5);
        assert_eq!(s.owned_total(&"bolts".to_string()), 5);
        s.delete_container(&c);
        assert!(s.containers.is_empty());
        assert_eq!(s.owned_total(&"bolts".to_string()), 0);
    }

    #[test]
    fn container_round_trip_persist() {
        let mut s = AppState::new(fixture());
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 4);
        let persisted = PersistedState::from_app(&s);
        let json = serde_json::to_string(&persisted).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();

        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert_eq!(s2.containers.len(), 1);
        assert_eq!(s2.containers[0].name, "Backpack");
        assert_eq!(s2.containers[0].contents.get("bolts"), Some(&4));
        assert_eq!(s2.owned_total(&"bolts".to_string()), 4);
        // The id counter survives so a freshly minted id can't collide with
        // the restored container.
        assert_eq!(s2.next_container_seq, 1);
        let c2 = s2.create_container("Item case".into());
        assert_ne!(c2, c);
    }

    #[test]
    fn old_state_json_without_containers_loads_empty() {
        // Exactly the shape today's state.json has — no `containers` /
        // `next_container_seq` keys. Must deserialize via serde defaults and
        // merge cleanly with no warning and all existing data intact.
        let json = r#"{
            "schema_version": 1,
            "data_version": "test",
            "tracked_upgrades": ["workbench_lv1"],
            "completed_upgrades": [],
            "tracked_tasks": [],
            "completed_tasks": [],
            "collected": {"bolts": 2},
            "disabled_modules": []
        }"#;
        let back: PersistedState = serde_json::from_str(json).unwrap();
        assert!(back.containers.is_empty());
        assert_eq!(back.next_container_seq, 0);
        let mut s = AppState::new(fixture());
        let warn = back.merge_into(&mut s);
        assert!(warn.is_none(), "same data version + known ids → no warning");
        assert!(s.containers.is_empty());
        assert!(s.tracked_upgrades.contains("workbench_lv1"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 2);
    }
}
