//! User-tunable settings, persisted next to `state.json`.
//!
//! Kept deliberately separate from `state.json` — that file tracks wishlist
//! progress and is mutated on every checkbox click; settings change rarely and
//! have their own schema-version lifecycle.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;

/// Bounds on each tunable (UI clamps the slider; loader clamps the file).
pub mod bounds {
    pub const WIDTH_METERS: std::ops::RangeInclusive<f32> = 0.2..=2.0;
    pub const PITCH_DEG: std::ops::RangeInclusive<f32> = 0.0..=89.0;
    pub const DWELL_MS: std::ops::RangeInclusive<u64> = 50..=2000;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub vr: VrSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            vr: VrSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VrSettings {
    /// Overlay width in meters when projected in front of the user.
    pub width_meters: f32,
    /// Pitch above the horizon required to start the show timer.
    pub show_pitch_deg: f32,
    /// Pitch below which the overlay hides immediately. Must stay strictly
    /// below `show_pitch_deg` or the hysteresis collapses.
    pub hide_pitch_deg: f32,
    /// Time the show pitch must be held before fade-in begins.
    pub show_dwell_ms: u64,
}

impl Default for VrSettings {
    fn default() -> Self {
        // Defaults track the SPEC.md §7.2 baselines documented in vr/pose.rs.
        Self {
            width_meters: 0.6,
            show_pitch_deg: crate::vr::pose::SHOW_PITCH_DEG,
            hide_pitch_deg: crate::vr::pose::HIDE_PITCH_DEG,
            show_dwell_ms: crate::vr::pose::DWELL_MS,
        }
    }
}

impl VrSettings {
    /// Re-clamp + enforce hide < show. Called after every UI edit and on load.
    pub fn sanitize(&mut self) {
        let w = bounds::WIDTH_METERS;
        self.width_meters = self.width_meters.clamp(*w.start(), *w.end());
        let p = bounds::PITCH_DEG;
        self.show_pitch_deg = self.show_pitch_deg.clamp(*p.start(), *p.end());
        self.hide_pitch_deg = self.hide_pitch_deg.clamp(*p.start(), *p.end());
        if self.hide_pitch_deg >= self.show_pitch_deg {
            self.hide_pitch_deg = (self.show_pitch_deg - 1.0).max(*p.start());
        }
        let d = bounds::DWELL_MS;
        self.show_dwell_ms = self.show_dwell_ms.clamp(*d.start(), *d.end());
    }
}

/// Read settings from disk. Missing or unreadable file → defaults (silent).
/// Corrupt JSON → defaults, with a warning logged and the file backed up.
pub fn load(path: &Path) -> Settings {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read settings.json; using defaults");
            return Settings::default();
        }
    };
    match serde_json::from_str::<Settings>(&raw) {
        Ok(mut s) => {
            s.vr.sanitize();
            s
        }
        Err(e) => {
            tracing::warn!(error = %e, "settings.json was unparseable; using defaults");
            let _ = backup_corrupt(path);
            Settings::default()
        }
    }
}

/// Atomic save: write a temp file then rename. Same pattern as `persist::save`.
pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(settings).context("serializing settings")?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

fn backup_corrupt(path: &Path) -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("corrupt-{ts}"));
    std::fs::rename(path, &backup)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_swaps_invalid_hysteresis() {
        let mut vr = VrSettings {
            width_meters: 0.6,
            show_pitch_deg: 40.0,
            hide_pitch_deg: 50.0,
            show_dwell_ms: 350,
        };
        vr.sanitize();
        assert!(
            vr.hide_pitch_deg < vr.show_pitch_deg,
            "hide must end up below show"
        );
    }

    #[test]
    fn sanitize_clamps_out_of_range() {
        let mut vr = VrSettings {
            width_meters: 99.0,
            show_pitch_deg: 200.0,
            hide_pitch_deg: -10.0,
            show_dwell_ms: 1_000_000,
        };
        vr.sanitize();
        assert_eq!(vr.width_meters, 2.0);
        assert_eq!(vr.show_pitch_deg, 89.0);
        assert_eq!(vr.hide_pitch_deg, 0.0);
        assert_eq!(vr.show_dwell_ms, 2000);
    }

    #[test]
    fn defaults_match_pose_spec() {
        let vr = VrSettings::default();
        assert_eq!(vr.show_pitch_deg, crate::vr::pose::SHOW_PITCH_DEG);
        assert_eq!(vr.hide_pitch_deg, crate::vr::pose::HIDE_PITCH_DEG);
        assert_eq!(vr.show_dwell_ms, crate::vr::pose::DWELL_MS);
    }
}
