//! Parse `src/data/tasks.ts` into Vendor + Task records.
//!
//! Upstream stores `corps` (vendors) and `tasksData` (tasks) as TypeScript
//! object literals. Task submission requirements only live in free-text
//! `objectives` strings, so we apply a regex-based extractor and back it up
//! with the `taskIds` reverse-link in `task-items.json`.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::items::ItemCatalog;
use crate::model::{Requirement, Task, TaskId, Vendor, VendorId};
use crate::parse_ts;

#[derive(Debug, Deserialize)]
struct UpstreamCorp {
    name: String,
}

#[derive(Debug, Deserialize)]
struct UpstreamTask {
    id: String,
    name: String,
    #[serde(default)]
    objectives: Vec<String>,
    #[serde(rename = "corpId")]
    corp_id: String,
    #[serde(default, rename = "type")]
    task_type: Vec<String>,
    #[serde(default, rename = "requiredTasks")]
    required_tasks: Vec<String>,
}

pub struct TasksResult {
    pub vendors: Vec<Vendor>,
    pub referenced_items: HashSet<String>,
    pub unparsed_objectives: Vec<UnparsedObjective>,
}

#[derive(Debug, Clone)]
pub struct UnparsedObjective {
    pub task_id: TaskId,
    pub objective: String,
    pub reason: String,
}

/// Capture groups: 1 = quantity, 2 = item-name phrase.
///
/// Matches strings like:
///   "Turn in 9 Intel Items Found In Raid"
///   "Submit 3 Electric Items"
///   "Provide 5 batteries"
///   "Deliver 2 Family Videotapes"
static SUBMIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^\s*(?:turn in|submit|deliver|hand over|provide|give)\s+(\d+)\s+(.+?)(?:\s+(?:items?|in raid|found in raid|in\s+raid))?\s*$",
    )
    .expect("submit regex")
});

/// Matches "Find N <name>" / "Collect N <name>" / "Retrieve N <name>" /
/// "Bring N <name>" — these typically pair with a separate "Turn in" step
/// but we still want to capture the quantity in case "Turn in" is omitted.
static FIND_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^\s*(?:find|collect|retrieve|bring|locate)\s+(\d+)\s+(.+?)(?:\s+(?:items?|in raid|found in raid))?\s*$",
    )
    .expect("find regex")
});

/// Implicit quantity = 1: "Turn in <SpecificItem>" / "Submit <Foo>".
/// We require the item name to start with a letter (not a digit) to avoid
/// shadowing the quantified regexes above.
static SUBMIT_ONE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\s*(?:turn in|submit|deliver|hand over|provide|give)\s+([a-zA-Z'][^$]+?)\s*$")
        .expect("submit-one regex")
});

