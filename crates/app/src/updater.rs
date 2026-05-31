//! Background "is there a newer release?" check.
//!
//! Spawns one thread at startup that hits the GitHub releases API and posts
//! the terminal [`CheckStatus`] back over a channel so the GUI can surface
//! it (header indicator + About dialog), regardless of outcome. Failures
//! (offline, rate-limited, GitHub down) are reported as
//! [`CheckStatus::Failed`] rather than swallowed, so the user can tell
//! "check ran, nothing to do" apart from "check never finished".
//!
//! This is the only runtime network call the app makes; everything else is
//! embedded at compile time (see SPEC.md §data acquisition). Gated behind
//! `Settings::check_for_updates`; when off, the GUI uses
//! [`CheckStatus::Disabled`] directly without spawning the thread.

use crossbeam_channel::{Receiver, Sender};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const RELEASES_API: &str =
    "https://api.github.com/repos/etiennechabert/ez-wishlist-overlay/releases/latest";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Latest version stripped of the leading `v` (e.g. `"0.2.0"`).
    pub latest_version: String,
    /// Browser URL for the release page — the fallback "Download ↗" link, and
    /// what portable builds use since they have no installer to invoke.
    pub release_url: String,
    /// The `.msi` release asset, when one is attached. `Some` means an
    /// MSI-managed install can be upgraded in place (download + `msiexec`);
    /// `None` falls back to the browser link.
    pub msi: Option<ReleaseAsset>,
}

/// Terminal state of the most recent update check. The GUI starts in
/// [`Self::Checking`] and replaces it with whichever variant the worker
/// thread sends.
#[derive(Debug, Clone)]
pub enum CheckStatus {
    /// User turned off `check_for_updates` in settings; no thread spawned.
    Disabled,
    /// Thread is running, no result yet.
    Checking,
    /// Check completed, the installed version is the latest.
    UpToDate { latest_version: String },
    /// Check completed and `CARGO_PKG_VERSION` is *newer* than the latest
    /// published release. Typically a local dev build — surfaced
    /// separately from [`Self::UpToDate`] so we don't claim "up to date"
    /// for an unreleased version.
    Ahead { latest_version: String },
    /// Check completed, a newer release is available.
    UpdateAvailable(UpdateInfo),
    /// Network / parse / API error. Held for display; the next app start
    /// will retry.
    Failed { reason: String },
}

