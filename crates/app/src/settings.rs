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
    /// How long the terminal OCR feedback card stays before fading out.
    /// 1 s is a reasonable lower bound (anything shorter and the user
    /// barely sees the result); 15 s is generous enough that even a
    /// careful read of a 4-cell panel fits in one show.
    pub const OCR_DISMISS_SECS: std::ops::RangeInclusive<u32> = 1..=15;
    /// Seconds the auto-capture loop pauses between OCR reads. 1 s keeps it
    /// from hammering the compositor mirror back-to-back; 15 s is a relaxed
    /// "walking between panels" pace.
    pub const AUTO_CAPTURE_INTERVAL_SECS: std::ops::RangeInclusive<u32> = 1..=15;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub vr: VrSettings,
    #[serde(default)]
    pub theme: Theme,
    /// Hideout tab layout. See [`HideoutView`]. `#[serde(default)]` means old
    /// settings files (which lack the key) load as `Modules` — no migration
    /// branch needed, same as the other plain defaulted fields below.
    #[serde(default)]
    pub hideout_view: HideoutView,
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
    /// Which compositor mirror eye texture to capture for OCR. Default
    /// is the RIGHT eye — empirically the left eye's mirror buffer
    /// occasionally returned the previous frame on consecutive
    /// captures (the "second screenshot OCR's the first" bug), while
    /// the right eye stayed in sync. Switch to `Left` if a specific
    /// headset behaves the opposite way.
    #[serde(default = "default_capture_eye")]
    pub capture_eye: CaptureEye,
    /// When true, every OCR pass keeps the source screenshot PNG, drops
    /// per-cell binarised strip PNGs (`<stem>.cell<i>.<HHMMSS>.png`),
    /// and writes a `<stem>.ocr-debug.<HHMMSS>.txt` sidecar with every
    /// intermediate the pipeline produced. Also keeps the in-headset
    /// feedback card visible until the next capture replaces it (so
    /// the user has time to inspect the read before grabbing the
    /// debug artifacts). Default OFF — production users would
    /// otherwise accumulate ~10 MB screenshots per capture in their
    /// data dir, and the long-lived overlay would obstruct play.
    #[serde(default)]
    pub ocr_debug: bool,
    /// How long the OCR feedback card stays before fading out, when
    /// `ocr_debug` is off. Ignored when `ocr_debug` is on (the card
    /// then sticks until the next capture so you have time to read
    /// it alongside the on-disk debug artifacts).
    #[serde(default = "default_ocr_dismiss_seconds")]
    pub ocr_dismiss_seconds: u32,
    /// When true, a successful OCR auto-tracks the matched upgrade
    /// and marks every lower-level upgrade in the same module as
    /// completed (the game only shows Lv N's panel after Lv (N-1) is
    /// claimed, so seeing the panel is proof). Default ON.
    ///
    /// Turn OFF when you want to bulk-OCR a bunch of panels just to
    /// refresh inventory counts without touching your tracked /
    /// completed lists. Turn back ON before a raid when you want
    /// the next panel you peek at to be auto-added to "what I'm
    /// working on this run."
    #[serde(default = "default_ocr_auto_track")]
    pub ocr_auto_track: bool,
    /// When true, every OCR capture emits a deep diagnostic trace:
    /// per-process `capture_seq`, FNV-1a hashes of the raw pixel
    /// buffer / RGB-stripped buffer / encoded PNG / pipeline-decoded
    /// bytes, compositor frame index and timing from
    /// `IVRCompositor::GetFrameTiming`, the opposite-eye mirror's
    /// fingerprint, per-step elapsed times, and the first 12 words
    /// the OCR engine returned. Lets you cross-reference exactly
    /// what content the mirror handed back vs what the pipeline
    /// processed if "OCR is reading the previous screenshot"-style
    /// bugs recur. Off by default — the FNV passes hash ~30 MB
    /// per capture and the logs are voluminous.
    #[serde(default)]
    pub ocr_capture_trace: bool,
    /// Seconds the auto-capture loop waits after one OCR read finishes
    /// before grabbing the next mirror frame. Only consulted while the
    /// (non-persisted) auto-capture mode is active — see
    /// [`crate::vr::Runtime::set_auto_capture`]. The mode toggle itself
    /// is deliberately NOT a setting: it force-resets to off on every
    /// launch so you can't leave the loop running into a raid with a
    /// CPU core pegged.
    #[serde(default = "default_auto_capture_interval_secs")]
    pub auto_capture_interval_secs: u32,
}

fn default_check_for_updates() -> bool {
    true
}

fn default_ocr_enabled() -> bool {
    true
}

fn default_capture_eye() -> CaptureEye {
    CaptureEye::Right
}

fn default_ocr_dismiss_seconds() -> u32 {
    4
}

fn default_auto_capture_interval_secs() -> u32 {
    3
}

fn default_ocr_auto_track() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureEye {
    Left,
    /// Default. The left-eye mirror was empirically observed leaking
    /// the previous frame on some headsets — right eye stays in sync.
    #[default]
    Right,
}

/// Which layout the Hideout tab shows. `Modules` is the spatial grid
/// (default — it's the map of the whole hideout); `Progress` is the flat
/// "what's closest to claimable?" list that floats ready / near-complete
/// upgrades to the top. Persisted so the choice survives restarts, like the
/// other view preferences.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HideoutView {
    #[default]
    Modules,
    Progress,
}

impl Settings {
    /// Clamp the OCR-side numeric tunables to their declared bounds.
    /// Called on load and after every UI edit so a hand-written
    /// settings file with an out-of-range value can't break the UI
    /// or the runtime.
    pub fn sanitize_ocr(&mut self) {
        let d = bounds::OCR_DISMISS_SECS;
        self.ocr_dismiss_seconds = self.ocr_dismiss_seconds.clamp(*d.start(), *d.end());
        let a = bounds::AUTO_CAPTURE_INTERVAL_SECS;
        self.auto_capture_interval_secs =
            self.auto_capture_interval_secs.clamp(*a.start(), *a.end());
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            vr: VrSettings::default(),
            theme: Theme::default(),
            hideout_view: HideoutView::default(),
            check_for_updates: default_check_for_updates(),
            dismissed_update_version: None,
            ocr_enabled: default_ocr_enabled(),
            capture_eye: default_capture_eye(),
            ocr_debug: false,
            ocr_dismiss_seconds: default_ocr_dismiss_seconds(),
            ocr_auto_track: default_ocr_auto_track(),
            ocr_capture_trace: false,
            auto_capture_interval_secs: default_auto_capture_interval_secs(),
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
            s.sanitize_ocr();
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

    #[test]
    fn hideout_view_defaults_to_modules_when_absent() {
        // Settings files written before this field existed lack the key;
        // serde(default) must fill it with the grid view, not error.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.hideout_view, HideoutView::Modules);
    }

    #[test]
    fn hideout_view_round_trips() {
        let s = Settings {
            hideout_view: HideoutView::Progress,
            ..Settings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hideout_view, HideoutView::Progress);
    }
}
