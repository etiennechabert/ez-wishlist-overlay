//! Acquire a checkout of the upstream ExfilZone Assistant repository.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Upstream {
    pub root: PathBuf,
    pub commit: String,
    /// True if we created the dir ourselves and may delete it on cleanup.
    pub owned_temp: bool,
}

impl Upstream {
    pub fn from_existing(path: &Path) -> Result<Self> {
        if !path.join(".git").exists() && !path.join("package.json").exists() {
            bail!(
                "{} does not look like a checkout of exfil-zone-assistant",
                path.display()
            );
        }
        let commit = git_head_hash(path).unwrap_or_else(|_| "unknown".to_string());
        Ok(Self {
            root: path.to_path_buf(),
            commit,
            owned_temp: false,
        })
    }

    pub fn clone(repo_url: &str, git_ref: &str, into: &Path) -> Result<Self> {
        if into.exists() {
            std::fs::remove_dir_all(into)
                .with_context(|| format!("removing stale clone at {}", into.display()))?;
        }
        let status = Command::new("git")
            .args([
                "clone",
                "--depth=1",
                "--branch",
                git_ref,
                repo_url,
                &into.to_string_lossy(),
            ])
            .status()
            .context("failed to spawn git")?;
        if !status.success() {
            // Fall back: clone default branch then maybe checkout the ref.
            let status2 = Command::new("git")
                .args(["clone", "--depth=1", repo_url, &into.to_string_lossy()])
                .status()
                .context("failed to spawn git (retry)")?;
            if !status2.success() {
                bail!("git clone failed for {repo_url}");
            }
        }
        let commit = git_head_hash(into)?;
        Ok(Self {
            root: into.to_path_buf(),
            commit,
            owned_temp: true,
        })
    }

    pub fn cleanup(self) {
        if self.owned_temp {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn git_head_hash(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .context("git rev-parse HEAD")?;
    if !out.status.success() {
        bail!("git rev-parse HEAD failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read the upstream package.json `version` field (used as our game/data version).
pub fn upstream_package_version(root: &Path) -> Result<String> {
    let pkg_path = root.join("package.json");
    let text = std::fs::read_to_string(&pkg_path)
        .with_context(|| format!("reading {}", pkg_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text).context("parsing package.json")?;
    Ok(v["version"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_string()))
}
