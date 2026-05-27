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
    /// Items-per-row on the VR overlay grid. 2 is the lower bound where the
    /// layout still feels like a grid; above 10 each cell gets too small to
    /// be readable at typical overlay sizes.
    pub const GRID_COLS: std::ops::RangeInclusive<u32> = 2..=10;
    /// Vertical offset of the overlay above the HMD at show-time, in metres.
    /// 0 sits the panel at eye level (looking forward sees its lower edge);
    /// the upper end pushes it well above so you have to crane up to see it.
    pub const HEIGHT_OFFSET_M: std::ops::RangeInclusive<f32> = 0.0..=1.5;
    /// Vertical offset used when the overlay is locked. Capped well below
    /// `HEIGHT_OFFSET_M`'s max because the whole point of locked mode is
    /// "panel sits just above natural eye-line" — values close to 1.5 m
    /// would defeat the feature.
    pub const LOCKED_HEIGHT_OFFSET_M: std::ops::RangeInclusive<f32> = 0.0..=0.6;
    /// Tilt (degrees) of the locked overlay around its local X axis. 0 = flat
    /// (panel faces the user head-on, like a billboard); higher values lean
    /// the top edge backward so the panel feels like a tilted-up surface.
    /// Capped at the canonical summon tilt — locked mode is the "HUD" mode
    /// and a steeper tilt undermines the glance-able UX.
    pub const LOCKED_TILT_DEG: std::ops::RangeInclusive<f32> = 0.0..=35.0;
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
}

fn default_check_for_updates() -> bool {
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
    /// When true, the overlay stays visible at its current world anchor and
    /// the pitch-driven show/hide FSM is bypassed. Toggled from the desktop
    /// header so the user can pin the panel for hands-free reference.
    #[serde(default)]
    pub locked: bool,
    /// Vertical offset above the HMD used when the overlay is locked.
    /// Separate from `height_offset_m` so locked mode can sit close to
    /// natural eye-line (no craning required) while the unlocked summon
    /// mode keeps its higher "look up to see" placement.
    #[serde(default = "default_locked_height_offset_m")]
    pub locked_height_offset_m: f32,
    /// Tilt (degrees) of the panel when locked. Defaults to 0 (flat) so the
    /// HUD reads head-on; the unlocked summon mode keeps the steeper
    /// `anchor::TILT_DEG` baked-in tilt.
    #[serde(default = "default_locked_tilt_deg")]
    pub locked_tilt_deg: f32,
}

fn default_grid_cols() -> u32 {
    8
}

fn default_height_offset_m() -> f32 {
    // Tracks the canonical anchor::HEIGHT_M so the slider default and the
    // hard-coded fallback used by `world_anchor_from_hmd` stay in lockstep.
    crate::vr::anchor::HEIGHT_M
}

fn default_locked_height_offset_m() -> f32 {
    // Lock mode targets "just above natural forward gaze" so the user can
    // glance at the panel without craning. 0.15 m above the HMD puts the
    // panel's lower edge roughly at brow-level for a typical sitting pose
    // — visible without effort but out of the way of the actual hideout
    // workbench view.
    0.15
}

fn default_locked_tilt_deg() -> f32 {
    // Flat by default — the HUD reads head-on so the user doesn't have to
    // angle their head down to look at a tilted-up surface. The summon-mode
    // tilt (anchor::TILT_DEG) only makes sense when you trigger the panel
    // by looking up.
    0.0
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
            locked: false,
            locked_height_offset_m: default_locked_height_offset_m(),
            locked_tilt_deg: default_locked_tilt_deg(),
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
        let lh = bounds::LOCKED_HEIGHT_OFFSET_M;
        self.locked_height_offset_m = self.locked_height_offset_m.clamp(*lh.start(), *lh.end());
        let lt = bounds::LOCKED_TILT_DEG;
        self.locked_tilt_deg = self.locked_tilt_deg.clamp(*lt.start(), *lt.end());
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
            grid_cols: 6,
            height_offset_m: 0.6,
            locked: false,
            locked_height_offset_m: 0.15,
            locked_tilt_deg: 0.0,
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
            locked: false,
            locked_height_offset_m: 99.0,
            locked_tilt_deg: 999.0,
        };
        vr.sanitize();
        assert_eq!(vr.width_meters, 2.0);
        assert_eq!(vr.show_pitch_deg, 89.0);
        assert_eq!(vr.hide_pitch_deg, 0.0);
        assert_eq!(vr.grid_cols, *bounds::GRID_COLS.end());
        assert_eq!(vr.height_offset_m, *bounds::HEIGHT_OFFSET_M.end());
        assert_eq!(
            vr.locked_height_offset_m,
            *bounds::LOCKED_HEIGHT_OFFSET_M.end()
        );
        assert_eq!(vr.locked_tilt_deg, *bounds::LOCKED_TILT_DEG.end());
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
    fn defaults_match_pose_spec() {
        let vr = VrSettings::default();
        assert_eq!(vr.show_pitch_deg, crate::vr::pose::SHOW_PITCH_DEG);
        assert_eq!(vr.hide_pitch_deg, crate::vr::pose::HIDE_PITCH_DEG);
    }
}