impl CheckStatus {
    /// `Some(latest_version)` iff the check positively confirmed the running
    /// build is behind a published release — i.e. *definitively* out of date.
    /// Every other state returns `None` so callers **fail open**: a build
    /// that's still checking, offline (`Failed`), ahead of latest (`Ahead`,
    /// a dev build), up to date, or has the check disabled is never treated
    /// as stale. The `match` is exhaustive on purpose — a new variant must
    /// make an explicit out-of-date / not decision here rather than silently
    /// inheriting a default.
    ///
    /// Used to gate "Export corrections": exporting recipe fixes from a stale
    /// build tends to re-report recipes already corrected upstream, so the
    /// button is disabled while this returns `Some`.
    pub fn out_of_date_version(&self) -> Option<&str> {
        match self {
            CheckStatus::UpdateAvailable(info) => Some(&info.latest_version),
            CheckStatus::Disabled
            | CheckStatus::Checking
            | CheckStatus::UpToDate { .. }
            | CheckStatus::Ahead { .. }
            | CheckStatus::Failed { .. } => None,
        }
    }
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// One downloadable file attached to a GitHub release. We only need its name
/// (to pick the `.msi`), its direct download URL, and its size (to verify the
/// download finished intact — GitHub doesn't publish a checksum we can use).
#[derive(Deserialize, Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

/// Spawn the check in a detached thread. Returns the receiving end of a
/// 1-slot channel; the GUI polls it via `try_recv` each frame. The thread
/// exits after exactly one send.
pub fn spawn_check() -> Receiver<CheckStatus> {
    let (tx, rx) = crossbeam_channel::bounded::<CheckStatus>(1);
    let thread = std::thread::Builder::new().name("update-check".into());
    if let Err(e) = thread.spawn(move || {
        let status = run();
        let _ = tx.try_send(status);
    }) {
        tracing::warn!(error = %e, "could not spawn update-check thread");
    }
    rx
}

fn run() -> CheckStatus {
    let current_raw = env!("CARGO_PKG_VERSION");
    let Some(current) = parse_semver(current_raw) else {
        let reason = format!("could not parse own version `{current_raw}`");
        tracing::warn!(version = current_raw, "skipping update check");
        return CheckStatus::Failed { reason };
    };

    let release = match fetch_latest() {
        Ok(r) => r,
        Err(e) => {
            let reason = format!("{e}");
            tracing::warn!(error = %reason, "update check failed");
            return CheckStatus::Failed { reason };
        }
    };

    if release.draft || release.prerelease {
        tracing::debug!(tag = %release.tag_name, "skipping draft/prerelease");
        return CheckStatus::UpToDate {
            latest_version: current_raw.to_string(),
        };
    }

    let latest_str = release.tag_name.trim_start_matches('v').to_string();
    let Some(latest) = parse_semver(&latest_str) else {
        let reason = format!("could not parse upstream version `{}`", release.tag_name);
        tracing::warn!(tag = %release.tag_name, "skipping malformed tag");
        return CheckStatus::Failed { reason };
    };

    match latest.cmp(&current) {
        std::cmp::Ordering::Greater => {
            tracing::info!(
                current = current_raw,
                latest = %latest_str,
                "newer release available"
            );
            CheckStatus::UpdateAvailable(UpdateInfo {
                latest_version: latest_str,
                msi: pick_msi_asset(&release.assets),
                release_url: release.html_url,
            })
        }
        std::cmp::Ordering::Equal => {
            tracing::debug!(current = current_raw, latest = %latest_str, "up to date");
            CheckStatus::UpToDate {
                latest_version: latest_str,
            }
        }
        std::cmp::Ordering::Less => {
            tracing::debug!(
                current = current_raw,
                latest = %latest_str,
                "ahead of latest release (dev build)"
            );
            CheckStatus::Ahead {
                latest_version: latest_str,
            }
        }
    }
}

fn fetch_latest() -> Result<LatestRelease, Box<ureq::Error>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let user_agent = concat!("ez-wishlist-overlay/", env!("CARGO_PKG_VERSION"));
    let body = agent
        .get(RELEASES_API)
        .set("User-Agent", user_agent)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(Box::new)?
        .into_json::<LatestRelease>()
        .map_err(|e| Box::new(ureq::Error::from(e)))?;
    Ok(body)
}

/// Choose the MSI installer asset from a release's attachments. Prefers a name
/// containing "installer" (our release names it `…-<version>-installer.msi`)
/// but accepts any `.msi`, so a naming tweak upstream doesn't silently disable
/// in-app updates.
fn pick_msi_asset(assets: &[ReleaseAsset]) -> Option<ReleaseAsset> {
    let msis: Vec<&ReleaseAsset> = assets
        .iter()
        .filter(|a| a.name.to_lowercase().ends_with(".msi"))
        .collect();
    msis.iter()
        .find(|a| a.name.to_lowercase().contains("installer"))
        .or_else(|| msis.first())
        .map(|a| (*a).clone())
}

// ───────────────────────── download + apply ─────────────────────────
//
// MSI-managed installs apply an update in place: download the release's `.msi`,
// hand it to `msiexec`, and exit so the installer can swap the binary (its
// Finish dialog offers to relaunch). Portable builds have no installer and keep
// using the browser link — see `platform::install_kind`.

