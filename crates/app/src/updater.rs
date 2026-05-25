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

use crossbeam_channel::Receiver;
use serde::Deserialize;
use std::time::Duration;

const RELEASES_API: &str =
    "https://api.github.com/repos/etiennechabert/ez-wishlist-overlay/releases/latest";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Latest version stripped of the leading `v` (e.g. `"0.2.0"`).
    pub latest_version: String,
    /// Browser URL for the release page so the user can grab the MSI.
    pub release_url: String,
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
    /// Check completed, a newer release is available.
    UpdateAvailable(UpdateInfo),
    /// Network / parse / API error. Held for display; the next app start
    /// will retry.
    Failed { reason: String },
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
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

    if latest > current {
        tracing::info!(
            current = current_raw,
            latest = %latest_str,
            "newer release available"
        );
        CheckStatus::UpdateAvailable(UpdateInfo {
            latest_version: latest_str,
            release_url: release.html_url,
        })
    } else {
        tracing::debug!(current = current_raw, latest = %latest_str, "up to date");
        CheckStatus::UpToDate {
            latest_version: latest_str,
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
}
