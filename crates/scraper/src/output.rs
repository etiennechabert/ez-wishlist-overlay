//! Materialize the scraper output: `data.json`, `icons/*.png`, `SOURCE.md`.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::items::ItemCatalog;
use crate::model::GameData;

pub const ICON_DIR_NAME: &str = "icons";
pub const ICON_TARGET_PX: u32 = 128;

pub struct WriteOutcome {
    pub icons_written: usize,
    pub icons_missing: Vec<String>,
}

pub fn write_data_json(out_dir: &Path, data: &GameData) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let json = serde_json::to_string_pretty(data).context("serializing data.json")?;
    let path = out_dir.join("data.json");
    write_atomic(&path, json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(
        bytes = json.len(),
        path = %path.display(),
        "wrote data.json"
    );
    Ok(())
}

pub fn write_source_md(out_dir: &Path, data: &GameData) -> Result<()> {
    let body = format!(
        "# Source provenance\n\
        \n\
        - **Upstream repo:** {repo}\n\
        - **Commit:** `{commit}`\n\
        - **Game/data version:** `{version}`\n\
        - **Scraped at:** {ts}\n\
        \n\
        Regenerate with:\n\
        \n\
        ```\n\
        cargo run -p scraper -- --output {out}\n\
        ```\n",
        repo = data.source_repo,
        commit = data.source_commit,
        version = data.data_version,
        ts = data.scraped_at,
        out = out_dir.display(),
    );
    let path = out_dir.join("SOURCE.md");
    write_atomic(&path, body.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Copy + re-encode every icon referenced by `data.items`. Icons are read
/// from `<upstream_root>/public/<upstream_icon>` and written as PNG to
/// `<out_dir>/icons/<item_id>.png`, scaled to fit in `ICON_TARGET_PX`.
pub fn copy_icons(
    out_dir: &Path,
    upstream_root: &Path,
    catalog: &ItemCatalog,
    referenced_ids: &HashSet<String>,
) -> Result<WriteOutcome> {
    let icon_dir = out_dir.join(ICON_DIR_NAME);
    std::fs::create_dir_all(&icon_dir)
        .with_context(|| format!("creating {}", icon_dir.display()))?;

    let mut written = 0;
    let mut missing = Vec::new();

    for id in referenced_ids {
        let Some(record) = catalog.items.get(id) else {
            missing.push(id.clone());
            continue;
        };
        let Some(rel) = &record.upstream_icon else {
            missing.push(id.clone());
            continue;
        };

        let src = upstream_path_for(upstream_root, rel);
        if !src.exists() {
            tracing::warn!(item = %id, src = %src.display(), "icon source missing");
            missing.push(id.clone());
            continue;
        }

        let dest = icon_dir.join(format!("{id}.png"));
        match convert_icon(&src, &dest) {
            Ok(()) => written += 1,
            Err(e) => {
                tracing::warn!(item = %id, error = %e, "icon conversion failed");
                missing.push(id.clone());
            }
        }
    }

    tracing::info!(written, missing = missing.len(), "icon copy complete");
    Ok(WriteOutcome {
        icons_written: written,
        icons_missing: missing,
    })
}

fn upstream_path_for(root: &Path, rel: &str) -> PathBuf {
    let trimmed = rel.trim_start_matches('/');
    root.join("public").join(trimmed)
}

fn convert_icon(src: &Path, dest: &Path) -> Result<()> {
    let img = image::ImageReader::open(src)
        .with_context(|| format!("opening {}", src.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing format of {}", src.display()))?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;

    // Scale to fit ICON_TARGET_PX while preserving aspect.
    let scaled = if img.width() > ICON_TARGET_PX || img.height() > ICON_TARGET_PX {
        img.resize(
            ICON_TARGET_PX,
            ICON_TARGET_PX,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    scaled
        .save_with_format(dest, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
