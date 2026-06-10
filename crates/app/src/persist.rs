//! Atomic save/load of `PersistedState` to `%APPDATA%/ez-wishlist-overlay/state.json`.
//! Recipe overrides ride alongside in `overrides.json` so a corrupt overrides
//! file never takes user progress down with it.

use crate::state::{AppState, PersistedOverrides, PersistedState};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PersistPaths {
    pub data_dir: PathBuf,
    pub state_file: PathBuf,
    pub overrides_file: PathBuf,
    pub settings_file: PathBuf,
    /// Per-session debug bundle: VR capture PNGs, OCR sidecar files, and
    /// (when file-logging lands) `app.log`. Lives under `data_dir` but is
    /// kept strictly separate from the user-data files so "Open debug
    /// folder" never mixes throwaway output with `state.json` /
    /// `overrides.json` / `settings.json`. Flushed empty at startup (see
    /// [`PersistPaths::flush_session_debug`]) so it only ever holds the
    /// current session — bounding disk use and making the "attach the
    /// debug folder to a bug report" workflow unambiguous.
    pub debug_dir: PathBuf,
}

impl PersistPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "etienneb", "ez-wishlist-overlay")
            .context("could not resolve %APPDATA% via `directories` crate")?;
        let data_dir = dirs.data_dir().to_path_buf();
        let state_file = data_dir.join("state.json");
        let overrides_file = data_dir.join("overrides.json");
        let settings_file = data_dir.join("settings.json");
        let debug_dir = data_dir.join("debug");
        Ok(Self {
            data_dir,
            state_file,
            overrides_file,
            settings_file,
            debug_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        Ok(())
    }

    /// Clear last session's debug artifacts and recreate `debug/` empty, so
    /// the directory only ever holds *this* session's bundle. Called once
    /// from `main()` right after [`ensure_dirs`](Self::ensure_dirs), before
    /// any thread can write a capture.
    ///
    /// Best-effort and scoped strictly to throwaway debug paths: a locked or
    /// in-use file (a screenshot still open in a viewer, say) must never
    /// block startup, so every failure is logged and swallowed. It only ever
    /// touches `debug/` and the legacy dirs below — never `state.json`,
    /// `overrides.json`, or `settings.json`.
    ///
    /// Also migrates the pre-consolidation layout: earlier builds wrote
    /// captures to `<data_dir>/vr_screenshots/` and created an always-empty
    /// `<data_dir>/logs/` that nothing ever wrote to. Both are superseded by
    /// `debug/`; remove them so the data folder stops showing stale debug
    /// output next to the user files.
    pub fn flush_session_debug(&self) {
        Self::remove_dir_best_effort(&self.debug_dir, "debug dir");
        if let Err(e) = std::fs::create_dir_all(&self.debug_dir) {
            tracing::warn!(
                error = %e,
                dir = %self.debug_dir.display(),
                "could not recreate debug dir at startup (continuing)",
            );
        }

        // Legacy locations from before the `debug/` consolidation. Removing
        // them every launch is cheap (a no-op once gone) and simpler than
        // tracking a one-time migration.
        for legacy in ["vr_screenshots", "logs"] {
            Self::remove_dir_best_effort(&self.data_dir.join(legacy), "legacy debug dir");
        }
    }

    /// `remove_dir_all` that treats "already gone" as success and only warns
    /// on a real failure. Used by the startup flush so a stubborn file can't
    /// abort launch.
    fn remove_dir_best_effort(dir: &std::path::Path, what: &str) {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "could not clear {what} at startup (continuing)",
            ),
        }
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
    // The temp name must be unique per call: the exit-time flush
    // (`gui::App::on_exit`) can run while the debounced save thread is
    // mid-write. With a shared name, one thread's rename can promote — or
    // delete out from under — the other's half-written temp file, leaving a
    // truncated state.json. Unique names keep each save independently atomic;
    // whichever rename lands last wins with complete content. The pid guards
    // the same way against a second app instance writing the same file.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming to {}", path.display()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `PersistPaths` rooted at a unique temp dir so the
    /// `flush_session_debug` filesystem tests can't collide with each other
    /// or with a real install. The process-id + tag suffix keeps parallel
    /// test runs isolated; the up-front wipe clears any leftovers from a
    /// crashed prior run.
    fn temp_paths(tag: &str) -> PersistPaths {
        let root =
            std::env::temp_dir().join(format!("ezwo-persist-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        PersistPaths {
            data_dir: root.clone(),
            state_file: root.join("state.json"),
            overrides_file: root.join("overrides.json"),
            settings_file: root.join("settings.json"),
            debug_dir: root.join("debug"),
        }
    }

    fn count_entries(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).map(|it| it.count()).unwrap_or(0)
    }

    #[test]
    fn flush_recreates_debug_empty_and_preserves_user_data() {
        let paths = temp_paths("flush-preserve");
        paths.ensure_dirs().unwrap();

        // User-data files the flush must NEVER touch.
        std::fs::write(&paths.state_file, b"STATE").unwrap();
        std::fs::write(&paths.overrides_file, b"OVERRIDES").unwrap();
        std::fs::write(&paths.settings_file, b"SETTINGS").unwrap();

        // Last session's debug artifacts + the pre-consolidation layout.
        std::fs::create_dir_all(paths.debug_dir.join("vr_screenshots")).unwrap();
        std::fs::write(paths.debug_dir.join("vr_screenshots").join("old.png"), b"x").unwrap();
        std::fs::create_dir_all(paths.data_dir.join("vr_screenshots")).unwrap();
        std::fs::write(
            paths.data_dir.join("vr_screenshots").join("legacy.png"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir_all(paths.data_dir.join("logs")).unwrap();

        paths.flush_session_debug();

        // debug/ is back, and empty — this session starts from scratch.
        assert!(paths.debug_dir.is_dir(), "debug dir should be recreated");
        assert_eq!(
            count_entries(&paths.debug_dir),
            0,
            "debug dir should be empty"
        );
        // Legacy locations migrated away.
        assert!(!paths.data_dir.join("vr_screenshots").exists());
        assert!(!paths.data_dir.join("logs").exists());
        // User data is exactly as it was.
        assert_eq!(std::fs::read(&paths.state_file).unwrap(), b"STATE");
        assert_eq!(std::fs::read(&paths.overrides_file).unwrap(), b"OVERRIDES");
        assert_eq!(std::fs::read(&paths.settings_file).unwrap(), b"SETTINGS");

        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn flush_on_fresh_install_creates_empty_debug_dir() {
        let paths = temp_paths("flush-fresh");
        paths.ensure_dirs().unwrap();

        // No debug/ or legacy dirs exist yet (first launch). Flush must still
        // succeed and leave an empty debug/ behind — best-effort, no error.
        paths.flush_session_debug();

        assert!(paths.debug_dir.is_dir());
        assert_eq!(count_entries(&paths.debug_dir), 0);

        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    /// The exit-time flush (`gui::App::on_exit`) can race the debounced save
    /// thread's own in-flight write. Each save must be independently atomic:
    /// with a shared temp name, one thread's rename could promote — or delete
    /// out from under — the other's half-written temp, surfacing as a failed
    /// `save()` or a truncated `state.json`. Unique temp names make every
    /// call succeed and leave the survivor complete, with no temp residue.
    #[test]
    fn concurrent_saves_all_succeed_and_leave_a_parseable_file() {
        use crate::data::GameData;
        use crate::state::{AppState, PersistedState};
        use std::sync::Arc;

        let paths = temp_paths("concurrent-saves");
        paths.ensure_dirs().unwrap();
        let state = Arc::new(parking_lot::RwLock::new(AppState::new(Arc::new(
            GameData {
                data_version: "test".into(),
                scraped_at: "now".into(),
                source_repo: "test".into(),
                source_commit: "deadbeef".into(),
                modules: Vec::new(),
                items: Vec::new(),
            },
        ))));
        state.write().set_collected(&"bolts".to_string(), 7);

        std::thread::scope(|s| {
            for _ in 0..2 {
                let state = &state;
                let paths = &paths;
                s.spawn(move || {
                    for _ in 0..100 {
                        save(paths, &state.read()).expect("every concurrent save must succeed");
                    }
                });
            }
        });

        let raw = std::fs::read_to_string(&paths.state_file).unwrap();
        let parsed: PersistedState =
            serde_json::from_str(&raw).expect("surviving state.json parses");
        assert_eq!(parsed.collected.get("bolts"), Some(&7));
        // Every temp file was either renamed into place or cleaned up.
        let leftovers: Vec<String> = std::fs::read_dir(&paths.data_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }
}
