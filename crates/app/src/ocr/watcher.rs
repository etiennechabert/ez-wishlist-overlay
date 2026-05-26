//! Background thread that polls a directory for newly-arrived image files,
//! runs OCR on each, and emits events back to the GUI thread.
//!
//! The intended trigger is **SteamVR's built-in screenshot chord** (default
//! grip + system on Index / Quest controllers): the user looks at an
//! in-game upgrade panel, presses the chord, SteamVR writes a PNG to its
//! per-app screenshots folder, and we pick it up within ~1 s. No new VR
//! input infrastructure required.
//!
//! Dedup strategy: we track the modification time of the most recent file
//! we've processed and persist that timestamp to disk. Files older than
//! that get skipped. On first run we set the threshold to *now*, so the
//! folder's existing screenshots are ignored — we only react to new
//! captures the user takes after enabling the feature.
//!
//! Why polling instead of `ReadDirectoryChangesW` / `notify`: dependencies
//! pulled in by file-watcher crates are heavy for one cold-path feature.
//! A 1 s `read_dir` over a folder that typically holds <100 files is
//! noise-floor cheap.

use crate::ocr::parse::CapturedUpgrade;
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// SteamVR writes the screenshot file in chunks; if we OCR while it's
/// still being written we get a partial-image error. After we notice a
/// new file we wait until its size is stable across two consecutive
/// polls before reading it.
const STABILITY_INTERVAL: Duration = Duration::from_millis(250);

pub enum WatchEvent {
    /// A screenshot was successfully decoded, OCR'd (two-pass), and
    /// parsed into a structured [`CapturedUpgrade`]. The receiver
    /// just needs to upsert this into the wishlist and notify the user.
    /// `raw_text` is the first-pass full-image OCR dump, kept so we
    /// can re-parse historical captures if the parser logic changes.
    Captured {
        path: PathBuf,
        upgrade: CapturedUpgrade,
        raw_text: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

pub struct Watcher {
    pub events: Receiver<WatchEvent>,
    _join: std::thread::JoinHandle<()>,
}

/// Spawn the watcher. `state_file` is a tiny JSON file (just a single
/// timestamp) used to remember the last-processed file across app
/// restarts so we don't re-process old screenshots on every launch.
pub fn spawn(watch_dir: PathBuf, state_file: PathBuf) -> Watcher {
    let (tx, rx) = unbounded();
    let join = std::thread::Builder::new()
        .name("ez-wishlist-ocr-watcher".into())
        .spawn(move || run(watch_dir, state_file, tx))
        .expect("spawn OCR watcher thread");
    Watcher {
        events: rx,
        _join: join,
    }
}

fn run(watch_dir: PathBuf, state_file: PathBuf, tx: Sender<WatchEvent>) {
    // Make sure the dir exists — Steam doesn't create the per-app
    // screenshot folder until the first screenshot in that app. We
    // create it eagerly so the watcher has a stable target.
    if let Err(e) = std::fs::create_dir_all(&watch_dir) {
        tracing::error!(
            error = %e,
            dir = %watch_dir.display(),
            "OCR watcher cannot create watch dir; thread exiting",
        );
        return;
    }

    // Initialize last_mtime to whatever we persisted last run, or to the
    // current time on first launch. The "current time" branch makes the
    // first session ignore pre-existing screenshots — they're noise from
    // before the feature was enabled.
    let mut last_mtime = load_state(&state_file).unwrap_or_else(SystemTime::now);
    tracing::info!(
        dir = %watch_dir.display(),
        cutoff = ?last_mtime,
        "OCR watcher started",
    );

    loop {
        std::thread::sleep(POLL_INTERVAL);

        let new_files = match scan_new_files(&watch_dir, last_mtime) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, dir = %watch_dir.display(), "scan failed");
                continue;
            }
        };

        for path in new_files {
            if !wait_until_stable(&path) {
                // File disappeared or stayed unstable too long — skip
                // this round; if it stabilizes later, the mtime check
                // will pick it up on a subsequent poll.
                continue;
            }

            // Advance the cutoff *before* OCR runs, so a slow / crashing
            // OCR call doesn't trap us in a loop re-processing the same
            // file on every restart.
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(t) = meta.modified() {
                    if t > last_mtime {
                        last_mtime = t;
                        if let Err(e) = save_state(&state_file, last_mtime) {
                            tracing::warn!(error = %e, "persist watcher cutoff failed");
                        }
                    }
                }
            }

