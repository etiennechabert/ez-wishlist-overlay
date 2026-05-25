//! Atomic save/load of `PersistedState` to `%APPDATA%/ez-wishlist-overlay/state.json`.

use crate::state::{AppState, PersistedState};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PersistPaths {
    pub data_dir: PathBuf,
    pub state_file: PathBuf,
    pub settings_file: PathBuf,
    pub log_dir: PathBuf,
    /// Tiny JSON file holding the OCR watcher's last-processed mtime
    /// cutoff, so we don't re-OCR existing screenshots on every launch.
    pub ocr_watcher_state_file: PathBuf,
    /// OCR-captured upgrade entries — the new source of truth for what
    /// the user wants to track. See [`crate::wishlist`].
    pub wishlist_file: PathBuf,
}

impl PersistPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "etienneb", "ez-wishlist-overlay")
            .context("could not resolve %APPDATA% via `directories` crate")?;
        let data_dir = dirs.data_dir().to_path_buf();
        let state_file = data_dir.join("state.json");
        let settings_file = data_dir.join("settings.json");
        let log_dir = data_dir.join("logs");
        let ocr_watcher_state_file = data_dir.join("ocr-watcher.json");
        let wishlist_file = data_dir.join("wishlist.json");
        Ok(Self {
            data_dir,
            state_file,
            settings_file,
            log_dir,
            ocr_watcher_state_file,
            wishlist_file,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        std::fs::create_dir_all(&self.log_dir)
            .with_context(|| format!("creating {}", self.log_dir.display()))?;
        Ok(())
    }
}

pub enum LoadOutcome {
    Fresh,
    Loaded(Box<PersistedState>),
    Corrupt(Box<CorruptOutcome>),
}

pub struct CorruptOutcome {
    pub backup_path: PathBuf,
    pub error: String,
}

pub fn load(paths: &PersistPaths) -> LoadOutcome {
    let raw = match std::fs::read_to_string(&paths.state_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Fresh,
        Err(e) => {
            tracing::warn!(error = %e, "could not read state.json; starting fresh");
            return LoadOutcome::Fresh;
        }
    };
    match serde_json::from_str::<PersistedState>(&raw) {
        Ok(state) => LoadOutcome::Loaded(Box::new(state)),
        Err(e) => {
            let backup = backup_corrupt(&paths.state_file).unwrap_or_else(|err| {
                tracing::error!(error = %err, "failed to back up corrupt state");
                paths.state_file.with_extension("corrupt")
            });
            LoadOutcome::Corrupt(Box::new(CorruptOutcome {
                backup_path: backup,
                error: e.to_string(),
            }))
        }
    }
}

fn backup_corrupt(state_file: &std::path::Path) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = state_file.with_extension(format!("corrupt-{ts}"));
    std::fs::rename(state_file, &backup)?;
    Ok(backup)
}

/// Atomic write: dump to a temp file, then rename. Survives mid-write crashes.
pub fn save(paths: &PersistPaths, state: &AppState) -> Result<()> {
    paths.ensure_dirs()?;
    let persisted = PersistedState::from_app(state);
    let json = serde_json::to_string_pretty(&persisted).context("serializing state")?;
    let tmp = paths.state_file.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &paths.state_file)
        .with_context(|| format!("renaming to {}", paths.state_file.display()))?;
    Ok(())
}
