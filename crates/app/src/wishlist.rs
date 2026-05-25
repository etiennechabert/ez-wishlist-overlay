//! `wishlist.json` — the OCR-captured upgrade entries that replace the
//! stale upstream `data.json` recipes as the source of truth for what
//! the user wants to track.
//!
//! Schema: a flat map keyed by the upgrade's display title + level
//! ("Storage Room A LV0"). Re-screenshotting the same upgrade overwrites
//! its entry, which lets the user keep things fresh as they collect
//! items (a future iteration will OCR the per-cell progress digits too).
//!
//! Atomic save: same temp-file-then-rename pattern as `persist::save`
//! and `settings::save` — never leaves a half-written file on disk.

use crate::ocr::parse::CapturedUpgrade;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;
use time::OffsetDateTime;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Wishlist {
    #[serde(default)]
    pub schema_version: u32,
    /// Keyed by `CapturedUpgrade::key` ("Storage Room A LV0"). BTreeMap
    /// for stable JSON output across saves (diff-friendly).
    #[serde(default)]
    pub entries: BTreeMap<String, WishlistEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WishlistEntry {
    /// RFC3339 timestamp of the most recent capture for this upgrade.
    /// On overwrite this advances; the prior values are simply replaced.
    pub captured_at: String,
    /// Original screenshot path (for debugging — lets us re-OCR with a
    /// new parser if logic changes).
    pub source_screenshot: String,
    /// The structured parse result. See [`CapturedUpgrade`].
    pub upgrade: CapturedUpgrade,
    /// Raw line-by-line text from OCR — preserved so improvements to
    /// the parser can be re-run against historical captures without
    /// needing the original screenshot.
    pub raw_ocr_text: String,
}

/// Read wishlist.json. Missing or unreadable file → empty wishlist;
/// corrupt JSON → empty wishlist with a warning logged (we don't back
/// up here because the file is purely additive — losing one capture
/// just means the user takes another screenshot).
pub fn load(path: &Path) -> Wishlist {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Wishlist::default(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read wishlist.json; using empty wishlist");
            return Wishlist::default();
        }
    };
    match serde_json::from_str::<Wishlist>(&raw) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "wishlist.json was unparseable; using empty wishlist");
            Wishlist::default()
        }
    }
}

pub fn save(path: &Path, wishlist: &Wishlist) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(wishlist).context("serializing wishlist")?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Insert or overwrite an entry. Returns the previous entry if there
/// was one, so the caller can diff for drift logging.
pub fn upsert(
    wishlist: &mut Wishlist,
    upgrade: CapturedUpgrade,
    raw_ocr_text: String,
    source_screenshot: &Path,
) -> Option<WishlistEntry> {
    wishlist.schema_version = SCHEMA_VERSION;
    let entry = WishlistEntry {
        captured_at: now_rfc3339(),
        source_screenshot: source_screenshot.display().to_string(),
        upgrade: upgrade.clone(),
        raw_ocr_text,
    };
    wishlist.entries.insert(upgrade.key.clone(), entry)
}

fn now_rfc3339() -> String {
    let st = SystemTime::now();
    OffsetDateTime::from(st)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::parse::{CapturedItem, CapturedUpgrade};
    use std::path::PathBuf;

    fn cap(key: &str, items: u32) -> CapturedUpgrade {
        CapturedUpgrade {
            key: key.to_string(),
            title: key.split_whitespace().take(3).collect::<Vec<_>>().join(" "),
            level: "LV0".to_string(),
            cost: Some(80_000),
            items: (0..items)
                .map(|i| CapturedItem {
                    name: format!("Item {i}"),
                    collected: None,
                    needed: None,
                })
                .collect(),
        }
    }

    #[test]
    fn upsert_overwrites_existing_key() {
        let mut w = Wishlist::default();
        let prev = upsert(
            &mut w,
            cap("Storage Room A LV0", 4),
            "first".into(),
            &PathBuf::from("a.jpg"),
        );
        assert!(prev.is_none());
        assert_eq!(w.entries.len(), 1);

        let prev = upsert(
            &mut w,
            cap("Storage Room A LV0", 3),
            "second".into(),
            &PathBuf::from("b.jpg"),
        );
        assert!(prev.is_some(), "second insert should return the first entry");
        assert_eq!(w.entries.len(), 1, "same key shouldn't duplicate");
        assert_eq!(w.entries["Storage Room A LV0"].upgrade.items.len(), 3);
    }
}
