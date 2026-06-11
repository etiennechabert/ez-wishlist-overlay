//! Embedded `data.json` + `icons/*.png` — the committed, hand-maintained
//! dataset (validated against `screenshots/`; see `screenshots/CLAUDE.md`).

use anyhow::{Context, Result};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/assets/"]
#[include = "data.json"]
#[include = "icons/*"]
#[include = "container_icons/*"]
#[include = "vr_actions.json"]
#[include = "vr_bindings_*.json"]
#[include = "ocr_templates/*"]
pub struct Assets;

/// Files comprising the OpenVR action manifest + default bindings. Used by
/// the VR runtime to drop them into a temp dir on startup and feed the
/// manifest path to `IVRInput::SetActionManifestPath`. Manifest first,
/// bindings after — order matters because the manifest references the
/// bindings by relative path. Gated to Windows: the only consumer
/// (`vr::overlay::init_action_input`) is Windows-only too, so on Linux
/// it would be dead code under `-D warnings`.
#[cfg(target_os = "windows")]
pub const VR_ACTION_FILES: &[&str] = &[
    "vr_actions.json",
    "vr_bindings_oculus_touch.json",
    "vr_bindings_knuckles.json",
    "vr_bindings_vive_controller.json",
];

/// Load the bundled `data.json` into a `GameData` struct.
pub fn load_game_data() -> Result<crate::data::GameData> {
    let raw = Assets::get("data.json").context("embedded data.json missing")?;
    let json = std::str::from_utf8(&raw.data).context("data.json is not utf-8")?;
    let data = serde_json::from_str(json).context("data.json failed to deserialize")?;
    Ok(data)
}

/// Read an embedded icon as raw bytes. The `icon_path` comes straight from
/// `data.json` (e.g. `"icons/misc_b_bolts.png"`).
pub fn read_icon(icon_path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
    Assets::get(icon_path).map(|f| f.data)
}

/// Enumerate every PNG under `assets/ocr_templates/` as
/// `(stem_without_extension, bytes)` pairs. Used by the OCR digit-template
/// matcher to load `0.png`…`9.png` + `slash.png` at first use. Returns an
/// empty iterator if the folder is missing (the OCR pipeline falls back to
/// "owned count = 0" — upgrade identification still works).
///
/// Windows-only: the OCR pipeline itself is `cfg(target_os = "windows")`
/// (Windows.Media.Ocr is the only engine), so this helper has no caller
/// on Linux/macOS — gating it here keeps `-D warnings` happy in CI.
#[cfg(target_os = "windows")]
pub fn ocr_template_files() -> Vec<(String, std::borrow::Cow<'static, [u8]>)> {
    Assets::iter()
        .filter(|p| p.starts_with("ocr_templates/") && p.ends_with(".png"))
        .filter_map(|p| {
            let stem = std::path::Path::new(p.as_ref())
                .file_stem()
                .and_then(|s| s.to_str())?
                .to_string();
            let file = Assets::get(&p)?;
            Some((stem, file.data))
        })
        .collect()
}

/// Extract every file in [`VR_ACTION_FILES`] into `dir` (creating it if
/// missing). Returns the absolute path to `vr_actions.json`, which is what
/// `IVRInput::SetActionManifestPath` wants. The binding files are
/// referenced from the manifest by relative path and need to live next to
/// it on disk.
#[cfg(target_os = "windows")]
pub fn extract_vr_actions(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir).context("create VR actions dir")?;
    let mut manifest_path = None;
    for &name in VR_ACTION_FILES {
        let file = Assets::get(name).with_context(|| format!("embedded {name} missing"))?;
        let out = dir.join(name);
        std::fs::write(&out, &file.data).with_context(|| format!("write {name}"))?;
        if name == "vr_actions.json" {
            manifest_path = Some(out);
        }
    }
    manifest_path.context("vr_actions.json not in VR_ACTION_FILES")
}
