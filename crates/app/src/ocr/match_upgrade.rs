//! Resolve an OCR'd upgrade title to a specific `Upgrade.id` in `data.json`.
//!
//! Strict matching against `module.name`, per the Phase 0 invariant
//! (see `hideout_screenshots_native/CLAUDE.md`): the row-label text
//! in-game is canonical and equals `module.name` for every module. We
//! tolerate OCR character errors via Levenshtein distance — never
//! tolerate dataset drift via fallbacks to `upgrade.name`,
//! `module.id`, or synonym shims.

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

/// Resolve `(panel_text, current_level)` to an `Upgrade.id`.
///
/// `panel_text` is whitespace-separated OCR tokens from anywhere on the
/// panel — usually the entire first-pass OCR output joined together. The
/// resolver slides a window the width of each candidate `module.name`
/// across the tokens, scoring each window with normalized Levenshtein,
/// and picks the best match across all modules. This is more robust than
/// cropping a precise "row label" rectangle (which would require pixel-
/// accurate panel bounds we can't easily derive from anchors alone) and
/// still respects the Phase 0 invariant: the only fuzziness is OCR
/// character noise within a window, not dataset drift.
///
/// `current_level` is the integer from the header's `LV<digit>` token;
/// the target upgrade is `level == current_level + 1`. Returns `None`
/// if no module matches strictly, or if the matched module has no
/// upgrade at the target level (i.e. already maxed out).
pub fn resolve(data: &GameData, panel_text: &str, current_level: u32) -> Option<String> {
    let tokens: Vec<String> = panel_text
        .split_whitespace()
        .map(normalize)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        tracing::debug!("match_upgrade: empty OCR input");
        return None;
    }

    let target_level = current_level + 1;
    // Best is `(module_id, score, target_word_count)`. Tiebreak on
    // ties (or near-ties): prefer the LONGER module.name. Reason:
    // when the OCR'd panel contains both a short and a long module
    // name that match perfectly, the long match is more specific and
    // almost always the intended one — for example, the panel for
    // "Starter's Storage Expansion" also contains the word "Storage"
    // (which scores 1.0 against the short module.name "Storage"); we
    // want the 3-word match, not the 1-word one.
    let mut best: Option<(&str, f64, usize)> = None;
    for module in &data.modules {
        let target = normalize(&module.name);
        let n = target.split_whitespace().count();
        if n == 0 || n > tokens.len() {
            continue;
        }
        for start in 0..=tokens.len() - n {
            let window = tokens[start..start + n].join(" ");
            let score = normalized_levenshtein(&window, &target);
            if score < MIN_SCORE {
                continue;
            }
            let replace = match best {
                None => true,
                Some((_, prev_score, prev_n)) => {
                    score > prev_score + 1e-9 || (score >= prev_score - 1e-9 && n > prev_n)
                }
            };
            if replace {
                best = Some((module.id.as_str(), score, n));
            }
        }
    }
    let (module_id, score, _) = match best {
        Some(b) => b,
        None => {
            tracing::debug!(
                tokens = ?tokens,
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
                ocr_input = panel_text,
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
                ocr_input = panel_text,
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
