//! Application state — what the user has enabled, completed, and collected.
//!
//! `AppState` lives behind an `Arc<RwLock<_>>`. Both the GUI thread and the
//! (future) VR thread mutate it; both hold the write lock only for the
//! duration of the mutation.

use crate::data::{
    base_slots, DataIndex, GameData, ItemId, ModuleId, RecipeOverride, Requirement, ResearchNodeId,
    UpgradeId, RECIPE_SLOTS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const OVERRIDES_SCHEMA_VERSION: u32 = 1;

pub type ContainerId = String;

/// Fixed id of the built-in **Gunsmith storage** container. It's a *primary*
/// like the stash — the game gives every player exactly one (Gunsmith →
/// Storage, 30 kg cap) — so it's seeded once per profile with this well-known
/// id ([`AppState::seed_gunsmith_storage`]) and pinned in the Containers tab's
/// Primary section with no Edit/Delete. Minted ids are `ctr-<n>`, so this can
/// never collide.
pub const GUNSMITH_STORAGE_ID: &str = "gunsmith-storage";

/// What kind of secondary container this is. Only [`ContainerKind::Case`]
/// containers have an in-game contents screen, so only they support the
/// screenshot/OCR scan; bags and shelves are entered by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContainerKind {
    /// A backpack or pouch — no screen; contents entered manually.
    #[default]
    Bag,
    /// An item case with a contents screen — supports the scroll-and-merge OCR
    /// scan. Serialized as `"Case"`; the `"Box"` alias loads profiles written
    /// before this kind was renamed from `Box`.
    #[serde(alias = "Box")]
    Case,
    /// A shelf (hideout storage furniture) — declarative/manual like a bag, but
    /// its own category. One is seeded by default on first run.
    Shelf,
}

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
    /// Chosen container-icon key — a file stem under `assets/container_icons/`
    /// (e.g. `"backpack_rucksack"`). `None` → the UI shows a default bag icon.
    /// Pure presentation; the data layer never interprets it.
    #[serde(default)]
    pub icon: Option<String>,
    /// Whether this is a plain bag/case or a box with a scannable screen.
    /// Defaults to `Bag` so containers saved before this field existed load
    /// unchanged.
    #[serde(default)]
    pub kind: ContainerKind,
    /// Optional in-game weight cap in kg — the gunsmith storage and the junk
    /// boxes cap at 30. Display-only: the Containers KPI renders the weight as
    /// `Σ / cap kg` and warns as Σ approaches the cap; nothing is enforced
    /// (the game itself refuses over-cap deposits, so an over-cap tally here
    /// means the recorded counts drifted). `None` → plain weight, no cap.
    /// Defaults so profiles saved before this field existed load unchanged.
    #[serde(default)]
    pub capacity_kg: Option<f32>,
}

/// User-recorded progress of one research node. `Locked` / `Available` are
/// never stored — they're derived from the parents' `Developed` state at read
/// time ([`AppState::research_status`]); only the user-actioned stages persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResearchNodeState {
    /// Research started — samples outstanding (the game's "Need Research
    /// Samples").
    InProgress,
    /// Blueprint developed; the part is purchasable and children unlock.
    Developed,
}

/// Fully derived status of a research node, for display and gating. The
/// string each maps to in the UI is the game's own vocabulary
/// (`Gunsmith_StringTable` `Research_state_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResearchStatus {
    /// At least one parent isn't Developed yet ("Unknown Blueprint").
    Locked,
    /// Every parent developed, nothing recorded ("Ready For Research").
    Available,
    /// Recorded as started ("Need Research Samples").
    InProgress,
    /// Recorded as developed ("Developed").
    Developed,
}

