//! Loaded, immutable game data. Deserialized once at startup from the embedded
//! `data.json` (hand-maintained; validated against `screenshots/`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type UpgradeId = String;
pub type ItemId = String;
pub type ModuleId = String;
pub type ResearchNodeId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameData {
    pub data_version: String,
    pub scraped_at: String,
    pub source_repo: String,
    pub source_commit: String,
    pub modules: Vec<HideoutModule>,
    pub items: Vec<Item>,
    /// Merchant research trees (issue #168) — hand-maintained like the hideout
    /// recipes, panel-verified against `screenshots/research/`. Defaulted so
    /// older `data.json` files without the section still load.
    #[serde(default)]
    pub research: Vec<ResearchCategory>,
}

/// One research tree tab at a merchant (the game has Basic / Advanced /
/// In-depth Research; only Basic is populated as of S5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchCategory {
    pub id: String,
    pub name: String,
    /// Display name of the merchant offering this tree (e.g. "Neumann").
    pub merchant: String,
    pub nodes: Vec<ResearchNode>,
}

/// One blueprint node in a research tree. Nodes form a DAG, not a tree —
/// side branches merge back into the main spine (e.g. Basic's `b3`→`a8`,
/// `c4`→`a7`) — so a node carries a full `parents` list; it is researchable
/// only once every parent is developed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchNode {
    /// The game's task id, e.g. `task.research.a1` (stable across UI renames).
    pub id: ResearchNodeId,
    /// The tree-node label, e.g. "AR-15 4in RIS" (`WFTaskStringTable`); can
    /// differ from the unlocked item's full display name.
    pub name: String,
    #[serde(default)]
    pub parents: Vec<ResearchNodeId>,
    /// Catalog item this node's blueprint unlocks for purchase.
    pub unlocks_item_id: ItemId,
    /// Items consumed to develop the node ("research samples"; all FROM RAID
    /// in-game). At most [`RECIPE_SLOTS`] entries, same as hideout recipes.
    pub samples: Vec<Requirement>,
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
    /// The exact short label this item shows on the **Gunsmith → Storage** grid,
    /// when it differs from [`Item::name`] in a way the structural gun-part
    /// matcher can't bridge on its own. The misc box/stash tiles show the full
    /// `name`, but the gunsmith storage shows a hand-authored short name
    /// (`"AR308"` for `"AR-10 AR308 7.62x51mm compensator"`, `"AR-308DMR"` for
    /// the `"…Design marksman rifle"` lower). It is *real in-game text*, not a
    /// synonym shim: [`crate::ocr::match_item`] matches a storage tile against
    /// this alias before falling back to structural token matching, so the few
    /// parts whose short name is an acronym/abbreviation (DMR, `Nrd` mags) or
    /// collides with another part's leading token resolve to the right id.
    /// Only set where needed; most gunsmith parts resolve structurally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_alias: Option<String>,
}

/// Lookup tables built once from a `GameData`.
pub struct DataIndex {
    pub items_by_id: HashMap<ItemId, Item>,
    pub upgrades_by_id: HashMap<UpgradeId, UpgradeRef>,
    /// Research nodes across every category, keyed by the game task id.
    pub research_nodes_by_id: HashMap<ResearchNodeId, ResearchNode>,
}

#[derive(Clone)]
pub struct UpgradeRef {
    #[allow(dead_code)] // Used by the future VR overlay renderer.
    pub module_id: ModuleId,
    pub module_name: String,
    pub upgrade: Upgrade,
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

        let research_nodes_by_id = data
            .research
            .iter()
            .flat_map(|c| &c.nodes)
            .map(|n| (n.id.clone(), n.clone()))
            .collect();

