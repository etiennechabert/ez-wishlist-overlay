//! Embedded `data.json` + `icons/*.png` produced by the scraper.

use anyhow::{Context, Result};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/assets/"]
#[include = "data.json"]
#[include = "icons/*"]
pub struct Assets;

/// Load the bundled `data.json` into a `GameData` struct.
pub fn load_game_data() -> Result<crate::data::GameData> {
    let raw = Assets::get("data.json").context("embedded data.json missing")?;
    let json = std::str::from_utf8(&raw.data).context("data.json is not utf-8")?;
    let data = serde_json::from_str(json).context("data.json failed to deserialize")?;
    Ok(data)
}

/// Read an embedded icon as raw bytes. The `icon_path` is what the scraper
/// wrote (e.g. `"icons/misc_b_bolts.png"`).
pub fn read_icon(icon_path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
    Assets::get(icon_path).map(|f| f.data)
}