pub struct AppState {
    pub data: Arc<GameData>,
    pub index: Arc<DataIndex>,
    pub tracked_upgrades: HashSet<UpgradeId>,
    pub completed_upgrades: HashSet<UpgradeId>,
    /// Upgrades the user has pinned to prioritize. Advisory metadata: pinned
    /// upgrades float to the top of the "By progress" list and their items lead
    /// the overlay / wishlist ordering. Filtered at read time to
    /// tracked-and-not-completed wherever it affects order, so a pin on an
    /// upgrade later completed or untracked sits inert (and re-applies if the
    /// upgrade is re-tracked) without any of the hot mutation paths having to
    /// clear it. Only `reset_all` + `PersistedState::merge_into` touch this set
    /// besides `set_pinned_upgrade`.
    pub pinned_upgrades: HashSet<UpgradeId>,
    pub collected: HashMap<ItemId, u32>,
    /// User-defined secondary containers (backpacks, item cases). Their
    /// contents sum with `collected` via [`AppState::owned_total`] for every
    /// "how many do I own?" computation. `collected` stays the implicit
    /// primary container and the target of the quick +/- / VR-click edits.
    pub containers: Vec<Container>,
    /// Monotonic id source for `containers`; persisted so a deleted
    /// container's id is never reused while anything still references it.
    pub next_container_seq: u64,
    /// Set once the built-in default Shelf has been created. Persisted so the
    /// shelf is seeded exactly once per profile (fresh *or* pre-existing) and a
    /// later deletion doesn't bring it back.
    pub default_shelf_seeded: bool,
    /// Set once the built-in Gunsmith storage has been created (see
    /// [`AppState::seed_gunsmith_storage`]). Same one-shot contract as
    /// [`Self::default_shelf_seeded`].
    pub gunsmith_storage_seeded: bool,
    /// Modules the user has marked as unavailable (e.g. quest-locked). Their
    /// tracked upgrades stay tracked but don't contribute to `active_items`,
    /// so the wishlist hides items the user can't act on yet.
    pub disabled_modules: HashSet<ModuleId>,
    /// User-supplied recipe corrections, keyed by upgrade id. Present entries
    /// fully replace the bundled recipe; absence falls through to `data.json`.
    pub overrides: HashMap<UpgradeId, RecipeOverride>,
    /// Recorded research progress per node (absent = not started). Locked is
    /// derived from parents at read time, never stored.
    pub research_states: HashMap<ResearchNodeId, ResearchNodeState>,
    /// Research nodes whose samples feed the overlay wishlist. Tracking is
    /// overlay-only, same stance as `tracked_upgrades` — nothing else gates on
    /// it. Unlike hideout upgrades a *locked* tracked node still contributes:
    /// samples are lootable long before the node itself unlocks (deep spine
    /// nodes even require parts unlocked by earlier nodes), and collecting
    /// ahead is the point of tracking. Developed nodes contribute nothing.
    pub tracked_research: HashSet<ResearchNodeId>,
    /// Research nodes the user has pinned to prioritize — the research-tab
    /// counterpart of [`Self::pinned_upgrades`]. Pinned-and-tracked nodes push
    /// their samples to the front of the overlay with the highlight accent.
    /// Same advisory contract as the upgrade set: filtered at read time to
    /// tracked-and-not-developed wherever it affects order (see
    /// [`Self::active_items`]), so a pin on a node later untracked or developed
    /// sits inert without any hot path having to clear it. Only `reset_all` +
    /// `PersistedState::merge_into` touch it besides `set_pinned_research`.
    pub pinned_research: HashSet<ResearchNodeId>,
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
            pinned_upgrades: HashSet::new(),
            collected: HashMap::new(),
            containers: Vec::new(),
            next_container_seq: 0,
            default_shelf_seeded: false,
            gunsmith_storage_seeded: false,
            disabled_modules: HashSet::new(),
            overrides: HashMap::new(),
            research_states: HashMap::new(),
            tracked_research: HashSet::new(),
            pinned_research: HashSet::new(),
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
    pub fn set_recipe_override(
        &mut self,
        upgrade_id: &UpgradeId,
        mut new_override: RecipeOverride,
    ) {
        if let Some(uref) = self.index.upgrades_by_id.get(upgrade_id) {
            if new_override.matches_base(&uref.upgrade) {
                self.overrides.remove(upgrade_id);
                self.bump();
                return;
            }
            // Stamp the hash of the bundled recipe this correction is based
            // on. If a future update changes the official recipe, the stored
            // hash stops matching and `PersistedOverrides::merge_into` discards
            // the correction so the official data wins — whether it filled a
            // previously-empty placeholder or fixed a wrong recipe. Recomputed
            // from the live bundled recipe every time, so re-editing on top of
            // a fresh dataset re-bases the correction.
            new_override.base_hash = Some(crate::data::recipe_hash(&uref.upgrade));
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

    /// Pin or unpin an upgrade to prioritize it. Pinned upgrades float to the
    /// top of the "By progress" list and lead the overlay / wishlist item order.
    ///
    /// The `bump()` is load-bearing: pinning reorders [`Self::active_items`], and
    /// the VR overlay only rebuilds its texture + click hit-table when `version`
    /// changes. Without the bump a reorder would leave the cached hit rects
    /// pointing at stale cells, so clicks would land on the wrong item. We bump
    /// only on a real change so a redundant toggle doesn't force a re-render.
    pub fn set_pinned_upgrade(&mut self, id: &UpgradeId, on: bool) {
        let changed = if on {
            self.pinned_upgrades.insert(id.clone())
        } else {
            self.pinned_upgrades.remove(id)
        };
        if changed {
            self.bump();
        }
    }

    pub fn is_upgrade_pinned(&self, id: &UpgradeId) -> bool {
        self.pinned_upgrades.contains(id)
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
    /// True if this upgrade is not yet completed and the user has collected
    /// enough of every required item to claim it — whether or not it's tracked.
    /// A fully-stocked upgrade therefore reads as ready in the grid even when
    /// it isn't on the wishlist (you can claim it in-game either way); tracking
    /// only governs the VR overlay's per-item wishlist. Ignores whether
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
        if self.completed_upgrades.contains(upgrade_id) {
            return false;
        }
        let reqs = self.effective_requirements(upgrade_id);
        if reqs.is_empty() {
            return false;
        }
        reqs.iter()
            .all(|r| self.owned_total(&r.item_id) >= r.quantity)
    }

    /// True iff every item in this upgrade's *effective* recipe is currently
    /// stocked to at least its required quantity across the *whole* inventory —
    /// stash (`collected`) plus every secondary container, via
    /// [`Self::owned_total`] — i.e. we could subtract the whole recipe without
    /// flooring anything at zero. Drives the "Consume items required" choice in
    /// the upgrade-completion modal: disabled when this is false because you
    /// can't burn materials you don't have. Counting containers (not just the
    /// stash) keeps this in lockstep with [`Self::is_upgrade_ready`]: an upgrade
    /// that reads green because container stock covers it can also be consumed,
    /// with [`Self::complete_upgrade`] draining the stash first and falling back
    /// to containers for the remainder. Empty recipes return `false` (mirrors
    /// [`Self::is_upgrade_ready`]): an unknown/placeholder recipe has nothing
    /// meaningful to consume.
    pub fn can_consume_materials(&self, upgrade_id: &UpgradeId) -> bool {
        let reqs = self.effective_requirements(upgrade_id);
        if reqs.is_empty() {
            return false;
        }
        reqs.iter()
            .all(|r| self.owned_total(&r.item_id) >= r.quantity)
    }

    /// Mark an upgrade completed, optionally burning its recipe's items from the
    /// inventory first to mirror the in-game material cost.
    ///
    /// Building an upgrade in-game removes the required items from wherever
    /// they're stored. When the user confirms a *manual* upgrade with
    /// `consume = true` we do the same here, keeping the app's inventory in
    /// lockstep with the game so the wishlist doesn't keep counting spent items
    /// toward other upgrades. Consumption drains the stash (`collected`) first
    /// and falls back to secondary containers — boxes before shelves/bags — for
    /// any shortfall (see `consume_from_inventory`), so a recipe the user can
    /// only afford by counting container stock still subtracts cleanly.
    /// `consume = false`
    /// leaves the inventory untouched — the "skip" modal choice, and the path
    /// the OCR worker uses (it reads true post-build stash counts off the
    /// screen, so consuming again would double-subtract).
    ///
    /// Consumption clamps at zero overall; the modal only offers
    /// `consume = true` when [`Self::can_consume_materials`] holds, so in
    /// practice the full cost always comes out. Each `adjust_collected` /
    /// `adjust_container_item` / `set_completed_upgrade` call bumps `version`;
    /// the caller issues one `SaveTick` after.
    pub fn complete_upgrade(&mut self, upgrade_id: &UpgradeId, consume: bool) {
        if consume {
            for req in self.effective_requirements(upgrade_id) {
                self.consume_from_inventory(&req.item_id, req.quantity);
            }
        }
        self.set_completed_upgrade(upgrade_id, true);
    }

    /// Subtract `qty` of `item` from the inventory, draining the stash
    /// (`collected`) first and then the secondary containers — **boxes (item
    /// cases) before shelves and bags**, declaration order within a tier — until
    /// the quantity is covered or the inventory runs dry. Stash-first keeps the
    /// scannable, game-synced stash as the primary store; the
    /// box-before-shelves/bags order is the user's chosen container-fallback
    /// priority (see [`Self::container_drain_rank`]). Clamps at zero per store,
    /// so a short inventory just empties what's there rather than going
    /// negative — callers gate on [`Self::can_consume_materials`] when the full
    /// cost must come out. Reuses [`Self::adjust_collected`] /
    /// [`Self::adjust_container_item`], so each touched store drops emptied
    /// entries and bumps `version`.
    fn consume_from_inventory(&mut self, item: &ItemId, qty: u32) {
        let mut remaining = qty as i64;
        let from_stash = (*self.collected.get(item).unwrap_or(&0) as i64).min(remaining);
        if from_stash > 0 {
            self.adjust_collected(item, -from_stash);
            remaining -= from_stash;
        }
        if remaining <= 0 {
            return;
        }
        // Snapshot (id, kind) up front so the drain can call the mutating
        // `adjust_container_item` helper (which reuses the canonical remove-on-
        // zero + version-bump path) without holding a borrow of `self.containers`
        // across the call. Order boxes before shelves/bags; `sort_by_key` is
        // stable, so same-tier containers keep their declaration order.
        // Containers are few, so the clones are cheap.
        let mut order: Vec<(ContainerId, ContainerKind)> = self
            .containers
            .iter()
            .map(|c| (c.id.clone(), c.kind))
            .collect();
        order.sort_by_key(|(_, kind)| Self::container_drain_rank(*kind));
        for (id, _) in order {
            if remaining <= 0 {
                break;
            }
            let have = self
                .containers
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.contents.get(item).copied())
                .unwrap_or(0) as i64;
            let take = have.min(remaining);
            if take > 0 {
                self.adjust_container_item(&id, item, -take);
                remaining -= take;
            }
        }
    }

    /// Container-fallback drain priority for [`Self::consume_from_inventory`]:
    /// **boxes (item cases) drain before shelves and bags** (lower rank = drained
    /// first). Shelves and bags share one tier, so within it containers empty in
    /// declaration order. Pure classification; mutates nothing.
    fn container_drain_rank(kind: ContainerKind) -> u8 {
        match kind {
            ContainerKind::Case => 0,
            ContainerKind::Shelf | ContainerKind::Bag => 1,
        }
    }

    /// True when every parent of the node is recorded Developed (roots have no
    /// parents, so they're always unlocked). Unknown ids read as locked — the
    /// UI never offers actions on a node the dataset doesn't know.
    pub fn is_research_unlocked(&self, id: &ResearchNodeId) -> bool {
        let Some(node) = self.index.research_nodes_by_id.get(id) else {
            return false;
        };
        node.parents.iter().all(|p| {
            matches!(
                self.research_states.get(p),
                Some(ResearchNodeState::Developed)
            )
        })
    }

    /// Derived display/gating status: the recorded state when present, else
    /// Locked/Available from the parents. A recorded state wins even if the
    /// parents say locked — the user's explicit record reflects the game (e.g.
    /// state restored from an older session), and second-guessing it would
    /// fight them.
    pub fn research_status(&self, id: &ResearchNodeId) -> ResearchStatus {
        match self.research_states.get(id) {
            Some(ResearchNodeState::Developed) => ResearchStatus::Developed,
            Some(ResearchNodeState::InProgress) => ResearchStatus::InProgress,
            None => {
                if self.is_research_unlocked(id) {
                    ResearchStatus::Available
                } else {
                    ResearchStatus::Locked
                }
            }
        }
    }

    /// Track/untrack a research node on the overlay wishlist. Mirrors
    /// [`Self::set_tracked_upgrade`] — overlay-only, no gating side effects.
    pub fn set_tracked_research(&mut self, id: &ResearchNodeId, on: bool) {
        if on {
            self.tracked_research.insert(id.clone());
        } else {
            self.tracked_research.remove(id);
        }
        self.bump();
    }

    /// Pin or unpin a research node to prioritize its samples. Mirrors
    /// [`Self::set_pinned_upgrade`]: the pin only bites while the node is also
    /// tracked-and-not-developed (resolved in [`Self::active_items`]), and the
    /// `bump()` is load-bearing — pinning reorders the overlay, whose cached
    /// click hit-table only rebuilds when `version` changes. Bumps only on a
    /// real change so a redundant toggle doesn't force a re-render.
    pub fn set_pinned_research(&mut self, id: &ResearchNodeId, on: bool) {
        let changed = if on {
            self.pinned_research.insert(id.clone())
        } else {
            self.pinned_research.remove(id)
        };
        if changed {
            self.bump();
        }
    }

    pub fn is_research_pinned(&self, id: &ResearchNodeId) -> bool {
        self.pinned_research.contains(id)
    }

    /// Record a node's research progress (`None` = back to not-started).
    ///
    /// Recording **Developed** drops the node from the tracked set — there's
    /// nothing left to collect for it. It deliberately does NOT auto-track the
    /// blueprints it unlocks: pulling things onto the wishlist behind the user's
    /// back was confusing, so what to pursue next is always an explicit choice
    /// (the Track checkbox or [`Self::focus_research`]). The node's pin is left
    /// in place but inert — [`Self::active_items`] only honors a pin while the
    /// node is tracked-and-not-developed, mirroring [`Self::set_pinned_upgrade`].
    pub fn set_research_state(&mut self, id: &ResearchNodeId, state: Option<ResearchNodeState>) {
        match state {
            Some(s) => {
                self.research_states.insert(id.clone(), s);
            }
            None => {
                self.research_states.remove(id);
            }
        }
        if matches!(state, Some(ResearchNodeState::Developed)) {
            self.tracked_research.remove(id);
        }
        self.bump();
    }

    /// "Focus" a target blueprint: track **and** pin it plus every prerequisite
    /// still standing between the user and it, so the whole route's samples lead
    /// the overlay. Walks the parent DAG transitively (the data layer guarantees
    /// acyclicity, but a `seen` set guards regardless) and stops at any branch
    /// already **Developed** — a developed node is done, and its own parents are
    /// necessarily developed too, so there's nothing left to collect up that
    /// line. A focus whose every node is already developed is a no-op.
    pub fn focus_research(&mut self, id: &ResearchNodeId) {
        let mut to_focus: Vec<ResearchNodeId> = Vec::new();
        let mut seen: HashSet<ResearchNodeId> = HashSet::new();
        let mut stack = vec![id.clone()];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if matches!(
                self.research_states.get(&cur),
                Some(ResearchNodeState::Developed)
            ) {
                continue;
            }
            let Some(node) = self.index.research_nodes_by_id.get(&cur) else {
                continue;
            };
            for parent in &node.parents {
                stack.push(parent.clone());
            }
            to_focus.push(cur);
        }
        if to_focus.is_empty() {
            return;
        }
        for nid in to_focus {
            self.tracked_research.insert(nid.clone());
            self.pinned_research.insert(nid);
        }
        self.bump();
    }

    /// True when the inventory covers every sample of the node's requirement
    /// list — the gate for the "consume samples" completion path. Mirrors
    /// [`Self::can_consume_materials`]; empty sample lists return `false`.
    pub fn can_consume_research_samples(&self, id: &ResearchNodeId) -> bool {
        let Some(node) = self.index.research_nodes_by_id.get(id) else {
            return false;
        };
        if node.samples.is_empty() {
            return false;
        }
        node.samples
            .iter()
            .all(|r| self.owned_total(&r.item_id) >= r.quantity)
    }

    /// Mark a node Developed, optionally burning its samples from the
    /// inventory first — submitting samples in-game consumes them, so
    /// `consume = true` keeps the app's counts in lockstep (stash first, then
    /// containers, exactly like [`Self::complete_upgrade`]). `consume = false`
    /// is the "already submitted them ages ago" path.
    pub fn complete_research(&mut self, id: &ResearchNodeId, consume: bool) {
        if consume {
            let samples = self
                .index
                .research_nodes_by_id
                .get(id)
                .map(|n| n.samples.clone())
                .unwrap_or_default();
            for req in samples {
                self.consume_from_inventory(&req.item_id, req.quantity);
            }
        }
        self.set_research_state(id, Some(ResearchNodeState::Developed));
    }