pub fn parse(tasks_ts_path: &Path, catalog: &ItemCatalog) -> Result<TasksResult> {
    let source = std::fs::read_to_string(tasks_ts_path)
        .with_context(|| format!("reading {}", tasks_ts_path.display()))?;

    let corps_value = parse_ts::extract_object(&source, "corps").context("extract corps")?;
    let corps_raw: BTreeMap<String, UpstreamCorp> =
        serde_json::from_value(corps_value).context("deserialize corps")?;

    let tasks_value =
        parse_ts::extract_object(&source, "tasksData").context("extract tasksData")?;
    let tasks_raw: BTreeMap<String, UpstreamTask> =
        serde_json::from_value(tasks_value).context("deserialize tasksData")?;

    // Build reverse map: task_id → Vec<(item_id, qty_hint)> from task-items.json.
    let mut item_to_tasks: HashMap<String, Vec<String>> = HashMap::new();
    for item in catalog.items.values() {
        for tid in &item.task_ids {
            item_to_tasks
                .entry(tid.clone())
                .or_default()
                .push(item.id.clone());
        }
    }

    let mut vendor_tasks: BTreeMap<VendorId, Vec<Task>> = BTreeMap::new();
    let mut referenced_items: HashSet<String> = HashSet::new();
    let mut unparsed: Vec<UnparsedObjective> = Vec::new();

    let mut total = 0;
    let mut with_requirements = 0;
    let mut dropped_no_requirements = 0;

    for (_key, t) in tasks_raw {
        total += 1;
        // Skip community / placeholder entries with no objectives.
        if t.objectives.is_empty() {
            continue;
        }

        let requirements = build_task_requirements(&t, &item_to_tasks, catalog, &mut unparsed);
        if requirements.is_empty() {
            dropped_no_requirements += 1;
            continue;
        }
        with_requirements += 1;

        let reqs: Vec<Requirement> = requirements
            .into_iter()
            .map(|(item_id, quantity)| {
                referenced_items.insert(item_id.clone());
                Requirement { item_id, quantity }
            })
            .collect();

        let task = Task {
            id: t.id.clone(),
            name: t.name,
            vendor_id: t.corp_id.clone(),
            prerequisites: t.required_tasks,
            requirements: reqs,
            source_url: format!("https://www.exfil-zone-assistant.app/tasks/{}", t.id),
        };
        vendor_tasks.entry(t.corp_id).or_default().push(task);
    }

    let vendors: Vec<Vendor> = corps_raw
        .into_iter()
        .map(|(id, c)| {
            let mut tasks = vendor_tasks.remove(&id).unwrap_or_default();
            tasks.sort_by(|a, b| a.id.cmp(&b.id));
            Vendor {
                id,
                name: c.name,
                tasks,
            }
        })
        .collect();

    // Any tasks pointing at a vendor we don't know about: log them.
    for (orphan_vendor, tasks) in &vendor_tasks {
        tracing::warn!(
            vendor = %orphan_vendor,
            count = tasks.len(),
            "tasks pointed at unknown vendor; dropped"
        );
    }

    tracing::info!(
        total,
        with_requirements,
        dropped_no_requirements,
        unparsed = unparsed.len(),
        "parsed tasks",
    );

    Ok(TasksResult {
        vendors,
        referenced_items,
        unparsed_objectives: unparsed,
    })
}

/// Build the (item_id → quantity) map for a single task. Pulls in any
/// linked task-items first (default qty=1), then walks `objectives` and
/// overwrites with parsed quantities. Anything we can't parse but looks
/// like a submission objective is pushed to `unparsed` for diagnostics.
fn build_task_requirements(
    t: &UpstreamTask,
    item_to_tasks: &HashMap<String, Vec<String>>,
    catalog: &ItemCatalog,
    unparsed: &mut Vec<UnparsedObjective>,
) -> BTreeMap<String, u32> {
    let mut requirements: BTreeMap<String, u32> = BTreeMap::new();

    if let Some(linked) = item_to_tasks.get(&t.id) {
        for item_id in linked {
            requirements.entry(item_id.clone()).or_insert(1);
        }
    }

    for obj in &t.objectives {
        apply_objective(t, obj, catalog, item_to_tasks, &mut requirements, unparsed);
    }

    requirements
}

fn apply_objective(
    t: &UpstreamTask,
    obj: &str,
    catalog: &ItemCatalog,
    item_to_tasks: &HashMap<String, Vec<String>>,
    requirements: &mut BTreeMap<String, u32>,
    unparsed: &mut Vec<UnparsedObjective>,
) {
    if let Some((qty, name)) = match_objective(obj) {
        if let Some(item_id) = resolve_item_name(&name, catalog, &t.id, item_to_tasks) {
            // Overwrite the "1" default we placed for task-items.
            requirements.insert(item_id, qty);
        } else if t.task_type.iter().any(|tt| tt == "submit") {
            unparsed.push(UnparsedObjective {
                task_id: t.id.clone(),
                objective: obj.to_string(),
                reason: format!("no item match for `{name}`"),
            });
        }
    } else if is_submit_like(obj) {
        unparsed.push(UnparsedObjective {
            task_id: t.id.clone(),
            objective: obj.to_string(),
            reason: "regex did not match".to_string(),
        });
    }
}

fn match_objective(obj: &str) -> Option<(u32, String)> {
    if let Some(caps) = SUBMIT_RE.captures(obj) {
        let qty: u32 = caps[1].parse().ok()?;
        return Some((qty, clean_item_name(&caps[2])));
    }
    if let Some(caps) = FIND_RE.captures(obj) {
        let qty: u32 = caps[1].parse().ok()?;
        return Some((qty, clean_item_name(&caps[2])));
    }
    if let Some(caps) = SUBMIT_ONE_RE.captures(obj) {
        return Some((1, clean_item_name(&caps[1])));
    }
    None
}