        Self {
            items_by_id,
            upgrades_by_id,
            research_nodes_by_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Referential integrity: every hideout recipe requirement must point at an
    /// item that exists in the bundled catalog.
    ///
    /// `data.json` has two halves that evolve independently — the `modules`
    /// recipes are hand-authored from in-game observation, while the `items`
    /// catalog is regenerated from upstream (filtered to `category == "misc"`).
    /// Nothing else verifies that a requirement's `item_id` actually exists in
    /// the catalog, so a stale, mistyped, or wrong-category id ships silently
    /// and renders as the raw id in the UI instead of a display name (issue #89
    /// `misc_b_valve`, issue #96 `taskitem_radio`). This test is the ratchet
    /// that catches that whole class of drift at PR time. Fix a failure by
    /// pointing the requirement at the correct upstream slug in `data.json` —
    /// patch the data, never add a synonym/fallback shim in code.
    #[test]
    fn every_requirement_item_id_resolves_in_catalog() {
        let raw = include_str!("assets/data.json");
        let data: GameData = serde_json::from_str(raw).expect("data.json deserializes");
        let ids: HashSet<&str> = data.items.iter().map(|i| i.id.as_str()).collect();

        let mut orphans: Vec<String> = Vec::new();
        for module in &data.modules {
            for upgrade in &module.upgrades {
                for req in &upgrade.requirements {
                    if !ids.contains(req.item_id.as_str()) {
                        orphans.push(format!("{} requires '{}'", upgrade.id, req.item_id));
                    }
                }
            }
        }

        assert!(
            orphans.is_empty(),
            "{} hideout requirement(s) reference an item_id with no catalog entry \
             (they render as raw ids in the UI — point them at the correct upstream \
             slug in data.json):\n  {}",
            orphans.len(),
            orphans.join("\n  ")
        );
    }

    /// Same ratchet for the research section (issue #168): node ids unique,
    /// every parent resolves and the graph is acyclic, every `unlocks_item_id`
    /// and sample `item_id` exists in the catalog, and sample lists stay
    /// within the game's 4-slot pane (`RECIPE_SLOTS`, same cap as recipes).
    #[test]
    fn research_section_is_internally_consistent() {
        let raw = include_str!("assets/data.json");
        let data: GameData = serde_json::from_str(raw).expect("data.json deserializes");
        let item_ids: HashSet<&str> = data.items.iter().map(|i| i.id.as_str()).collect();

        assert!(
            !data.research.is_empty(),
            "data.json lost its research section"
        );

        let mut errs: Vec<String> = Vec::new();
        for cat in &data.research {
            let node_ids: HashSet<&str> = cat.nodes.iter().map(|n| n.id.as_str()).collect();
            assert_eq!(
                node_ids.len(),
                cat.nodes.len(),
                "{}: duplicate node ids",
                cat.id
            );
            for node in &cat.nodes {
                if node.samples.is_empty() || node.samples.len() > RECIPE_SLOTS {
                    errs.push(format!(
                        "{}: {} has {} samples (want 1..={RECIPE_SLOTS})",
                        cat.id,
                        node.id,
                        node.samples.len()
                    ));
                }
                if !item_ids.contains(node.unlocks_item_id.as_str()) {
                    errs.push(format!(
                        "{}: {} unlocks unknown item '{}'",
                        cat.id, node.id, node.unlocks_item_id
                    ));
                }
                for s in &node.samples {
                    if !item_ids.contains(s.item_id.as_str()) {
                        errs.push(format!(
                            "{}: {} sample references unknown item '{}'",
                            cat.id, node.id, s.item_id
                        ));
                    }
                    if s.quantity == 0 {
                        errs.push(format!(
                            "{}: {} has a zero-quantity sample",
                            cat.id, node.id
                        ));
                    }
                }
                for p in &node.parents {
                    if p == &node.id {
                        errs.push(format!("{}: {} is its own parent", cat.id, node.id));
                    } else if !node_ids.contains(p.as_str()) {
                        errs.push(format!("{}: {} has unknown parent '{p}'", cat.id, node.id));
                    }
                }
            }

            // Acyclicity via Kahn's algorithm: peel nodes whose parents are all
            // peeled; anything left sits on a cycle.
            let mut resolved: HashSet<&str> = HashSet::new();
            loop {
                let before = resolved.len();
                for node in &cat.nodes {
                    if !resolved.contains(node.id.as_str())
                        && node.parents.iter().all(|p| resolved.contains(p.as_str()))
                    {
                        resolved.insert(node.id.as_str());
                    }
                }
                if resolved.len() == before {
                    break;
                }
            }
            if resolved.len() != cat.nodes.len() {
                let stuck: Vec<&str> = cat
                    .nodes
                    .iter()
                    .map(|n| n.id.as_str())
                    .filter(|id| !resolved.contains(id))
                    .collect();
                errs.push(format!("{}: parent cycle through {:?}", cat.id, stuck));
            }
        }

        assert!(
            errs.is_empty(),
            "{} research integrity error(s):\n  {}",
            errs.len(),
            errs.join("\n  ")
        );
    }

    /// Every gunsmith item must ship its icon: unlike the misc catalog (icons
    /// inherited from upstream), these are extracted from the game's 128x
    /// texture atlases, so a typo'd `icon_path` or a forgotten PNG would
    /// silently render as the placeholder rect.
    #[test]
    fn every_gunsmith_item_has_an_embedded_icon() {
        let raw = include_str!("assets/data.json");
        let data: GameData = serde_json::from_str(raw).expect("data.json deserializes");
        let missing: Vec<&str> = data
            .items
            .iter()
            .filter(|i| i.category.as_deref() == Some("gunsmith"))
            .filter(|i| crate::assets::read_icon(&i.icon_path).is_none())
            .map(|i| i.id.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "gunsmith item(s) missing their embedded icon: {missing:?}"
        );
    }

    /// The medical catalog (the in-hideout Medical Box scan) is sourced like
    /// gunsmith —
    /// names from the game `WFItemsStringTable` (`medical.*`), icons cut from the
    /// paks' `ItemIcons/Medicals` textures, weights/prices curated from upstream
    /// (two weight families are pak-confirmed). This ratchet guards that whole
    /// row set: every medical item ships an embedded icon, a weight, and a price,
    /// and — critically for the box-scan OCR, which resolves a tile by matching
    /// its label to `Item.name` — every medical `name` is unique across the
    /// *entire* catalog, so a scanned medical tile can't resolve ambiguously.
    #[test]
    fn medical_items_are_complete_and_unambiguous() {
        let raw = include_str!("assets/data.json");
        let data: GameData = serde_json::from_str(raw).expect("data.json deserializes");
        let medical: Vec<&Item> = data
            .items
            .iter()
            .filter(|i| i.category.as_deref() == Some("medical"))
            .collect();

        let mut errs: Vec<String> = Vec::new();
        for i in &medical {
            if crate::assets::read_icon(&i.icon_path).is_none() {
                errs.push(format!("{}: missing embedded icon {}", i.id, i.icon_path));
            }
            if i.weight.is_none() {
                errs.push(format!("{}: no weight", i.id));
            }
            if i.price.is_none() {
                errs.push(format!("{}: no price", i.id));
            }
        }

        // Name uniqueness across the whole catalog (OCR resolves by `name`).
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for i in &data.items {
            *name_counts.entry(i.name.as_str()).or_default() += 1;
        }
        for i in &medical {
            if name_counts[i.name.as_str()] > 1 {
                errs.push(format!(
                    "{}: name {:?} is not unique in the catalog (box-scan would \
                     resolve it ambiguously)",
                    i.id, i.name
                ));
            }
        }

        assert!(
            errs.is_empty(),
            "{} medical issue(s):\n  {}",
            errs.len(),
            errs.join("\n  ")
        );
    }

    /// Validates the research **database** against the panel-verified ground
    /// truth in `screenshots/research/research.label.txt` (the in-game pane is
    /// the source of truth, per `screenshots/CLAUDE.md`): same node set, same
    /// parents, same unlocked item, and the same ordered sample list. Gun-part
    /// refs in the label keep their game tag (`gunsmith.ar15&ar10.muzzle…`);
    /// app ids are that tag with `.`/`&`/`-` flattened to `_` — the transform
    /// asserted here is the canonical bridge.
    #[test]
    fn research_label_matches_data_json() {
        let raw = include_str!("assets/data.json");
        let data: GameData = serde_json::from_str(raw).expect("data.json deserializes");
        let basic = data
            .research
            .iter()
            .find(|c| c.id == "basic")
            .expect("basic research category");

        let label_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../screenshots/research/research.label.txt");
        let text = std::fs::read_to_string(&label_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", label_path.display()));

        fn tag_to_id(tag: &str) -> String {
            tag.replace(['.', '&', '-'], "_")
        }

        struct LabelNode {
            name: String,
            parents: Vec<String>,
            unlocks: String,
            samples: Vec<(String, u32)>,
        }
        let mut nodes: Vec<(String, LabelNode)> = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let (id, rest) = rest.split_once(']').expect("node header");
                let pi = rest.find("parents=").expect("parents field");
                let ui = rest.find("unlocks=").expect("unlocks field");
                let name = rest[..pi].trim().to_string();
                let parents: Vec<String> = rest[pi + 8..ui]
                    .trim()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|p| format!("task.research.{}", p.trim()))
                    .collect();
                let unlocks = tag_to_id(rest[ui + 8..].trim());
                nodes.push((
                    format!("task.research.{id}"),
                    LabelNode {
                        name,
                        parents,
                        unlocks,
                        samples: Vec::new(),
                    },
                ));
            } else {
                let mut it = line.split_whitespace();
                let (Some(item), Some(qty)) = (it.next(), it.next()) else {
                    panic!("malformed label line: {line}");
                };
                let id = if item.starts_with("gunsmith.") {
                    tag_to_id(item)
                } else {
                    item.to_string()
                };
                nodes
                    .last_mut()
                    .expect("requirement before first node header")
                    .1
                    .samples
                    .push((id, qty.parse().expect("quantity")));
            }
        }