/// Progress of an in-app MSI update, surfaced in the banner. The download runs
/// on a worker thread; the GUI drains a channel of these each frame.
#[derive(Debug, Clone)]
pub enum UpdateApplyStatus {
    /// No update in progress — the banner shows "Update now" / "Download ↗".
    Idle,
    /// Streaming the `.msi` to disk. `total` is 0 when the size is unknown.
    Downloading { received: u64, total: u64 },
    /// Download finished; checking the byte count before launching.
    Verifying,
    /// The `.msi` is staged at `path`; the GUI launches `msiexec` and exits.
    ReadyToInstall { path: PathBuf },
    /// `msiexec` started; the app is about to close. Transient.
    Launching,
    /// Download/verify failed. The banner shows this plus the browser link.
    Failed { reason: String },
}

/// Spawn a worker that downloads `asset`, reporting progress over the returned
/// channel and ending in exactly one [`UpdateApplyStatus::ReadyToInstall`] or
/// [`UpdateApplyStatus::Failed`]. The GUI drains it each frame.
pub fn spawn_msi_download(asset: ReleaseAsset) -> Receiver<UpdateApplyStatus> {
    let (tx, rx) = crossbeam_channel::unbounded::<UpdateApplyStatus>();
    let thread = std::thread::Builder::new().name("update-download".into());
    let tx_err = tx.clone();
    if let Err(e) = thread.spawn(move || {
        let terminal = match download_and_stage(&asset, &tx) {
            Ok(path) => UpdateApplyStatus::ReadyToInstall { path },
            Err(reason) => {
                tracing::warn!(error = %reason, "update download failed");
                UpdateApplyStatus::Failed { reason }
            }
        };
        let _ = tx.send(terminal);
    }) {
        // Couldn't even start the thread — report it so the GUI doesn't hang on
        // a spinner that never resolves.
        tracing::warn!(error = %e, "could not spawn update-download thread");
        let _ = tx_err.send(UpdateApplyStatus::Failed {
            reason: format!("could not start download: {e}"),
        });
    }
    rx
}

/// Stream the asset to `%TEMP%/ez-wishlist-overlay-update/<name>` and return
/// its path. Progress is reported on `tx` once per whole-percent change to keep
/// the channel quiet. The file is left in the OS temp dir for `msiexec` to read
/// after we exit; the next download clears the directory first so stale
/// installers don't accumulate.
fn download_and_stage(
    asset: &ReleaseAsset,
    tx: &Sender<UpdateApplyStatus>,
) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("ez-wishlist-overlay-update");
    // Clear anything a previous attempt left behind; we only ever need the one
    // installer we're about to fetch.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create temp dir: {e}"))?;
    let path = dir.join(&asset.name);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .build();
    let user_agent = concat!("ez-wishlist-overlay/", env!("CARGO_PKG_VERSION"));
    let resp = agent
        .get(&asset.url)
        .set("User-Agent", user_agent)
        .call()
        .map_err(|e| format!("request: {e}"))?;

    // Prefer the server's Content-Length; fall back to the size the API
    // reported. 0 means "unknown" → the bar shows an indeterminate state.
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(asset.size);

    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_pct = u8::MAX;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write: {e}"))?;
        received += n as u64;
        // `checked_div` (rather than a manual `if total > 0` guard) keeps
        // clippy happy; when `total` is unknown (0) it yields `None`, so we
        // simply don't report a percentage that frame.
        if let Some(pct) = received.min(total).saturating_mul(100).checked_div(total) {
            let pct = pct as u8;
            if pct != last_pct {
                last_pct = pct;
                let _ = tx.send(UpdateApplyStatus::Downloading { received, total });
            }
        }
    }
    file.flush().map_err(|e| format!("flush: {e}"))?;
    drop(file);

    // Size is the only integrity check we have until releases publish a
    // checksum. A truncated download (dropped connection) is the realistic
    // failure; catch it rather than hand `msiexec` a half file.
    if asset.size > 0 && received != asset.size {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "size mismatch: got {received} bytes, expected {}",
            asset.size
        ));
    }

    let _ = tx.send(UpdateApplyStatus::Verifying);
    Ok(path)
}

