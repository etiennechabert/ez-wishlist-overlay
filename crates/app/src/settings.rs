//! User-tunable settings, persisted next to `state.json`.
//!
//! Kept deliberately separate from `state.json` — that file tracks wishlist
//! progress and is mutated on every checkbox click; settings change rarely and
//! have their own schema-version lifecycle.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 2;

/// Bounds on each tunable (UI clamps the slider; loader clamps the file).
pub mod bounds {
    pub const WIDTH_METERS: std::ops::RangeInclusive<f32> = 0.2..=2.0;
    pub const PITCH_DEG: std::ops::RangeInclusive<f32> = 0.0..=89.0;
    /// Items-per-row on the VR overlay grid. 2 is the lower bound where the
    /// layout still feels like a grid; above 10 each cell gets too small to
    /// be readable at typical overlay sizes.
    pub const GRID_COLS: std::ops::RangeInclusive<u32> = 2..=10;
    /// Vertical offset of the overlay above the HMD at show-time, in metres.
    /// 0 sits the panel at eye level (looking forward sees its lower edge);
    /// the upper end pushes it well above so you have to crane up to see it.
    pub const HEIGHT_OFFSET_M: std::ops::RangeInclusive<f32> = 0.0..=1.5;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub vr: VrSettings,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default = "default_check_for_updates")]
    pub check_for_updates: bool,
    /// Latest release version (e.g. "0.2.0") the user clicked "Dismiss" on.
    /// Suppresses the in-app update banner until a strictly newer release
    /// shows up — re-prompting for the same version every launch would be
    /// nagware, but newer versions still surface.
    #[serde(default)]
    pub dismissed_update_version: Option<String>,
    /// When true, the OCR worker auto-extracts owned counts from every
    /// VR mirror-texture screenshot and overwrites `AppState.collected`
    /// for the matched upgrade. Defaults to ON now that the per-digit
    /// templates ship under `crates/app/src/assets/ocr_templates/` and
    /// the pipeline has been validated against
    /// `hideout_screenshots_native/` (15/15 upgrades identified, owned
    /// counts read correctly across the committed digit templates).
    #[serde(default = "default_ocr_enabled")]
    pub ocr_enabled: bool,
}

fn default_check_for_updates() -> bool {
    true
}

fn default_ocr_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            vr: VrSettings::default(),
            theme: Theme::default(),
            check_for_updates: default_check_for_updates(),
            dismissed_update_version: None,
            ocr_enabled: default_ocr_enabled(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
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
    /// Items-per-row on the overlay grid. Rows are derived from the
    /// wishlist size at render time, so a smaller `grid_cols` produces a
    /// taller, narrower panel and vice versa.
    #[serde(default = "default_grid_cols")]
    pub grid_cols: u32,
    /// Vertical offset above the HMD (metres) baked into the world anchor
    /// captured on show. Takes effect on the next show — already-visible
    /// overlays keep their position until they hide and reappear.
    #[serde(default = "default_height_offset_m")]
    pub height_offset_m: f32,
}

fn default_grid_cols() -> u32 {
    8
}

fn default_height_offset_m() -> f32 {
    // Tracks the canonical anchor::HEIGHT_M so the slider default and the
    // hard-coded fallback used by `world_anchor_from_hmd` stay in lockstep.
    crate::vr::anchor::HEIGHT_M
}

impl Default for VrSettings {
    fn default() -> Self {
        // Defaults track the SPEC.md §7.2 baselines documented in vr/pose.rs.
        Self {
            width_meters: 1.0,
            show_pitch_deg: crate::vr::pose::SHOW_PITCH_DEG,
            hide_pitch_deg: crate::vr::pose::HIDE_PITCH_DEG,
            grid_cols: default_grid_cols(),
            height_offset_m: default_height_offset_m(),
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
        let g = bounds::GRID_COLS;
        self.grid_cols = self.grid_cols.clamp(*g.start(), *g.end());
        let h = bounds::HEIGHT_OFFSET_M;
        self.height_offset_m = self.height_offset_m.clamp(*h.start(), *h.end());
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
            migrate(&mut s);
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

/// Forward-migrate a settings struct loaded from a previous schema
/// version. Runs once per load; we don't gate behind a fresh
/// `SCHEMA_VERSION` check after each step because the migration is
/// idempotent (each branch's predicate is "is this still on the
/// older default I want to flip?").
///
/// v1 → v2: flip `ocr_enabled` from the old default-false (kept while
/// per-digit templates were still being calibrated) to true, so the
/// in-headset feedback overlay surfaces on every capture without the
/// user having to dig into Settings.
fn migrate(s: &mut Settings) {
    if s.schema_version < 2 {
        if !s.ocr_enabled {
            tracing::info!(
                "settings migration v1→v2: enabling OCR (default flipped on now \
                 that digit templates ship and the pipeline is validated)",
            );
            s.ocr_enabled = true;
        }
        s.schema_version = 2;
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
            grid_cols: 6,
            height_offset_m: 0.6,
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
            grid_cols: 99,
            height_offset_m: 99.0,
        };
        vr.sanitize();
        assert_eq!(vr.width_meters, 2.0);
        assert_eq!(vr.show_pitch_deg, 89.0);
        assert_eq!(vr.hide_pitch_deg, 0.0);
        assert_eq!(vr.grid_cols, *bounds::GRID_COLS.end());
        assert_eq!(vr.height_offset_m, *bounds::HEIGHT_OFFSET_M.end());
    }

    #[test]
    fn sanitize_clamps_height_offset_below_min() {
        let mut vr = VrSettings {
            height_offset_m: -2.0,
            ..VrSettings::default()
        };
        vr.sanitize();
        assert_eq!(vr.height_offset_m, *bounds::HEIGHT_OFFSET_M.start());
    }

    #[test]
    fn sanitize_clamps_grid_cols_below_min() {
        let mut vr = VrSettings {
            grid_cols: 0,
            ..VrSettings::default()
        };
        vr.sanitize();
        assert_eq!(vr.grid_cols, *bounds::GRID_COLS.start());
    }

    #[test]
    fn migration_v1_to_v2_flips_ocr_enabled_on() {
        // Simulate the saved settings the previous build wrote:
        // schema_version 1 with ocr_enabled deliberately false (kept
        // off while templates were being calibrated).
        let mut s = Settings {
            schema_version: 1,
            ocr_enabled: false,
            ..Settings::default()
        };
        migrate(&mut s);
        assert_eq!(s.schema_version, 2);
        assert!(s.ocr_enabled, "OCR must auto-enable on v1 → v2 migration");
    }

    #[test]
    fn migration_leaves_v2_settings_alone() {
        // A user who already chose to turn OCR off on schema v2 must
        // keep that choice — the migration only runs when bumping out
        // of v1.
        let mut s = Settings {
            schema_version: 2,
            ocr_enabled: false,
            ..Settings::default()
        };
        migrate(&mut s);
        assert!(!s.ocr_enabled);
        assert_eq!(s.schema_version, 2);
    }

    #[test]
    fn defaults_match_pose_spec() {
        let vr = VrSettings::default();
        assert_eq!(vr.show_pitch_deg, crate::vr::pose::SHOW_PITCH_DEG);
        assert_eq!(vr.hide_pitch_deg, crate::vr::pose::HIDE_PITCH_DEG);
    }
}