    /// Collected-vs-needed rollup over a node's samples, per-item capped —
    /// same shape and caveats as [`Self::upgrade_progress`].
    pub fn research_progress(&self, id: &ResearchNodeId) -> UpgradeProgress {
        let mut collected = 0;
        let mut needed = 0;
        if let Some(node) = self.index.research_nodes_by_id.get(id) {
            for req in &node.samples {
                let have = self.owned_total(&req.item_id);
                collected += have.min(req.quantity);
                needed += req.quantity;
            }
        }
        UpgradeProgress { collected, needed }
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

    /// How far one upgrade's *effective* recipe is from claimable: how many
    /// distinct items are still below their target, and the total units still
    /// missing across them. Same recipe walk as [`Self::upgrade_progress`]
    /// (counts containers via `owned_total`) but answers "how far from done?"
    /// instead of "how complete?". Uses `<` so an item stocked to exactly its
    /// target is NOT counted missing — mirroring [`Self::is_upgrade_ready`]'s
    /// `>=`. An empty recipe yields all-zero: "we don't know the cost", not
    /// "0 away" (same stance the readiness/knowledge helpers take).
    pub fn upgrade_shortfall(&self, upgrade_id: &UpgradeId) -> UpgradeShortfall {
        let mut items_missing = 0;
        let mut units_missing = 0;
        for req in self.effective_requirements(upgrade_id) {
            let have = self.owned_total(&req.item_id);
            if have < req.quantity {
                items_missing += 1;
                units_missing += req.quantity - have;
            }
        }
        UpgradeShortfall {
            items_missing,
            units_missing,
        }
    }

    /// How much we trust an upgrade's recipe (drives the confidence badge in
    /// the Hideout views). `Unknown` wins whenever the effective recipe is
    /// empty — "we don't know the cost" is the dominant signal, the same
    /// reason `is_upgrade_ready` refuses to flash empty recipes green.
    pub fn recipe_knowledge(&self, upgrade_id: &UpgradeId) -> RecipeKnowledge {
        if self.effective_requirements(upgrade_id).is_empty() {
            RecipeKnowledge::Unknown
        } else if self.is_overridden(upgrade_id) {
            RecipeKnowledge::Edited
        } else {
            RecipeKnowledge::Bundled
        }
    }

    /// One row per hideout module: its lowest not-yet-built level — the upgrade
    /// you'd naturally build next, since levels are bought in strict order. This
    /// is the whole hideout's natural progression, deliberately INDEPENDENT of
    /// tracking and of module-disabled state. Tracking only governs the overlay's
    /// per-item wishlist (`active_items`); it does not decide what shows up ranked
    /// here. A module whose every level is already built contributes no row.
    ///
    /// Pinned upgrades float above everything else — a deliberate user priority
    /// ("show my goals first"). Within each pin group, ordering buckets:
    ///   0. Ready — every material collected, recipe known. Float to the top.
    ///   1. Nearly ready — 1–2 distinct items short of claimable; within the
    ///      band, fewest items then fewest units first (genuinely closest leads).
    ///   2. In progress (some collected, 3+ items short), descending by fraction
    ///      so the nearest-to-done sits highest.
    ///   3. Known recipe, nothing collected yet.
    ///   4. Unknown (empty recipe) — parked last and flagged, because its
    ///      fraction is a meaningless 0 and the action there is "fill in the
    ///      recipe", not "go grind".
    ///
    /// Tiebreak within a bucket: module name, then level, for a stable order
    /// that doesn't reshuffle on unrelated clicks.
    pub fn hideout_progress_rows(&self) -> Vec<UpgradeProgressRow> {
        // Mirrors the `AllNatural` horizon exactly (same module filter + same
        // `natural_next_upgrade`), so this ranked list and the per-item "natural"
        // need view never disagree on what each module's next build is. Disabled
        // (locked) modules are hidden, same as the wishlist.
        let mut rows: Vec<UpgradeProgressRow> = self
            .data
            .modules
            .iter()
            .filter(|module| !self.is_module_effectively_disabled(&module.id))
            .filter_map(|module| {
                let next = self.natural_next_upgrade(module)?;
                let id = &next.id;
                Some(UpgradeProgressRow {
                    upgrade_id: id.clone(),
                    module_name: module.name.clone(),
                    level: next.level,
                    progress: self.upgrade_progress(id),
                    knowledge: self.recipe_knowledge(id),
                    ready: self.is_upgrade_ready(id),
                    pinned: self.pinned_upgrades.contains(id),
                    shortfall: self.upgrade_shortfall(id),
                })
            })
            .collect();

        // Per row: (bucket rank, within-bucket key). Lower rank sorts first.
        // Keying on an explicit bucket (rather than raw fraction) keeps "ready"
        // / "nearly" / "unknown" from interleaving with the in-progress band —
        // an unknown-recipe row and a known-but-untouched one both have fraction
        // 0 but belong at opposite ends. The within-bucket f64 is only ever
        // compared against rows in the SAME bucket (rank separates them first),
        // so each branch can pick whatever ordering fits that band: ascending
        // "distance" for nearly (fewer items, then fewer units), and inverse
        // fraction for in-progress (higher completion first).
        fn sort_key(r: &UpgradeProgressRow) -> (u8, f64) {
            if matches!(r.knowledge, RecipeKnowledge::Unknown) {
                (4, 0.0)
            } else if r.ready {
                (0, 0.0)
            } else if (1..=2).contains(&r.shortfall.items_missing) {
                // Nearly: items dominate, units break ties (items can't realistically
                // exceed the 4-slot recipe, so the 1e9 scale won't collide).
                (
                    1,
                    r.shortfall.items_missing as f64 * 1.0e9 + r.shortfall.units_missing as f64,
                )
            } else if r.progress.collected > 0 {
                (2, 1.0 - r.progress.fraction() as f64)
            } else {
                (3, 0.0)
            }
        }

        rows.sort_by(|a, b| {
            let (ra, ka) = sort_key(a);
            let (rb, kb) = sort_key(b);
            // Pinned upgrades float above everything else (b.pinned vs a.pinned
            // puts `true` first), then the readiness bucket, then within-bucket
            // order. total_cmp keeps the f64 compare panic-free (house caution).
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| ra.cmp(&rb))
                .then_with(|| ka.total_cmp(&kb))
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
    /// containers are edited from the Containers tab — or drained as a
    /// consumption fallback once the stash runs short (see
    /// [`Self::complete_upgrade`]).
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
            icon: None,
            kind: ContainerKind::default(),
            capacity_kg: None,
        });
        self.bump();
        id
    }

    /// Set (or clear, with `None`) a container's chosen icon key. No-op if the
    /// id is unknown or the icon is unchanged.
    pub fn set_container_icon(&mut self, id: &ContainerId, icon: Option<String>) {
        if let Some(c) = self.containers.iter_mut().find(|c| &c.id == id) {
            if c.icon != icon {
                c.icon = icon;
                self.bump();
            }
        }
    }

    /// Set a container's kind (bag vs box). No-op if the id is unknown or
    /// unchanged.
    pub fn set_container_kind(&mut self, id: &ContainerId, kind: ContainerKind) {
        if let Some(c) = self.containers.iter_mut().find(|c| &c.id == id) {
            if c.kind != kind {
                c.kind = kind;
                self.bump();
            }
        }
    }

    /// Seed the built-in **Gunsmith storage** primary container exactly once
    /// per profile (fresh *or* pre-existing — same contract as the default
    /// Shelf). The game gives every player exactly one Gunsmith → Storage with
    /// a 30 kg cap, so it arrives ready to scan: fixed id, `Case` kind, the
    /// gunsmith icon, and the cap baked in. Returns whether it seeded (so the
    /// caller can persist synchronously, before the save loop exists).
    pub fn seed_gunsmith_storage(&mut self) -> bool {
        if self.gunsmith_storage_seeded {
            return false;
        }
        self.containers.push(Container {
            id: GUNSMITH_STORAGE_ID.to_string(),
            name: "Gunsmith storage".to_string(),
            contents: HashMap::new(),
            icon: Some("box_gunsmith".to_string()),
            kind: ContainerKind::Case,
            capacity_kg: Some(30.0),
        });
        self.gunsmith_storage_seeded = true;
        self.bump();
        true
    }

    /// Set (or clear, with `None`) a container's weight cap in kg. No-op if
    /// the id is unknown or the value is unchanged.
    pub fn set_container_capacity(&mut self, id: &ContainerId, capacity_kg: Option<f32>) {
        if let Some(c) = self.containers.iter_mut().find(|c| &c.id == id) {
            if c.capacity_kg != capacity_kg {
                c.capacity_kg = capacity_kg;
                self.bump();
            }
        }
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
        // Clear pins too — otherwise a zombie pin would silently re-attach if
        // the user re-tracks the same upgrade id under the same data version.
        self.pinned_upgrades.clear();
        self.collected.clear();
        // Clear secondary containers too — "Reset progress" wipes the whole
        // inventory. `next_container_seq` stays monotonic (not reset) so a
        // recreated container can never reuse a just-cleared id.
        self.containers.clear();
        self.disabled_modules.clear();
        self.research_states.clear();
        self.tracked_research.clear();
        // Clear research pins for the same reason as `pinned_upgrades` above —
        // a zombie pin would otherwise silently re-attach on re-track.
        self.pinned_research.clear();
        self.bump();
    }

    /// Aggregate every tracked-but-not-completed upgrade into a flat per-item
    /// view.
    pub fn active_items(&self) -> Vec<ActiveItem> {
        // Per item: (needed units, source labels, pinned-needed units). The
        // last field sums how many of this item the user's *pinned* upgrades
        // collectively call for; it's resolved to the `pinned` flag below, once
        // the owned total is known, as `owned < pinned_needed`. Summing across
        // pinned upgrades (rather than testing each in isolation) is what makes
        // two pinned upgrades that share an item behave correctly: the item
        // stays pinned until you own enough to satisfy BOTH at once — sum(reqs),
        // not max(reqs) — and clears the moment the combined pinned demand is
        // met, even while OTHER, unpinned tracked upgrades still want more.
        // Pinning one upgrade therefore no longer lights up materials it already
        // has enough of (the reported surprise). The loop iterates `tracked −
        // completed`, so a pin on a completed upgrade contributes nothing here.
        let mut totals: BTreeMap<ItemId, (u32, Vec<String>, u32)> = BTreeMap::new();

        for id in self.tracked_upgrades.difference(&self.completed_upgrades) {
            let Some(uref) = self.index.upgrades_by_id.get(id) else {
                continue;
            };
            if self.is_module_effectively_disabled(&uref.module_id) {
                continue;
            }
            let pinned = self.pinned_upgrades.contains(id);
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
                    .or_insert_with(|| (0, Vec::new(), 0));
                entry.0 += req.quantity;
                entry.1.push(label.clone());
                // Accumulate the pinned demand for this item; the flag is
                // derived from the sum vs the owned total below.
                if pinned {
                    entry.2 += req.quantity;
                }
            }
        }

