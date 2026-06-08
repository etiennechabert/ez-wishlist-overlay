//! Repo invariant: the app version is declared **once** — `crates/app/Cargo.toml`'s
//! `version` — and every other in-file copy derives it (the in-app About screen
//! via `env!("CARGO_PKG_VERSION")`, the MSI via cargo-wix's `$(var.Version)`).
//!
//! A bump that left a *hardcoded* copy behind would ship an installer or About
//! screen whose advertised version disagrees with the binary — the same class
//! of version drift that produced the dangling `v0.4.2` / `v0.4.4` release tags
//! (tag vs. Cargo.toml) the pre-push hook + `scripts/release.ps1` now guard.
//! These tests fail at PR time if a literal version sneaks into a file that's
//! supposed to derive it.

use std::fs;
use std::path::{Path, PathBuf};

/// Single source of truth: `crates/app/Cargo.toml`'s `version`, baked in at
/// compile time. Nothing else may duplicate this string.
const CANONICAL: &str = env!("CARGO_PKG_VERSION");

fn read(rel: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The version must be plain `X.Y.Z` integers: `scripts/release.ps1` and the
/// pre-push hook derive the git tag `v{version}` from it, and `ci.yml`'s
/// version-bump gate sorts versions — anything else breaks that machinery.
#[test]
fn canonical_version_is_plain_semver() {
    let parts: Vec<&str> = CANONICAL.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "crate version {CANONICAL:?} must be X.Y.Z (three dot-separated components)"
    );
    for p in &parts {
        assert!(
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()),
            "crate version {CANONICAL:?} component {p:?} is not a plain integer"
        );
    }
}

/// The MSI Product version must stay the cargo-wix preprocessor variable so the
/// installer's version is substituted from Cargo.toml at build time. A
/// hardcoded literal here lets the installer drift from the binary.
#[test]
fn msi_version_derives_from_crate() {
    let wxs = read("wix/main.wxs");
    assert!(
        wxs.contains("$(var.Version)"),
        "crates/app/wix/main.wxs must set the Product Version to $(var.Version) \
         so cargo-wix derives it from Cargo.toml"
    );
    assert!(
        !wxs.contains(CANONICAL),
        "crates/app/wix/main.wxs contains the literal version {CANONICAL:?} — the \
         MSI version must come from $(var.Version), never a hardcoded copy"
    );
}

/// The desktop UI renders the version in two places (the About modal and the
/// title bar); both must read it from `env!("CARGO_PKG_VERSION")` rather than
/// embed a literal that goes stale on the next bump.
#[test]
fn ui_version_is_derived_not_hardcoded() {
    for rel in ["src/gui/about_dialog.rs", "src/gui/mod.rs"] {
        let src = read(rel);
        assert!(
            src.contains("env!(\"CARGO_PKG_VERSION\")"),
            "{rel} must display the version via env!(\"CARGO_PKG_VERSION\")"
        );
        assert!(
            !src.contains(CANONICAL),
            "{rel} contains the literal version {CANONICAL:?} — render it from \
             env!(\"CARGO_PKG_VERSION\") instead so it can't drift"
        );
    }
}
