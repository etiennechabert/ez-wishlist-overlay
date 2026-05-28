//! Output schema for `data.json` — what the shipped app consumes.
//!
//! Keep this in sync with `crates/app/src/data.rs`. The two must agree byte
//! for byte on the JSON shape.

use serde::{Deserialize, Serialize};

pub type TaskId = String;
pub type ItemId = String;
pub type VendorId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameData {
    pub data_version: String,
    pub scraped_at: String,
    pub source_repo: String,
    pub source_commit: String,
    /// Hideout modules are owned by the hideout_screenshots skill (hand-
    /// validated against in-game screenshots). The scraper reads the existing
    /// data.json's `modules` value and writes it back unchanged, so we treat
    /// it as opaque JSON here — that way fields like `cost` (added later by
    /// the skill) round-trip without us having to grow this struct.
    pub modules: serde_json::Value,
    pub vendors: Vec<Vendor>,
    pub items: Vec<Item>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub name: String,
    pub icon_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcategory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rarity: Option<String>,
}