/// Parse strict `MAJOR.MINOR.PATCH`. Any pre-release / build suffix
/// (e.g. `0.2.0-rc1`, `0.2.0+sha.abc`) is stripped so the numeric core still
/// compares. Anything weirder returns None; the caller logs and bails.
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    // Strip the SemVer suffix introduced by `-` (prerelease) or `+` (build
    // metadata) before splitting — otherwise `1.0.0+sha.abc` splits into
    // four dot-separated parts and we'd reject a legal version.
    let core = s.find(['-', '+']).map(|i| &s[..i]).unwrap_or(s);
    let mut parts = core.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_semver() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("10.20.30"), Some((10, 20, 30)));
    }

    #[test]
    fn strips_prerelease_and_build_suffix() {
        assert_eq!(parse_semver("0.2.0-rc1"), Some((0, 2, 0)));
        assert_eq!(parse_semver("1.0.0+sha.abc"), Some((1, 0, 0)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_semver(""), None);
        assert_eq!(parse_semver("v0.1.0"), None); // caller strips the v
        assert_eq!(parse_semver("0.1"), None);
        assert_eq!(parse_semver("0.1.0.0"), None);
        assert_eq!(parse_semver("a.b.c"), None);
    }

    #[test]
    fn newer_is_strictly_greater() {
        // Sanity-check the tuple ordering we rely on in run().
        assert!(parse_semver("0.10.0") > parse_semver("0.2.0"));
        assert!(parse_semver("1.0.0") > parse_semver("0.99.99"));
        assert!(parse_semver("0.2.1") > parse_semver("0.2.0"));
        assert!(parse_semver("0.2.0") == parse_semver("0.2.0"));
    }

    #[test]
    fn only_update_available_counts_as_out_of_date() {
        // The "Export corrections" gate hangs off this: exactly one variant
        // blocks; the other five must fail open. This is the behavior we
        // cannot exercise from a dev build (which is always `Ahead`), so it
        // is pinned here instead.
        assert_eq!(
            CheckStatus::UpdateAvailable(UpdateInfo {
                latest_version: "0.3.0".into(),
                release_url: "https://example.invalid/r".into(),
                msi: None,
            })
            .out_of_date_version(),
            Some("0.3.0"),
            "a newer published release means out of date → gate closed"
        );

        // Fail-open states — each must be None so the gate stays open.
        assert_eq!(CheckStatus::Disabled.out_of_date_version(), None);
        assert_eq!(CheckStatus::Checking.out_of_date_version(), None);
        assert_eq!(
            CheckStatus::UpToDate {
                latest_version: "0.2.1".into()
            }
            .out_of_date_version(),
            None
        );
        assert_eq!(
            CheckStatus::Ahead {
                latest_version: "0.2.0".into()
            }
            .out_of_date_version(),
            None,
            "dev build ahead of latest must NOT be treated as stale"
        );
        assert_eq!(
            CheckStatus::Failed {
                reason: "offline".into()
            }
            .out_of_date_version(),
            None,
            "a failed/offline check must fail open, not block the user"
        );
    }

    #[test]
    fn picks_installer_msi_over_other_assets() {
        let assets = vec![
            ReleaseAsset {
                name: "ez-wishlist-overlay-0.3.1-portable.exe".into(),
                url: "u1".into(),
                size: 1,
            },
            ReleaseAsset {
                name: "ez-wishlist-overlay-0.3.1-installer.msi".into(),
                url: "u2".into(),
                size: 2,
            },
        ];
        let msi = pick_msi_asset(&assets).expect("an .msi asset should be picked");
        assert_eq!(msi.name, "ez-wishlist-overlay-0.3.1-installer.msi");
        assert_eq!(msi.url, "u2");
    }

    #[test]
    fn no_msi_asset_returns_none() {
        let assets = vec![ReleaseAsset {
            name: "ez-wishlist-overlay-0.3.1-portable.exe".into(),
            url: "u1".into(),
            size: 1,
        }];
        assert!(
            pick_msi_asset(&assets).is_none(),
            "a release with no .msi must not offer in-app update"
        );
    }
}