            match crate::ocr::extract::extract_upgrade(&path) {
                Ok((upgrade, raw_text)) => {
                    tracing::info!(
                        path = %path.display(),
                        key = %upgrade.key,
                        items = upgrade.items.len(),
                        cost = upgrade.cost,
                        with_progress = upgrade
                            .items
                            .iter()
                            .filter(|i| i.collected.is_some())
                            .count(),
                        "OCR two-pass extract finished",
                    );
                    if tx
                        .send(WatchEvent::Captured {
                            path: path.clone(),
                            upgrade,
                            raw_text,
                        })
                        .is_err()
                    {
                        tracing::info!("OCR watcher receiver dropped; exiting");
                        return;
                    }
                }
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    tracing::warn!(path = %path.display(), error = %err_msg, "OCR extract failed");
                    let _ = tx.send(WatchEvent::Failed {
                        path,
                        error: err_msg,
                    });
                }
            }
        }
    }
}

/// Return image files in `dir` modified strictly after `cutoff`, sorted
/// oldest-first so we process screenshots in the order the user took them.
fn scan_new_files(dir: &Path, cutoff: SystemTime) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let path = entry.path();
        if !is_image_extension(&path) {
            continue;
        }
        if is_steamvr_stereo(&path) {
            // SteamVR writes a stereo "<name>_vr.jpg" alongside every
            // mono screenshot. The stereo version is side-by-side
            // left/right eyes, so OCR sees every UI element twice +
            // some perspective distortion — strictly worse than the
            // mono shot. Skip it.
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if mtime > cutoff {
            out.push((mtime, path));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    Ok(out.into_iter().map(|(_, p)| p).collect())
}

fn is_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase()),
        Some(ref ext) if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp")
    )
}

/// True if the filename ends `_vr.<ext>` (case-insensitive on stem). This
/// is SteamVR's convention for the stereoscopic left+right screenshot it
/// writes alongside the mono `.jpg`. We only want the mono.
fn is_steamvr_stereo(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    stem.ends_with("_vr")
}

/// Block until the file's size stays the same across two consecutive
/// reads, or until we've waited too long (~5 s). Prevents OCR on a
/// half-written PNG. Returns true if stable, false if the file
/// disappeared or never stabilized.
fn wait_until_stable(path: &Path) -> bool {
    let mut prev = match std::fs::metadata(path).and_then(|m| Ok(m.len())) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for _ in 0..20 {
        std::thread::sleep(STABILITY_INTERVAL);
        let now = match std::fs::metadata(path).and_then(|m| Ok(m.len())) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if now == prev && now > 0 {
            return true;
        }
        prev = now;
    }
    false
}

// --- State persistence ------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    last_processed_unix_ms: u128,
}

fn load_state(path: &Path) -> Option<SystemTime> {
    let raw = std::fs::read_to_string(path).ok()?;
    let s: State = serde_json::from_str(&raw).ok()?;
    let dur = Duration::from_millis(s.last_processed_unix_ms.min(u64::MAX as u128) as u64);
    Some(SystemTime::UNIX_EPOCH + dur)
}

fn save_state(path: &Path, t: SystemTime) -> anyhow::Result<()> {
    let dur = t.duration_since(SystemTime::UNIX_EPOCH)?;
    let s = State {
        last_processed_unix_ms: dur.as_millis(),
    };
    let json = serde_json::to_string(&s)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// --- Steam screenshot dir auto-detection ------------------------------

/// Steam app ID for *Contractors Showdown : ExfilZone*. Used to build
/// the per-app screenshot path under `userdata\<id>\760\remote\<app>`.
pub const EXFILZONE_APPID: &str = "2719160";

/// Best-effort path to ExfilZone's SteamVR screenshot directory. Returns
/// the path if Steam appears to be installed at the default location and
/// at least one user profile exists under `userdata\`. The dir itself
/// may not exist yet — Steam creates it on the first per-app screenshot
/// — but our watcher creates it on startup.
pub fn auto_detect_exfilzone_screenshots_dir() -> Option<PathBuf> {
    // We don't read the registry for the Steam install path (would need
    // an extra crate) — the default-install location covers the
    // overwhelming majority of users. Power users can override via
    // settings.
    let candidates = [
        r"C:\Program Files (x86)\Steam",
        r"C:\Program Files\Steam",
    ];
    let steam = candidates.iter().map(PathBuf::from).find(|p| p.exists())?;
    let userdata = steam.join("userdata");
    let user = std::fs::read_dir(&userdata)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_digit())
        })?;
    Some(
        user.path()
            .join("760")
            .join("remote")
            .join(EXFILZONE_APPID)
            .join("screenshots"),
    )
}