/// Strip trailing "Items"/"in raid"/"Found In Raid" qualifiers and surrounding
/// quote chars from a captured item-name fragment.
fn clean_item_name(raw: &str) -> String {
    let mut s = raw.trim().trim_matches('\'').to_string();
    let lower = s.to_lowercase();
    for suffix in [
        " items found in raid",
        " items in raid",
        " found in raid",
        " in raid",
        " items",
        " item",
    ] {
        if lower.ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }
    s.trim().to_string()
}

fn is_submit_like(obj: &str) -> bool {
    let l = obj.to_lowercase();
    l.contains("turn in")
        || l.contains("submit")
        || l.contains("deliver")
        || l.contains("hand over")
        || l.contains("provide")
}

/// Try to map an objective's item-name fragment to an upstream item id.
///
/// Strategy:
///   1. Exact case-insensitive match against the item name.
///   2. Singularize plural ("Batteries" → "Battery") and retry.
///   3. Prefer task-items that link to this specific task.
fn resolve_item_name(
    raw_name: &str,
    catalog: &ItemCatalog,
    task_id: &TaskId,
    item_to_tasks: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let candidates = lookup_candidates(catalog, raw_name)?;
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }
    // Disambiguate: prefer a candidate that's a task-item linked to this task.
    if let Some(linked) = item_to_tasks.get(task_id) {
        if let Some(c) = candidates.iter().find(|c| linked.contains(c)) {
            return Some(c.clone());
        }
    }
    // Fall back to the first candidate (alphabetically) to be deterministic.
    let mut sorted = candidates.clone();
    sorted.sort();
    sorted.into_iter().next()
}

fn lookup_candidates(catalog: &ItemCatalog, raw: &str) -> Option<Vec<String>> {
    let key = raw.trim().to_lowercase();
    if let Some(ids) = catalog.by_name.get(&key) {
        return Some(ids.clone());
    }
    let singular = singularize(&key);
    if singular != key {
        if let Some(ids) = catalog.by_name.get(&singular) {
            return Some(ids.clone());
        }
    }
    None
}

fn singularize(s: &str) -> String {
    if let Some(stripped) = s.strip_suffix("ies") {
        return format!("{stripped}y");
    }
    if let Some(stripped) = s.strip_suffix('s') {
        return stripped.to_string();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_turn_in_pattern_basic() {
        let (qty, name) = match_objective("Turn in 5 bolts").unwrap();
        assert_eq!(qty, 5);
        assert_eq!(name, "bolts");
    }

    #[test]
    fn matches_submit_pattern() {
        let (qty, name) = match_objective("Submit 3 Electric Items").unwrap();
        assert_eq!(qty, 3);
        assert_eq!(name, "Electric");
    }

    #[test]
    fn strips_in_raid_qualifier() {
        let (qty, name) = match_objective("Turn in 9 Intel Items Found In Raid").unwrap();
        assert_eq!(qty, 9);
        assert_eq!(name, "Intel");
    }

    #[test]
    fn cleans_trailing_item_word() {
        let (qty, name) = match_objective("Turn in 4 combustible items").unwrap();
        assert_eq!(qty, 4);
        assert_eq!(name, "combustible");
    }

    #[test]
    fn matches_provide_pattern() {
        let (qty, name) = match_objective("Provide 5 batteries").unwrap();
        assert_eq!(qty, 5);
        assert_eq!(name, "batteries");
    }

    #[test]
    fn ignores_non_collection_objectives() {
        assert!(match_objective("Eliminate 4 scavengers in Suburb area").is_none());
        assert!(match_objective("Reach the Office Building Near the Motel").is_none());
    }

    #[test]
    fn matches_implicit_quantity_one() {
        let (qty, name) = match_objective("Turn in ARK Floppydisk").unwrap();
        assert_eq!(qty, 1);
        assert_eq!(name, "ARK Floppydisk");
    }

    #[test]
    fn singularizes_plurals() {
        assert_eq!(singularize("batteries"), "battery");
        assert_eq!(singularize("bolts"), "bolt");
        assert_eq!(singularize("intel"), "intel");
    }
}