        assert_eq!(
            nodes.len(),
            basic.nodes.len(),
            "label has {} nodes, data.json has {}",
            nodes.len(),
            basic.nodes.len()
        );
        let mut errs: Vec<String> = Vec::new();
        for (id, label) in &nodes {
            let Some(node) = basic.nodes.iter().find(|n| &n.id == id) else {
                errs.push(format!("{id}: in label but not in data.json"));
                continue;
            };
            if node.name != label.name {
                errs.push(format!(
                    "{id}: name '{}' != label '{}'",
                    node.name, label.name
                ));
            }
            if node.parents != label.parents {
                errs.push(format!(
                    "{id}: parents {:?} != label {:?}",
                    node.parents, label.parents
                ));
            }
            if node.unlocks_item_id != label.unlocks {
                errs.push(format!(
                    "{id}: unlocks '{}' != label '{}'",
                    node.unlocks_item_id, label.unlocks
                ));
            }
            let got: Vec<(String, u32)> = node
                .samples
                .iter()
                .map(|s| (s.item_id.clone(), s.quantity))
                .collect();
            if got != label.samples {
                errs.push(format!(
                    "{id}: samples {got:?} != label {:?}",
                    label.samples
                ));
            }
        }
        assert!(
            errs.is_empty(),
            "{} research label divergence(s) (the pane is ground truth — patch \
             data.json):\n  {}",
            errs.len(),
            errs.join("\n  ")
        );
    }
}
