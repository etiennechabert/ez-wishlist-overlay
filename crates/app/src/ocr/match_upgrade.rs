//! Resolve an OCR'd upgrade title to a specific `Upgrade.id` in `data.json`.
//!
//! Strict matching against `module.name`, per Phase 0 invariant
//! (`hideout_screenshots/CLAUDE.md`): the row-label text in-game is
//! canonical, equals `module.name` for every module. We tolerate OCR
//! character errors via Levenshtein distance — never tolerate dataset
//! drift via fallbacks to `upgrade.name`, `module.id`, or synonym shims.

use crate::data::GameData;
use strsim::normalized_levenshtein;

/// Minimum normalized-Levenshtein score for a strict match. 1.0 is
/// identical. 0.80 accommodates a single character OCR error even on the
/// shortest module name in the dataset ("Sofa" = 4 chars, 1 sub → 0.75
/// would be the floor; 0.80 gives a sliver of safety while still firmly
/// rejecting header-divergence cases like "Kitchen" → "Kitchen Area"
/// (score 0.583) and "Moreitem" → "Procurement System" (well below).
/// Tighter than a noise-tolerant matcher would use, because the only
/// fuzziness we permit here is genuine OCR character error.
const MIN_SCORE: f64 = 0.80;

/// Resolve `(title_text, current_level)` to an `Upgrade.id`.
///
/// `title_text` is the OCR'd row-label text. `current_level` is the
/// integer from the header's `LV<digit>` token; the target upgrade is
/// `level == current_level + 1`. Returns `None` if no module matches
/// strictly, or if the matched module has no upgrade at the target
/// level (i.e. already maxed out).
pub fn resolve(data: &GameData, title_text: &str, current_level: u32) -> Option<String> {
    let normalized_input = normalize(title_text);
    if normalized_input.is_empty() {
        tracing::debug!("match_upgrade: empty OCR title");
        return None;
    }

    let target_level = current_level + 1;
    let mut best: Option<(&str, f64)> = None;
    for module in &data.modules {
        let score = normalized_levenshtein(&normalized_input, &normalize(&module.name));
        if score >= MIN_SCORE && best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((module.id.as_str(), score));
        }
    }
    let (module_id, score) = match best {
        Some(b) => b,
        None => {
            tracing::debug!(
                ocr_title = title_text,
                "match_upgrade: no module name matched (min_score = {MIN_SCORE})",
            );
            return None;
        }
    };

    let module = data.modules.iter().find(|m| m.id == module_id)?;
    let upgrade = module.upgrades.iter().find(|u| u.level == target_level);
    match upgrade {
        Some(u) => {
            tracing::debug!(
                ocr_title = title_text,
                module = %module.name,
                score = score,
                target_level,
                resolved = %u.id,
                "match_upgrade: resolved",
            );
            Some(u.id.clone())
        }
        None => {
            tracing::debug!(
                ocr_title = title_text,
                module = %module.name,
                target_level,
                "match_upgrade: module matched but no upgrade at target level (maxed out?)",
            );
            None
        }
    }
}

/// Lowercase, strip non-alphanumeric except spaces, collapse whitespace.
fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_space = true;
    for c in lower.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            last_space = false;
        } else if c.is_whitespace() && !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{GameData, HideoutModule, Upgrade};

    fn module(id: &str, name: &str, max_level: u32) -> HideoutModule {
        HideoutModule {
            id: id.into(),
            name: name.into(),
            upgrades: (1..=max_level)
                .map(|lv| Upgrade {
                    id: format!("{id}Lv{lv}"),
                    name: name.into(),
                    level: lv,
                    description: String::new(),
                    requirements: Vec::new(),
                })
                .collect(),
        }
    }

    fn fixture() -> GameData {
        GameData {
            data_version: "test".into(),
            scraped_at: "test".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: vec![
                module("RestRoom", "Toilet", 3),
                module("CryptoMining", "Bitcoin Mine", 4),
                module("KitchenArea", "Kitchen Area", 4),
                module("Moreitem", "Procurement System", 2),
                module("StorageZoneLock1", "Storage Room A", 1),
            ],
            vendors: Vec::new(),
            items: Vec::new(),
        }
    }

    #[test]
    fn exact_match_resolves_to_target_level() {
        let data = fixture();
        assert_eq!(resolve(&data, "Toilet", 0), Some("RestRoomLv1".into()));
        assert_eq!(resolve(&data, "Toilet", 1), Some("RestRoomLv2".into()));
        assert_eq!(resolve(&data, "Toilet", 2), Some("RestRoomLv3".into()));
    }

    #[test]
    fn ocr_noise_within_tolerance() {
        let data = fixture();
        // 1-character substitution
        assert_eq!(resolve(&data, "Toi1et", 0), Some("RestRoomLv1".into()));
        // Multi-word with whitespace garbling
        assert_eq!(
            resolve(&data, "Bitcoin  Mine", 0),
            Some("CryptoMiningLv1".into())
        );
        assert_eq!(
            resolve(&data, "Kitchen Area", 0),
            Some("KitchenAreaLv1".into())
        );
    }

    #[test]
    fn max_level_returns_none() {
        let data = fixture();
        assert_eq!(resolve(&data, "Storage Room A", 1), None);
    }

    #[test]
    fn unrelated_text_returns_none() {
        let data = fixture();
        assert_eq!(resolve(&data, "Hello world", 0), None);
        assert_eq!(resolve(&data, "", 0), None);
    }

    #[test]
    fn rejects_header_shortform_that_is_not_module_name() {
        // Phase 0 finding: in-panel header text "Kitchen" diverges from
        // module.name "Kitchen Area". The resolver matches against
        // `module.name`, so passing the unreliable header text should
        // not work — we want the caller to pass the row-label text.
        let data = fixture();
        // "Kitchen" vs "kitchen area" — Levenshtein 0.5, below threshold.
        assert_eq!(resolve(&data, "Kitchen", 0), None);
        // Header "Moreitem" vs module.name "Procurement System" — well
        // below threshold; resolver refuses.
        assert_eq!(resolve(&data, "Moreitem", 0), None);
    }
}
