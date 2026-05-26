//! Atomic save/load of `PersistedState` to `%APPDATA%/ez-wishlist-overlay/state.json`.
//! Recipe overrides ride alongside in `overrides.json` so a corrupt overrides
//! file never takes user progress down with it.

use crate::state::{AppState, PersistedOverrides, PersistedState};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PersistPaths {
    pub data_dir: PathBuf,
    pub state_file: PathBuf,
    pub overrides_file: PathBuf,
    pub settings_file: PathBuf,
    pub log_dir: PathBuf,
}

impl PersistPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "etienneb", "ez-wishlist-overlay")
            .context("could not resolve %APPDATA% via `directories` crate")?;
        let data_dir = dirs.data_dir().to_path_buf();
        let state_file = data_dir.join("state.json");
        let overrides_file = data_dir.join("overrides.json");
        let settings_file = data_dir.join("settings.json");
        let log_dir = data_dir.join("logs");
        Ok(Self {
            data_dir,
            state_file,
            overrides_file,
            settings_file,
            log_dir,
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
    atomic_write(&paths.state_file, json.as_bytes())?;
    save_overrides(paths, state)?;
    Ok(())
}

fn save_overrides(paths: &PersistPaths, state: &AppState) -> Result<()> {
    // Empty `overrides.json` is just noise — clean it up when the user clears
    // every correction so a fresh install matches.
    if state.overrides.is_empty() {
        match std::fs::remove_file(&paths.overrides_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("removing empty {}", paths.overrides_file.display()))
            }
        }
        return Ok(());
    }
    let persisted = PersistedOverrides::from_app(state);
    let json = serde_json::to_string_pretty(&persisted).context("serializing overrides")?;
    atomic_write(&paths.overrides_file, json.as_bytes())?;
    Ok(())
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

pub enum OverridesLoadOutcome {
    Fresh,
    Loaded(Box<PersistedOverrides>),
    Corrupt(Box<CorruptOutcome>),
}

pub fn load_overrides(paths: &PersistPaths) -> OverridesLoadOutcome {
    let raw = match std::fs::read_to_string(&paths.overrides_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OverridesLoadOutcome::Fresh,
        Err(e) => {
            tracing::warn!(error = %e, "could not read overrides.json; starting fresh");
            return OverridesLoadOutcome::Fresh;
        }
    };
    match serde_json::from_str::<PersistedOverrides>(&raw) {
        Ok(o) => OverridesLoadOutcome::Loaded(Box::new(o)),
        Err(e) => {
            let backup = backup_corrupt(&paths.overrides_file).unwrap_or_else(|err| {
                tracing::error!(error = %err, "failed to back up corrupt overrides");
                paths.overrides_file.with_extension("corrupt")
            });
            OverridesLoadOutcome::Corrupt(Box::new(CorruptOutcome {
                backup_path: backup,
                error: e.to_string(),
            }))
        }
    }
}