        // Tracked research nodes contribute their sample lists. Developed
        // nodes are spent (mirroring `tracked − completed` above); locked ones
        // still count — samples are lootable long before the node unlocks,
        // and collecting ahead is what tracking a research node is *for*.
        // A pinned-and-tracked node feeds the same pinned-demand accumulator as
        // upgrades, so its samples float to the front of the overlay with the
        // highlight accent. A pin on an untracked/developed node never enters
        // this loop, so it stays inert without anything having to clear it.
        for id in &self.tracked_research {
            if matches!(
                self.research_states.get(id),
                Some(ResearchNodeState::Developed)
            ) {
                continue;
            }
            let Some(node) = self.index.research_nodes_by_id.get(id) else {
                continue;
            };
            let pinned = self.pinned_research.contains(id);
            let label = format!("Research: {}", node.name);
            for req in &node.samples {
                let entry = totals
                    .entry(req.item_id.clone())
                    .or_insert_with(|| (0, Vec::new(), 0));
                entry.0 += req.quantity;
                entry.1.push(label.clone());
                if pinned {
                    entry.2 += req.quantity;
                }
            }
        }

        let mut out: Vec<ActiveItem> = totals
            .into_iter()
            .filter_map(|(item_id, (needed, sources, pinned_needed))| {
                let item = self.index.items_by_id.get(&item_id)?;
                let collected = self.owned_total(&item_id);
                // Pinned iff the pinned upgrades, taken together, still need more
                // of this item than is owned. `<` (not `<=`): owning exactly the
                // combined pinned demand counts as satisfied, so the accent
                // clears — mirroring `is_upgrade_ready` and the struck-through
                // per-upgrade rows. Items no pinned upgrade needs have
                // `pinned_needed == 0`, so this is `collected < 0` → never
                // pinned.
                let pinned = collected < pinned_needed;
                Some(ActiveItem {
                    item_id,
                    name: item.name.clone(),
                    icon_path: item.icon_path.clone(),
                    needed,
                    collected,
                    sources,
                    pinned,
                })
            })
            .collect();

        // Order:
        //   1. Done items (collected ≥ needed) last so the user's eye lands on
        //      what still needs gathering. This stays the OUTERMOST key, so a
        //      pinned item you've already finished sinks rather than squatting
        //      top-left in the overlay.
        //   2. Among the not-done, pinned-sourced items first — a deliberate
        //      user priority (pinning/unpinning bumps `version`, one overlay
        //      re-render). An item is pinned-sourced only while the pinned
        //      upgrades together still need more of it than is owned (see the
        //      aggregation above), so collecting it up to that combined demand
        //      drops it out of the pinned band. That's a single reshuffle at the
        //      boundary — the same shape as bullet 1's "done sinks", at an
        //      equally meaningful moment (the pins no longer need that item).
        //   3. Then descending NEEDED quantity so the biggest grinds sit
        //      top-left. We deliberately key on `needed` rather than
        //      `needed - collected` — keying on the remaining count makes the
        //      grid reshuffle on every click in the VR overlay, which is
        //      disorienting.
        //   4. Tiebreak alphabetically by name for stability when two items
        //      share a target quantity.
        out.sort_by(|a, b| {
            let a_done = a.collected >= a.needed;
            let b_done = b.collected >= b.needed;
            a_done
                .cmp(&b_done)
                .then_with(|| b.pinned.cmp(&a.pinned)) // pinned (true) first
                .then_with(|| b.needed.cmp(&a.needed)) // descending
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// The lowest-level not-yet-completed upgrade in a module — the "natural"
    /// next build there, since hideout levels are claimed in strict order.
    /// `None` once every level is done. Shared by the `AllNatural` need horizon
    /// and the "By progress" view so the two always agree on what's next.
    fn natural_next_upgrade<'a>(
        &self,
        module: &'a crate::data::HideoutModule,
    ) -> Option<&'a crate::data::Upgrade> {
        module
            .upgrades
            .iter()
            .filter(|u| !self.completed_upgrades.contains(&u.id))
            .min_by_key(|u| u.level)
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
                .filter_map(|m| self.natural_next_upgrade(m))
                .map(|u| u.id.clone())
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
    /// [`Self::effective_requirements`].
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
    /// True while the user's pinned upgrades, taken together, still need more of
    /// this item than is owned (`owned < Σ pinned reqs`). Floats the item to the
    /// front of the overlay/wishlist order and lets the desktop preview tag it.
    /// Clears once the combined pinned demand is met, so a pin never flags a
    /// material it already has enough of — even if unpinned upgrades want more.
    pub pinned: bool,
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

/// How far one upgrade's effective recipe is from claimable: distinct items
/// still under their target, and total units still missing across them. The
/// "nearly ready" tier keys on `items_missing` (1–2 ⇒ nearly); the By-progress
/// row shows both so a "1 item, 100 units to go" case stays honest. An empty /
/// unknown recipe yields all-zero (unknown cost, not "0 away").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpgradeShortfall {
    /// Distinct items whose owned total is below the required quantity.
    pub items_missing: u32,
    /// Σ (required − owned) across those items.
    pub units_missing: u32,
}

/// How much we trust an upgrade's recipe — drives the confidence badge in the
/// Hideout views. Three-way so "we shipped a recipe the user then corrected"
/// reads differently from "we shipped it as-is".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeKnowledge {
    /// Effective recipe is empty — we don't know the cost. Parked + flagged.
    Unknown,
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
    /// `is_upgrade_ready`: every material collected, recipe known (not
    /// tracking-gated).
    pub ready: bool,
    /// User-pinned: floats this row above its readiness bucket and paints a
    /// priority accent. Carried on the row so the GUI never re-locks per row.
    pub pinned: bool,
    /// Distinct items / units still missing. Drives the "nearly ready" sort
    /// tier (1–2 items_missing floats a row just below the ready ones).
    pub shortfall: UpgradeShortfall,
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
    /// Pinned upgrades (priority ordering). `#[serde(default)]` so a
    /// pre-pinning `state.json` (no key) loads as an empty set — the same
    /// forward-compat path `containers` uses, no schema bump.
    #[serde(default)]
    pub pinned_upgrades: HashSet<UpgradeId>,
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
    /// Whether the built-in default Shelf has been seeded. `#[serde(default)]`
    /// → a `state.json` predating this feature loads as `false`, so the shelf
    /// is created once on the next launch.
    #[serde(default)]
    pub default_shelf_seeded: bool,
    /// Whether the built-in Gunsmith storage has been seeded — same
    /// forward-compat path as `default_shelf_seeded`.
    #[serde(default)]
    pub gunsmith_storage_seeded: bool,
    /// Recorded research progress. `#[serde(default)]` so a pre-research
    /// `state.json` loads as "nothing recorded" — same path as `containers`.
    #[serde(default)]
    pub research_states: HashMap<ResearchNodeId, ResearchNodeState>,
    #[serde(default)]
    pub tracked_research: HashSet<ResearchNodeId>,
    /// Pinned research nodes (priority ordering). `#[serde(default)]` so a
    /// `state.json` predating research pins loads as an empty set — same
    /// forward-compat path as `pinned_upgrades`, no schema bump.
    #[serde(default)]
    pub pinned_research: HashSet<ResearchNodeId>,
}

impl PersistedState {
    pub fn from_app(state: &AppState) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            data_version: state.data.data_version.clone(),
            tracked_upgrades: state.tracked_upgrades.clone(),
            completed_upgrades: state.completed_upgrades.clone(),
            pinned_upgrades: state.pinned_upgrades.clone(),
            collected: state.collected.clone(),
            disabled_modules: state.disabled_modules.clone(),
            containers: state.containers.clone(),
            next_container_seq: state.next_container_seq,
            default_shelf_seeded: state.default_shelf_seeded,
            gunsmith_storage_seeded: state.gunsmith_storage_seeded,
            research_states: state.research_states.clone(),
            tracked_research: state.tracked_research.clone(),
            pinned_research: state.pinned_research.clone(),
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

        let (kept_upgrades, dropped_upgrades) =
            filter_known(self.tracked_upgrades, |id| upgrades_index.contains_key(id));
        let (kept_done_upgrades, _) = filter_known(self.completed_upgrades, |id| {
            upgrades_index.contains_key(id)
        });
        // Pins are advisory metadata — prune ids that no longer resolve, but
        // silently (no user-facing warning the way dropped tracked upgrades get
        // one). A lost pin costs the user nothing; warning about it would be
        // noise.
        let (kept_pinned_upgrades, _) =
            filter_known(self.pinned_upgrades, |id| upgrades_index.contains_key(id));

        if dropped_upgrades > 0 {
            warnings.push(format!(
                "Dropped {dropped_upgrades} tracked upgrade(s) no longer present in data."
            ));
        }

        // Research mirrors the upgrade treatment: warn about dropped tracked
        // nodes (the user chose those), prune recorded states silently (a
        // vanished node's state is meaningless, losing it costs nothing).
        let research_index = &state.index.research_nodes_by_id;
        let (kept_tracked_research, dropped_research) =
            filter_known(self.tracked_research, |id| research_index.contains_key(id));
        if dropped_research > 0 {
            warnings.push(format!(
                "Dropped {dropped_research} tracked research node(s) no longer present in data."
            ));
        }
        let kept_research_states: HashMap<ResearchNodeId, ResearchNodeState> = self
            .research_states
            .into_iter()
            .filter(|(id, _)| research_index.contains_key(id))
            .collect();
        // Research pins prune silently, exactly like `pinned_upgrades`.
        let (kept_pinned_research, _) =
            filter_known(self.pinned_research, |id| research_index.contains_key(id));

        state.tracked_upgrades = kept_upgrades;
        state.completed_upgrades = kept_done_upgrades;
        state.pinned_upgrades = kept_pinned_upgrades;
        state.tracked_research = kept_tracked_research;
        state.research_states = kept_research_states;
        state.pinned_research = kept_pinned_research;
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
        state.default_shelf_seeded = self.default_shelf_seeded;
        state.gunsmith_storage_seeded = self.gunsmith_storage_seeded;
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
        let mut superseded = 0usize;
        for (id, ov) in self.overrides {
            match state.index.upgrades_by_id.get(&id) {
                None => dropped += 1,
                // The official recipe has changed since the user made this
                // correction (stored base hash no longer matches the bundled
                // recipe). Discard the local correction — the authoritative
                // data supersedes it. Covers both a once-empty placeholder
                // that's since been filled in and a wrong recipe that's since
                // been fixed. A correction whose base still matches is kept, as
                // is a legacy correction with no stored hash (`None`): we can't
                // tell whether its base changed, so we keep the user's data.
                Some(uref)
                    if ov
                        .base_hash
                        .is_some_and(|h| h != crate::data::recipe_hash(&uref.upgrade)) =>
                {
                    superseded += 1;
                }
                Some(_) => {
                    kept.insert(id, ov);
                }
            }
        }
        if dropped > 0 {
            warnings.push(format!(
                "Dropped {dropped} recipe override(s) for upgrade(s) no longer present."
            ));
        }
        if superseded > 0 {
            warnings.push(format!(
                "Discarded {superseded} recipe correction(s) now covered by the official dataset."
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
            research: Vec::new(),
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
                // the progress/knowledge tests exercise the `Unknown` paths
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
    fn container_capacity_persists_and_defaults_to_none() {
        let mut s = AppState::new(fixture());
        let id = s.create_container("Gunsmith storage".into());
        s.set_container_capacity(&id, Some(30.0));

        let json = serde_json::to_string(&PersistedState::from_app(&s)).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();
        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert_eq!(s2.containers[0].capacity_kg, Some(30.0));

        // A container serialized before the field existed loads with no cap.
        let legacy = r#"{"id":"ctr-9","name":"Old box","contents":{}}"#;
        let old: Container = serde_json::from_str(legacy).unwrap();
        assert_eq!(old.capacity_kg, None);

        // Clearing the cap sticks.
        s.set_container_capacity(&id, None);
        assert_eq!(s.containers[0].capacity_kg, None);
    }

    #[test]
    fn gunsmith_storage_seeds_exactly_once_and_stays_deleted() {
        let mut s = AppState::new(fixture());
        assert!(s.seed_gunsmith_storage(), "fresh profile seeds");
        assert!(!s.seed_gunsmith_storage(), "second call is a no-op");
        let g = s
            .containers
            .iter()
            .find(|c| c.id == GUNSMITH_STORAGE_ID)
            .expect("seeded container present");
        assert_eq!(g.kind, ContainerKind::Case);
        assert_eq!(g.capacity_kg, Some(30.0));

        // The flag round-trips, so a (programmatic) deletion doesn't come back
        // on the next launch — same contract as the default Shelf.
        s.delete_container(&GUNSMITH_STORAGE_ID.to_string());
        let json = serde_json::to_string(&PersistedState::from_app(&s)).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();
        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert!(!s2.seed_gunsmith_storage(), "flag survives the round-trip");
        assert!(s2.containers.iter().all(|c| c.id != GUNSMITH_STORAGE_ID));
    }

    #[test]
    fn loads_pre_73_state_with_task_fields_without_error() {
        // Quest-task tracking was removed in #73. A `state.json` written by an
        // older build still carries `tracked_tasks` / `completed_tasks` keys.
        // `PersistedState` doesn't set `deny_unknown_fields`, so serde must
        // silently ignore them — no crash, no migration — while hideout
        // progress and collected counts survive intact.
        let legacy = r#"{
            "schema_version": 1,
            "data_version": "test",
            "tracked_upgrades": ["workbench_lv1"],
            "completed_upgrades": [],
            "tracked_tasks": ["ark_11"],
            "completed_tasks": ["ark_07"],
            "collected": {"bolts": 4}
        }"#;
        let parsed: PersistedState =
            serde_json::from_str(legacy).expect("legacy state with task fields must still parse");

        let mut s = AppState::new(fixture());
        let warn = parsed.merge_into(&mut s);
        assert!(
            warn.is_none(),
            "matching data_version + resolvable upgrade should not warn, got {warn:?}"
        );
        assert!(s.tracked_upgrades.contains("workbench_lv1"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 4);
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
            crate::data::RecipeOverride::new(slots),
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
            crate::data::RecipeOverride::new(slots),
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
                    crate::data::RecipeOverride::new(slots.clone()),
                ),
                (
                    "ghost_upgrade".to_string(),
                    crate::data::RecipeOverride::new(slots),
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
    fn set_recipe_override_stamps_base_hash() {
        // Committing a correction records the hash of the bundled recipe it was
        // based on, so a later dataset change can be detected.
        let mut s = AppState::new(fixture());
        let mut slots: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
        slots[0] = Some(Requirement {
            item_id: "bolts".into(),
            quantity: 2,
        });
        s.set_recipe_override(&"workbench_lv1".to_string(), RecipeOverride::new(slots));

        let expected =
            crate::data::recipe_hash(&s.index.upgrades_by_id.get("workbench_lv1").unwrap().upgrade);
        assert_eq!(
            s.overrides.get("workbench_lv1").unwrap().base_hash,
            Some(expected),
            "the bundled recipe's hash should be stamped on the correction",
        );
    }

    #[test]
    fn merge_discards_correction_when_official_recipe_changed() {
        // The headline scenario, generalized: the user corrected a recipe, then
        // an app update changed the official recipe for that upgrade. The stored
        // base hash no longer matches, so the now-stale correction is discarded
        // and the official data wins. A correction whose base is unchanged
        // survives. Covers both a once-empty placeholder that got filled in and
        // a wrong recipe that got fixed — both are just "the base changed".
        let mut s1 = AppState::new(fixture());
        let mut slots: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
        slots[0] = Some(Requirement {
            item_id: "bolts".into(),
            quantity: 1,
        });
        s1.set_recipe_override(
            &"workbench_lv1".to_string(),
            RecipeOverride::new(slots.clone()),
        );
        s1.set_recipe_override(&"workbench_lv2".to_string(), RecipeOverride::new(slots));
        let persisted = PersistedOverrides::from_app(&s1);

        // Simulate an app update: workbench_lv1's official recipe changes;
        // workbench_lv2's stays exactly as the correction was based on.
        let mut data2 = (*fixture()).clone();
        for m in &mut data2.modules {
            for u in &mut m.upgrades {
                if u.id == "workbench_lv1" {
                    u.requirements[0].quantity = 6; // was 5
                }
            }
        }
        let mut s2 = AppState::new(Arc::new(data2));

        let warn = persisted.merge_into(&mut s2).expect("should warn");
        assert!(
            warn.contains("Discarded 1 recipe correction"),
            "the superseded correction should be reported: {warn}",
        );
        assert!(
            !s2.overrides.contains_key("workbench_lv1"),
            "a correction whose official recipe changed must be discarded",
        );
        assert!(
            s2.overrides.contains_key("workbench_lv2"),
            "a correction whose official recipe is unchanged must survive",
        );
    }

    #[test]
    fn merge_keeps_correction_when_base_unchanged() {
        // No dataset change between save and load → every correction's base
        // hash still matches, so all are kept with no warning. Includes a
        // gap-fill on an empty placeholder (its empty base is unchanged).
        let mut s1 = AppState::new(fixture());
        let mut slots: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
        slots[0] = Some(Requirement {
            item_id: "bolts".into(),
            quantity: 4,
        });
        s1.set_recipe_override(
            &"placeholder_lv1".to_string(),
            RecipeOverride::new(slots.clone()),
        );
        s1.set_recipe_override(&"workbench_lv1".to_string(), RecipeOverride::new(slots));
        let persisted = PersistedOverrides::from_app(&s1);

        let mut s2 = AppState::new(fixture());
        assert!(
            persisted.merge_into(&mut s2).is_none(),
            "no warning expected"
        );
        assert!(s2.overrides.contains_key("placeholder_lv1"));
        assert!(s2.overrides.contains_key("workbench_lv1"));
    }

    #[test]
    fn merge_keeps_legacy_override_without_base_hash() {
        // Overrides written before `base_hash` existed deserialize with it
        // `None`. We can't know their original base, so we never auto-discard
        // them even when the bundled recipe has since changed — keeping the
        // user's data is the safe default.
        let mut s = AppState::new(fixture());
        let legacy_json = r#"{
            "schema_version": 1,
            "data_version": "older",
            "overrides": {
                "workbench_lv1": {
                    "slots": [{"item_id": "bolts", "quantity": 1}, null, null, null]
                }
            }
        }"#;
        let persisted: PersistedOverrides = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(
            persisted.overrides.get("workbench_lv1").unwrap().base_hash,
            None,
            "missing field must default to None",
        );
        persisted.merge_into(&mut s);
        assert!(
            s.overrides.contains_key("workbench_lv1"),
            "legacy override (no base hash) must be kept across updates",
        );
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
            pinned_upgrades: HashSet::new(),
            collected: HashMap::from([("bolts".to_string(), 7)]),
            disabled_modules: HashSet::new(),
            containers: Vec::new(),
            next_container_seq: 0,
            default_shelf_seeded: false,
            gunsmith_storage_seeded: false,
            research_states: HashMap::new(),
            tracked_research: HashSet::new(),
            pinned_research: HashSet::new(),
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
    fn recipe_knowledge_bundled_edited_unknown() {
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
        s.set_recipe_override(&"workbench_lv1".to_string(), RecipeOverride::new(slots));
        assert_eq!(
            s.recipe_knowledge(&"workbench_lv1".to_string()),
            RecipeKnowledge::Edited
        );
        // Empty recipe = Unknown, regardless of tracking.
        assert_eq!(
            s.recipe_knowledge(&"placeholder_lv1".to_string()),
            RecipeKnowledge::Unknown
        );
    }

    #[test]
    fn hideout_progress_rows_shows_natural_next_level_untracked() {
        let mut s = AppState::new(fixture());
        // Nothing tracked at all — the list still shows every (unlocked)
        // module's next buildable level. Tracking only feeds the overlay's
        // per-item wishlist, not this view.
        let rows = s.hideout_progress_rows();
        assert!(
            rows.iter().any(|r| r.upgrade_id == "workbench_lv1"),
            "workbench's natural next build is Lv1, shown without tracking"
        );
        assert!(
            rows.iter().all(|r| r.upgrade_id != "workbench_lv2"),
            "Lv2 stays hidden until Lv1 is built (strict level order)"
        );
        assert!(rows.iter().any(|r| r.upgrade_id == "placeholder_lv1"));

        // Building Lv1 advances the workbench row to Lv2.
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert!(
            rows.iter().any(|r| r.upgrade_id == "workbench_lv2"),
            "row advances to Lv2 once Lv1 is built"
        );
        assert!(rows.iter().all(|r| r.upgrade_id != "workbench_lv1"));
    }

    #[test]
    fn hideout_progress_rows_parks_unknown_last() {
        let s = AppState::new(fixture());
        // Both modules' next level shows regardless of tracking; the known
        // recipe sorts above the unknown one.
        let rows = s.hideout_progress_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].upgrade_id, "workbench_lv1",
            "known not-started sorts above an unknown recipe"
        );
        assert_eq!(rows[1].upgrade_id, "placeholder_lv1");
        assert_eq!(rows[1].knowledge, RecipeKnowledge::Unknown);
    }

    #[test]
    fn hideout_progress_rows_excludes_completed_and_disabled() {
        let mut s = AppState::new(fixture());
        // Completing lv1 advances the workbench row to lv2 (lv1 no longer shows).
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert!(rows.iter().all(|r| r.upgrade_id != "workbench_lv1"));
        assert!(rows.iter().any(|r| r.upgrade_id == "workbench_lv2"));
        // Locking the workbench module hides its next level...
        s.set_module_disabled(&"workbench".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert!(
            rows.iter().all(|r| r.upgrade_id != "workbench_lv2"),
            "a locked module's next level is hidden"
        );
        // ...but other modules' progression still shows — locking one module
        // doesn't empty the whole view.
        assert!(rows.iter().any(|r| r.upgrade_id == "placeholder_lv1"));
    }

    #[test]
    fn hideout_progress_rows_drops_fully_built_module() {
        let mut s = AppState::new(fixture());
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        s.set_completed_upgrade(&"workbench_lv2".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert!(
            rows.iter().all(|r| !r.upgrade_id.starts_with("workbench")),
            "a module with every level built contributes no row"
        );
        assert!(rows.iter().any(|r| r.upgrade_id == "placeholder_lv1"));
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
    fn can_consume_materials_tracks_stock_against_recipe() {
        let mut s = AppState::new(fixture());
        // workbench_lv1 needs bolts×5 + screws×3.
        assert!(
            !s.can_consume_materials(&"workbench_lv1".to_string()),
            "nothing collected → can't consume",
        );
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 2);
        assert!(
            !s.can_consume_materials(&"workbench_lv1".to_string()),
            "screws short by one → still can't consume",
        );
        s.set_collected(&"screws".to_string(), 3);
        assert!(
            s.can_consume_materials(&"workbench_lv1".to_string()),
            "exact stock of every item → can consume",
        );
    }

    #[test]
    fn can_consume_materials_false_for_empty_recipe() {
        // placeholder_lv1 has no requirements — nothing to consume, so the
        // "consume" choice must stay disabled regardless of collected counts.
        let s = AppState::new(fixture());
        assert!(!s.can_consume_materials(&"placeholder_lv1".to_string()));
    }

    #[test]
    fn can_consume_materials_counts_container_stock() {
        // workbench_lv1 needs bolts×5 + screws×3. Hold the bolts in the stash
        // but the screws only in a container — neither store alone satisfies the
        // recipe, yet the combined inventory does, so consuming must be offered.
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 5);
        assert!(
            !s.can_consume_materials(&"workbench_lv1".to_string()),
            "screws missing entirely → can't consume",
        );
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"screws".to_string(), 3);
        assert!(
            s.can_consume_materials(&"workbench_lv1".to_string()),
            "stash bolts + container screws together cover the recipe",
        );
    }

    #[test]
    fn complete_upgrade_consuming_subtracts_recipe_from_collected() {
        let mut s = AppState::new(fixture());
        // Over-stock bolts (another upgrade also wants them) — consuming lv1
        // must subtract exactly its required 5, not floor or zero it out.
        s.set_collected(&"bolts".to_string(), 8);
        s.set_collected(&"screws".to_string(), 3);
        s.complete_upgrade(&"workbench_lv1".to_string(), true);
        assert!(s.completed_upgrades.contains("workbench_lv1"));
        assert_eq!(*s.collected.get("bolts").unwrap(), 3, "8 - 5 = 3");
        assert_eq!(
            s.collected.get("screws"),
            None,
            "3 - 3 = 0 → entry removed by set_collected",
        );
    }

    #[test]
    fn complete_upgrade_consuming_falls_back_to_containers() {
        // workbench_lv1 needs bolts×5 + screws×3. The stash covers all 3 screws
        // and only 3 of the 5 bolts; a container holds the other bolts. Consuming
        // must drain the stash first, then dip into the container for the 2-bolt
        // shortfall — leaving the stash empty and the container short by 2.
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 3);
        s.set_collected(&"screws".to_string(), 3);
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 4);
        s.complete_upgrade(&"workbench_lv1".to_string(), true);
        assert!(s.completed_upgrades.contains("workbench_lv1"));
        assert_eq!(
            s.collected.get("bolts"),
            None,
            "stash bolts drained first (3 → 0)",
        );
        assert_eq!(
            s.collected.get("screws"),
            None,
            "stash screws drained (3 → 0)"
        );
        assert_eq!(
            s.containers[0].contents.get("bolts"),
            Some(&2),
            "container covers the 2-bolt shortfall once the stash empties (4 → 2)",
        );
    }

    #[test]
    fn complete_upgrade_consuming_drains_stash_then_same_tier_containers_in_order() {
        // bolts split 2 (stash) + 2 (first bag) + 4 (second bag). Consuming
        // workbench_lv2 (bolts×7) takes all of the stash, then — among two
        // same-tier bags — empties the first before dipping into the second:
        // 2 + 2 + 3, leaving the last-touched bag with the remainder. Same-kind
        // containers drain in declaration order; the box-vs-shelf/bag priority
        // only orders *across* tiers (see the next test).
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 2);
        let c1 = s.create_container("First bag".into());
        s.set_container_item(&c1, &"bolts".to_string(), 2);
        let c2 = s.create_container("Second bag".into());
        s.set_container_item(&c2, &"bolts".to_string(), 4);
        s.complete_upgrade(&"workbench_lv2".to_string(), true);
        assert_eq!(s.collected.get("bolts"), None, "stash emptied first");
        assert_eq!(
            s.containers[0].contents.get("bolts"),
            None,
            "first bag emptied next",
        );
        assert_eq!(
            s.containers[1].contents.get("bolts"),
            Some(&1),
            "second bag covers the last bolt (4 → 1)",
        );
    }

    #[test]
    fn complete_upgrade_consuming_drains_boxes_before_shelves_and_bags() {
        // workbench_lv2 needs bolts×7. Stock a bag (declared first), a shelf
        // (second), then a box / item case (third) — 3 bolts each. The box must
        // drain first despite its later declaration; the bag and shelf (one tier)
        // then cover the rest in declaration order: box 3 + bag 3 + shelf 1.
        let mut s = AppState::new(fixture());
        let bag = s.create_container("Backpack".into()); // Bag kind (default)
        s.set_container_item(&bag, &"bolts".to_string(), 3);
        let shelf = s.create_container("Shelf".into());
        s.set_container_kind(&shelf, ContainerKind::Shelf);
        s.set_container_item(&shelf, &"bolts".to_string(), 3);
        let case = s.create_container("Item case".into());
        s.set_container_kind(&case, ContainerKind::Case); // the "box"
        s.set_container_item(&case, &"bolts".to_string(), 3);
        s.complete_upgrade(&"workbench_lv2".to_string(), true);

        let bolts_in = |id: &ContainerId| {
            s.containers
                .iter()
                .find(|c| &c.id == id)
                .unwrap()
                .contents
                .get("bolts")
                .copied()
        };
        assert_eq!(bolts_in(&case), None, "box drains first (3 → 0)");
        assert_eq!(
            bolts_in(&bag),
            None,
            "bag drains next — declaration order within the shelves/bags tier (3 → 0)",
        );
        assert_eq!(
            bolts_in(&shelf),
            Some(2),
            "shelf covers the remainder after box + bag (3 → 2)",
        );
    }

    #[test]
    fn complete_upgrade_skipping_leaves_collected_untouched() {
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 8);
        s.set_collected(&"screws".to_string(), 3);
        s.complete_upgrade(&"workbench_lv1".to_string(), false);
        assert!(s.completed_upgrades.contains("workbench_lv1"));
        assert_eq!(
            *s.collected.get("bolts").unwrap(),
            8,
            "skip → inventory kept"
        );
        assert_eq!(*s.collected.get("screws").unwrap(), 3);
    }

    #[test]
    fn complete_upgrade_consuming_still_auto_tracks_next_level() {
        // Consumption is orthogonal to natural progression: completing lv1
        // (with consumption) must still pull lv2 into the tracked set.
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 3);
        s.complete_upgrade(&"workbench_lv1".to_string(), true);
        assert!(
            s.tracked_upgrades.contains("workbench_lv2"),
            "lv2 should auto-track once lv1 is done, consumption or not",
        );
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
    fn container_icon_set_and_persists() {
        let mut s = AppState::new(fixture());
        let c = s.create_container("Backpack".into());
        assert_eq!(s.containers[0].icon, None, "new containers start icon-less");
        s.set_container_icon(&c, Some("backpack_rucksack".into()));
        assert_eq!(s.containers[0].icon.as_deref(), Some("backpack_rucksack"));

        let json = serde_json::to_string(&PersistedState::from_app(&s)).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();
        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert_eq!(s2.containers[0].icon.as_deref(), Some("backpack_rucksack"));
    }

    #[test]
    fn container_without_icon_key_deserializes_to_none() {
        // A container persisted before the icon field existed (no `icon` key)
        // must load with serde's default (None), not fail.
        let json = r#"{
            "schema_version": 1,
            "data_version": "test",
            "collected": {},
            "containers": [{"id": "ctr-1", "name": "Old bag", "contents": {"bolts": 2}}],
            "next_container_seq": 1
        }"#;
        let back: PersistedState = serde_json::from_str(json).unwrap();
        assert_eq!(back.containers.len(), 1);
        assert_eq!(back.containers[0].icon, None);
        assert_eq!(back.containers[0].contents.get("bolts"), Some(&2));
    }

    #[test]
    fn container_round_trip_persist() {
        let mut s = AppState::new(fixture());
        let c = s.create_container("Backpack".into());
        s.set_container_item(&c, &"bolts".to_string(), 4);
        s.set_container_kind(&c, ContainerKind::Case);
        let persisted = PersistedState::from_app(&s);
        let json = serde_json::to_string(&persisted).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();

        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert_eq!(s2.containers.len(), 1);
        assert_eq!(s2.containers[0].name, "Backpack");
        assert_eq!(s2.containers[0].contents.get("bolts"), Some(&4));
        assert_eq!(s2.containers[0].kind, ContainerKind::Case);
        assert_eq!(s2.owned_total(&"bolts".to_string()), 4);
        // The id counter survives so a freshly minted id can't collide with
        // the restored container.
        assert_eq!(s2.next_container_seq, 1);
        let c2 = s2.create_container("Item case".into());
        assert_ne!(c2, c);
    }

    #[test]
    fn container_kind_box_alias_loads_as_case() {
        // The scannable kind was renamed `Box` -> `Case`; a profile written
        // before the rename stores `"kind": "Box"` and must still load (as
        // `Case`) via the serde alias rather than falling back to the default.
        let json = r#"{
            "schema_version": 1,
            "data_version": "test",
            "collected": {},
            "containers": [{"id": "ctr-1", "name": "Old box", "kind": "Box"}],
            "next_container_seq": 1
        }"#;
        let back: PersistedState = serde_json::from_str(json).unwrap();
        assert_eq!(back.containers[0].kind, ContainerKind::Case);
        // The current spelling still loads too.
        let json_new = json.replace("\"Box\"", "\"Case\"");
        let back_new: PersistedState = serde_json::from_str(&json_new).unwrap();
        assert_eq!(back_new.containers[0].kind, ContainerKind::Case);
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

    // --- Prioritization: pinning, shortfall, nearly-ready (issue #70) --------

    #[test]
    fn pin_toggle_bumps_version() {
        let mut s = AppState::new(fixture());
        let v0 = s.version;
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        assert!(s.version > v0, "pinning must bump version so VR re-renders");
        let v1 = s.version;
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), false);
        assert!(s.version > v1, "unpinning must bump version too");
        // A redundant change (already absent) must NOT bump — avoids needless
        // overlay re-renders.
        let v2 = s.version;
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), false);
        assert_eq!(s.version, v2, "redundant unpin shouldn't bump");
    }

    /// Override the two workbench levels to DISJOINT single-item recipes
    /// (lv1 → screws, lv2 → bolts) so pinning one upgrade moves only its item —
    /// the shared bolts+screws recipe can't demonstrate floating.
    fn disjoint_levels(s: &mut AppState) {
        let mut lv1: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
        lv1[0] = Some(Requirement {
            item_id: "screws".into(),
            quantity: 3,
        });
        s.set_recipe_override(&"workbench_lv1".to_string(), RecipeOverride::new(lv1));
        // lv2's bundled recipe is already bolts×7; tracking both is enough.
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
    }

    #[test]
    fn pinned_floats_in_active_items() {
        let mut s = AppState::new(fixture());
        disjoint_levels(&mut s);

        // Baseline: bolts (needed 7) before screws (needed 3); nothing pinned.
        let base = s.active_items();
        assert_eq!(base[0].item_id, "bolts");
        assert_eq!(base[1].item_id, "screws");
        assert!(base.iter().all(|a| !a.pinned));

        // Pin lv1 (needs screws): screws floats above the larger-needed bolts.
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        let rows = s.active_items();
        assert_eq!(rows[0].item_id, "screws", "pinned item leads");
        assert!(rows[0].pinned);
        assert_eq!(rows[1].item_id, "bolts");
        assert!(!rows[1].pinned);
    }

    #[test]
    fn pin_does_not_unsink_completed() {
        let mut s = AppState::new(fixture());
        disjoint_levels(&mut s);
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        // Fully stock the pinned item (screws) → it's "done" and must sink below
        // the still-needed bolts. `done` is the outermost sort key; a pin can't
        // float a finished item back to the top of the overlay.
        s.set_collected(&"screws".to_string(), 3);
        let rows = s.active_items();
        assert_eq!(rows[0].item_id, "bolts", "still-needed item leads");
        assert_eq!(rows[1].item_id, "screws");
        assert!(
            rows[1].collected >= rows[1].needed,
            "the pinned-but-done item sinks last"
        );
    }

    #[test]
    fn pin_skips_items_already_satisfied_for_that_upgrade() {
        // The reported surprise: pinning an upgrade lit up EVERY material in its
        // recipe as pinned — including ones already stocked to that upgrade's
        // requirement — because the per-item view OR'd the pin across the whole
        // recipe. Such items still read as globally "short" only because OTHER
        // tracked upgrades want more of them, so the pin surfaced materials the
        // pinned upgrade doesn't actually need. A pin must flag only what its
        // own upgrade still lacks.
        let mut s = AppState::new(fixture());
        // lv1 (bolts×5 + screws×3) is the pinned goal; lv2 (bolts×7) is also
        // tracked, so bolts is shared and its aggregate need (12) outruns lv1's.
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        // Stock bolts to exactly lv1's requirement: satisfied *for lv1*, yet
        // still 5/12 against the combined tracked demand.
        s.set_collected(&"bolts".to_string(), 5);

        let items = s.active_items();
        let bolts = items.iter().find(|a| a.item_id == "bolts").unwrap();
        let screws = items.iter().find(|a| a.item_id == "screws").unwrap();
        assert!(
            !bolts.pinned,
            "bolts is satisfied for the pinned upgrade (5/5) — the pin must not \
             flag it even though it's 5/12 across all tracked upgrades"
        );
        assert!(
            screws.pinned,
            "screws is still short for the pinned upgrade (0/3) — that's what the pin highlights"
        );
        // The pin's genuinely-needed item leads, even past the larger bolts grind.
        assert_eq!(
            items[0].item_id, "screws",
            "the pin's still-needed item leads"
        );
    }

    #[test]
    fn pin_sums_shared_item_across_pinned_upgrades() {
        // Two pinned upgrades that share an item are considered together: the
        // item stays pinned until you own enough to satisfy BOTH at once —
        // sum(reqs), not max(reqs). A per-upgrade check would wrongly clear the
        // pin once each upgrade's individual cost was covered.
        let mut s = AppState::new(fixture());
        // lv1 wants bolts×5, lv2 wants bolts×7 → combined pinned demand 12.
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_tracked_upgrade(&"workbench_lv2".to_string(), true);
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        s.set_pinned_upgrade(&"workbench_lv2".to_string(), true);

        // Own 7: clears each upgrade's bolts cost in isolation (≥5 and ≥7), but
        // not the two together (need 12) → still pinned.
        s.set_collected(&"bolts".to_string(), 7);
        let bolts = s
            .active_items()
            .into_iter()
            .find(|a| a.item_id == "bolts")
            .unwrap();
        assert!(
            bolts.pinned,
            "7 bolts covers 5 and 7 separately but not the combined 12 — still pinned"
        );

        // Own the full combined demand → the pin clears.
        s.set_collected(&"bolts".to_string(), 12);
        let bolts = s
            .active_items()
            .into_iter()
            .find(|a| a.item_id == "bolts")
            .unwrap();
        assert!(
            !bolts.pinned,
            "12 bolts satisfies both pinned upgrades together — pin clears"
        );
    }

    #[test]
    fn shortfall_counts_distinct_and_units() {
        let mut s = AppState::new(fixture());
        // workbench_lv1: bolts×5 + screws×3. Own bolts=5 (met), screws=1 (short 2).
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 1);
        let sf = s.upgrade_shortfall(&"workbench_lv1".to_string());
        assert_eq!(sf.items_missing, 1, "only screws is below target");
        assert_eq!(sf.units_missing, 2, "3 needed − 1 owned");
    }

    #[test]
    fn shortfall_boundary_owned_equals_qty_not_missing() {
        let mut s = AppState::new(fixture());
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 3); // exactly met
        let sf = s.upgrade_shortfall(&"workbench_lv1".to_string());
        assert_eq!(
            sf.items_missing, 0,
            "owned == target is not missing (uses <, not <=)"
        );
        assert_eq!(sf.units_missing, 0);
    }

    #[test]
    fn ready_is_tracking_independent() {
        let mut s = AppState::new(fixture());
        // Fully stock workbench_lv1's recipe (bolts×5 + screws×3) but never
        // track it. An untracked upgrade you already have every item for is
        // still claimable in-game, so it must read as ready.
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 3);
        assert!(
            !s.tracked_upgrades.contains("workbench_lv1"),
            "precondition: not tracked"
        );
        assert!(
            s.is_upgrade_ready(&"workbench_lv1".to_string()),
            "untracked + every item collected ⇒ ready"
        );
        // Completing it flips ready → done (a built upgrade isn't ready).
        s.set_completed_upgrade(&"workbench_lv1".to_string(), true);
        assert!(!s.is_upgrade_ready(&"workbench_lv1".to_string()));
    }

    #[test]
    fn hideout_progress_rows_floats_pinned_above_bucket() {
        let mut s = AppState::new(fixture());
        // workbench_lv1 is the module's natural next level; stock it fully so
        // it's ready (rank 0). placeholder_lv1 has an unknown recipe (rank 4).
        // Neither is tracked — pinning and readiness drive this view, not tracking.
        s.set_collected(&"bolts".to_string(), 5);
        s.set_collected(&"screws".to_string(), 3);

        // Unpinned: the ready upgrade leads, the unknown-recipe one is parked last.
        let rows = s.hideout_progress_rows();
        assert_eq!(rows[0].upgrade_id, "workbench_lv1");
        assert_eq!(rows[1].upgrade_id, "placeholder_lv1");

        // Pin the unknown-recipe upgrade → it floats above even the ready one,
        // because an explicit pin outranks the readiness buckets.
        s.set_pinned_upgrade(&"placeholder_lv1".to_string(), true);
        let rows = s.hideout_progress_rows();
        assert_eq!(rows[0].upgrade_id, "placeholder_lv1");
        assert!(rows[0].pinned);
        assert_eq!(rows[1].upgrade_id, "workbench_lv1");
    }

    #[test]
    fn hideout_progress_rows_orders_ready_nearly_inprogress_known_unknown() {
        // Disjoint single-upgrade modules so each upgrade's state is independent
        // (the shared-item fixture can't put one upgrade in every bucket at once).
        fn up(id: &str, reqs: &[(&str, u32)]) -> Upgrade {
            Upgrade {
                id: id.into(),
                name: id.into(),
                level: 1,
                description: String::new(),
                requirements: reqs
                    .iter()
                    .map(|(i, q)| Requirement {
                        item_id: (*i).into(),
                        quantity: *q,
                    })
                    .collect(),
            }
        }
        fn module(id: &str, upgrade: Upgrade) -> HideoutModule {
            HideoutModule {
                id: id.into(),
                name: id.into(),
                upgrades: vec![upgrade],
            }
        }
        let data = Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            research: Vec::new(),
            modules: vec![
                module("m_ready", up("u_ready", &[("ra", 2)])),
                module("m_nearly", up("u_nearly", &[("na", 2)])),
                module(
                    "m_inprog",
                    up("u_inprog", &[("pa", 2), ("pb", 2), ("pc", 2)]),
                ),
                module("m_known", up("u_known", &[("ka", 2), ("kb", 2), ("kc", 2)])),
                module("m_unknown", up("u_unknown", &[])),
            ],
            items: vec![],
        });
        let mut s = AppState::new(data);
        // Tracking is irrelevant to this list now; the ordering is purely by
        // readiness bucket. Collect just enough to place each row in its bucket.
        s.set_collected(&"ra".to_string(), 2); // ready: fully stocked
        s.set_collected(&"na".to_string(), 1); // nearly: 1 item short
        s.set_collected(&"pa".to_string(), 1); // in-progress: 3 items short, some collected
                                               // u_known: nothing collected
        let order: Vec<String> = s
            .hideout_progress_rows()
            .into_iter()
            .map(|r| r.upgrade_id)
            .collect();
        assert_eq!(
            order,
            vec!["u_ready", "u_nearly", "u_inprog", "u_known", "u_unknown"]
        );
    }

    #[test]
    fn pinned_round_trips_through_persisted_state() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        let json = serde_json::to_string(&PersistedState::from_app(&s)).unwrap();
        let back: PersistedState = serde_json::from_str(&json).unwrap();
        let mut s2 = AppState::new(s.data.clone());
        back.merge_into(&mut s2);
        assert!(s2.is_upgrade_pinned(&"workbench_lv1".to_string()));
    }

    #[test]
    fn merge_into_drops_unknown_pins() {
        let mut s = AppState::new(fixture());
        let persisted = PersistedState {
            schema_version: STATE_SCHEMA_VERSION,
            data_version: "test".into(),
            tracked_upgrades: HashSet::new(),
            completed_upgrades: HashSet::new(),
            pinned_upgrades: HashSet::from(["workbench_lv1".to_string(), "ghost".to_string()]),
            collected: HashMap::new(),
            disabled_modules: HashSet::new(),
            containers: Vec::new(),
            next_container_seq: 0,
            default_shelf_seeded: false,
            gunsmith_storage_seeded: false,
            research_states: HashMap::new(),
            tracked_research: HashSet::new(),
            pinned_research: HashSet::new(),
        };
        // Pruning an orphaned pin is silent — only dropped *tracked* upgrades warn.
        let warn = persisted.merge_into(&mut s);
        assert!(warn.is_none(), "a dropped pin must not produce a warning");
        assert!(s.is_upgrade_pinned(&"workbench_lv1".to_string()));
        assert!(
            !s.is_upgrade_pinned(&"ghost".to_string()),
            "unknown pin id is pruned on load"
        );
    }

    #[test]
    fn reset_all_clears_pins() {
        let mut s = AppState::new(fixture());
        s.set_tracked_upgrade(&"workbench_lv1".to_string(), true);
        s.set_pinned_upgrade(&"workbench_lv1".to_string(), true);
        s.reset_all();
        assert!(s.pinned_upgrades.is_empty());
    }

    /// Research fixture: a three-node chain `r1 → r2 → r3` plus `r4` with two
    /// parents (`r2`, `r3`) — the smallest shape exercising the DAG gating
    /// (Basic's `c4→a7` / `b3→a8` merges).
    fn research_fixture() -> Arc<GameData> {
        use crate::data::{ResearchCategory, ResearchNode};
        fn ritem(id: &str, name: &str) -> Item {
            Item {
                id: id.into(),
                name: name.into(),
                icon_path: format!("icons/{id}.png"),
                category: Some("gunsmith".into()),
                subcategory: None,
                weight: None,
                price: None,
                rarity: None,
            }
        }
        fn node(
            id: &str,
            parents: &[&str],
            unlocks: &str,
            samples: &[(&str, u32)],
        ) -> ResearchNode {
            ResearchNode {
                id: id.into(),
                name: id.to_uppercase(),
                parents: parents.iter().map(|p| p.to_string()).collect(),
                unlocks_item_id: unlocks.into(),
                samples: samples
                    .iter()
                    .map(|&(i, q)| Requirement {
                        item_id: i.into(),
                        quantity: q,
                    })
                    .collect(),
            }
        }
        Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: Vec::new(),
            items: vec![
                ritem("part_a", "Part A"),
                ritem("part_b", "Part B"),
                ritem("part_c", "Part C"),
                ritem("part_d", "Part D"),
                ritem("oil", "Oil"),
                ritem("tape", "Tape"),
            ],
            research: vec![ResearchCategory {
                id: "basic".into(),
                name: "Basic Research".into(),
                merchant: "Neumann".into(),
                nodes: vec![
                    node("r1", &[], "part_a", &[("oil", 2)]),
                    node("r2", &["r1"], "part_b", &[("tape", 3), ("oil", 1)]),
                    node("r3", &["r1"], "part_c", &[("tape", 1)]),
                    node("r4", &["r2", "r3"], "part_d", &[("oil", 4)]),
                ],
            }],
        })
    }

    #[test]
    fn research_status_derives_locked_and_available_from_parents() {
        let mut s = AppState::new(research_fixture());
        let (r1, r2) = ("r1".to_string(), "r2".to_string());
        assert_eq!(s.research_status(&r1), ResearchStatus::Available);
        assert_eq!(s.research_status(&r2), ResearchStatus::Locked);

        s.set_research_state(&r1, Some(ResearchNodeState::InProgress));
        assert_eq!(s.research_status(&r1), ResearchStatus::InProgress);
        assert_eq!(
            s.research_status(&r2),
            ResearchStatus::Locked,
            "in-progress parent doesn't unlock the child"
        );

        s.set_research_state(&r1, Some(ResearchNodeState::Developed));
        assert_eq!(s.research_status(&r1), ResearchStatus::Developed);
        assert_eq!(s.research_status(&r2), ResearchStatus::Available);

        assert_eq!(
            s.research_status(&"ghost".to_string()),
            ResearchStatus::Locked,
            "unknown ids read as locked"
        );
    }

    #[test]
    fn developing_untracks_only_itself_no_cascade() {
        let mut s = AppState::new(research_fixture());
        let (r1, r2, r3) = ("r1".to_string(), "r2".to_string(), "r3".to_string());
        s.set_tracked_research(&r1, true);

        s.set_research_state(&r1, Some(ResearchNodeState::Developed));
        assert!(
            !s.tracked_research.contains(&r1),
            "developing removes the node from the wishlist"
        );
        // The children it unlocked become Available, but are NOT auto-tracked —
        // pulling things onto the wishlist behind the user's back was confusing,
        // so pursuing them is now an explicit choice (Track / focus_research).
        assert_eq!(s.research_status(&r2), ResearchStatus::Available);
        assert_eq!(s.research_status(&r3), ResearchStatus::Available);
        assert!(
            !s.tracked_research.contains(&r2) && !s.tracked_research.contains(&r3),
            "developing must not auto-track the newly-unlocked children"
        );
    }

    #[test]
    fn focus_research_tracks_and_pins_the_prerequisite_chain() {
        let mut s = AppState::new(research_fixture());
        let (r1, r2, r3, r4) = (
            "r1".to_string(),
            "r2".to_string(),
            "r3".to_string(),
            "r4".to_string(),
        );
        // r4 depends on r2 + r3, which both depend on r1 — the whole DAG.
        s.focus_research(&r4);
        for id in [&r1, &r2, &r3, &r4] {
            assert!(s.tracked_research.contains(id), "{id} tracked by focus");
            assert!(s.is_research_pinned(id), "{id} pinned by focus");
        }
        // Every focused sample is flagged pinned in the overlay aggregation.
        assert!(s
            .active_items()
            .iter()
            .all(|i| i.pinned || i.collected >= i.needed));

        // Developed prerequisites are pruned from a later focus: develop r1, then
        // focusing r2 should track+pin r2 only (r1 is done, nothing to collect).
        let mut s2 = AppState::new(research_fixture());
        s2.set_research_state(&r1, Some(ResearchNodeState::Developed));
        s2.focus_research(&r2);
        assert!(s2.tracked_research.contains(&r2) && s2.is_research_pinned(&r2));
        assert!(
            !s2.tracked_research.contains(&r1),
            "a developed prerequisite is not re-tracked by focus"
        );
    }

    #[test]
    fn active_items_includes_tracked_research_until_developed() {
        let mut s = AppState::new(research_fixture());
        let r2 = "r2".to_string();
        s.set_tracked_research(&r2, true);

        let items = s.active_items();
        let tape = items.iter().find(|i| i.item_id == "tape").expect("tape");
        assert_eq!(tape.needed, 3);
        assert_eq!(tape.sources, vec!["Research: R2".to_string()]);
        assert!(
            items.iter().any(|i| i.item_id == "oil"),
            "locked-but-tracked nodes still contribute (samples are lootable early)"
        );

        s.set_research_state(&r2, Some(ResearchNodeState::Developed));
        assert!(
            s.active_items().is_empty(),
            "developing untracks the node, so nothing contributes"
        );
    }

    #[test]
    fn complete_research_consuming_drains_inventory() {
        let mut s = AppState::new(research_fixture());
        let r1 = "r1".to_string();
        s.set_collected(&"oil".to_string(), 3);
        assert!(s.can_consume_research_samples(&r1));
        s.complete_research(&r1, true);
        assert_eq!(*s.collected.get("oil").unwrap(), 1, "r1 burned oil×2");
        assert_eq!(s.research_status(&r1), ResearchStatus::Developed);

        // Skip path leaves the inventory untouched.
        let r3 = "r3".to_string();
        s.set_collected(&"tape".to_string(), 5);
        s.complete_research(&r3, false);
        assert_eq!(*s.collected.get("tape").unwrap(), 5);
    }

    #[test]
    fn research_persists_and_prunes_unknown_ids() {
        let mut s = AppState::new(research_fixture());
        s.set_research_state(&"r1".to_string(), Some(ResearchNodeState::Developed));
        s.set_tracked_research(&"r4".to_string(), true);
        s.set_pinned_research(&"r4".to_string(), true);
        let persisted = PersistedState::from_app(&s);

        // Round-trip into a fresh state over the same data: everything kept.
        let mut fresh = AppState::new(research_fixture());
        let warn = persisted.merge_into(&mut fresh);
        assert!(warn.is_none());
        assert_eq!(
            fresh.research_status(&"r1".to_string()),
            ResearchStatus::Developed
        );
        assert!(fresh.tracked_research.contains("r4"));
        assert!(
            fresh.is_research_pinned(&"r4".to_string()),
            "pin survives a round-trip"
        );

        // Merging into a dataset without those nodes prunes: tracked warns,
        // recorded states + pins drop silently.
        let persisted = PersistedState::from_app(&s);
        let mut other = AppState::new(fixture());
        let warn = persisted.merge_into(&mut other).expect("should warn");
        assert!(warn.contains("tracked research node"));
        assert!(other.tracked_research.is_empty());
        assert!(other.research_states.is_empty());
        assert!(
            other.pinned_research.is_empty(),
            "orphaned pin pruned silently"
        );
    }

    #[test]
    fn research_pin_drives_active_items_priority() {
        let mut s = AppState::new(research_fixture());
        let r2 = "r2".to_string();
        s.set_tracked_research(&r2, true);

        // Tracked-but-unpinned: samples present, but not flagged pinned.
        let tape = |s: &AppState| {
            s.active_items()
                .into_iter()
                .find(|i| i.item_id == "tape")
                .expect("tape")
        };
        assert!(!tape(&s).pinned, "tracking alone does not pin");

        // Pinning flags every sample the node still needs.
        s.set_pinned_research(&r2, true);
        assert!(tape(&s).pinned, "pinning a tracked node lights its samples");

        // The pin is inert without tracking — untracking clears the flag even
        // though the pin set still records r2 (mirrors `pinned_upgrades`).
        s.set_tracked_research(&r2, false);
        assert!(
            s.is_research_pinned(&r2),
            "untracking leaves the pin recorded (re-applies on re-track)"
        );
        assert!(
            s.active_items().is_empty(),
            "but an untracked node contributes nothing, pinned or not"
        );

        // Re-tracking re-applies the still-recorded pin without a fresh toggle.
        s.set_tracked_research(&r2, true);
        assert!(tape(&s).pinned, "re-tracking revives the recorded pin");

        // Developing spends the node: no samples, so nothing to prioritize.
        s.set_research_state(&r2, Some(ResearchNodeState::Developed));
        assert!(s.active_items().is_empty());
    }

    #[test]
    fn reset_all_clears_research_pins() {
        let mut s = AppState::new(research_fixture());
        s.set_tracked_research(&"r1".to_string(), true);
        s.set_pinned_research(&"r1".to_string(), true);
        s.reset_all();
        assert!(s.pinned_research.is_empty());
        assert!(s.tracked_research.is_empty());
    }
}
