//! Resolve an OCR'd box-screen tile label to a specific `Item.id` in `data.json`.
//!
//! Sibling of [`crate::ocr::match_upgrade`], but for the *box container*
//! screen: each tile shows an item icon with its name below. We OCR the label,
//! normalize it, and pick the closest `Item.name` by normalized Levenshtein
//! distance — tolerating OCR character error, never tolerating dataset drift
//! (no synonym shims / id fallbacks; the in-game label is canonical and equals
//! `Item.name`).
//!
//! Unlike `match_upgrade`, the caller has already isolated one tile's label, so
//! we compare the whole label against each candidate rather than sliding a
//! window across a token stream. Kept platform-independent (no OCR / image
//! types) so the matcher is unit-tested on every target, not just Windows.

use crate::data::{GameData, ItemId};
use strsim::normalized_levenshtein;

/// Minimum normalized-Levenshtein score (1.0 = identical) for a tile label to
/// resolve to an item. Starts level with [`crate::ocr::match_upgrade`]'s 0.80;
/// item names run longer than module names, so a single OCR character error
/// costs less here while this floor still firmly rejects near-miss neighbours.
/// Tune against real box-screen captures — if labels truncate in narrow tiles
/// we may need a prefix-aware variant rather than a looser threshold.
const MIN_SCORE: f64 = 0.80;

/// Resolve one tile's OCR'd label `tokens` to an `Item.id`.
///
/// Returns `None` when the label is empty or no item name scores at or above
/// [`MIN_SCORE`]. On a score tie, prefers the longer item name (more
/// specific), then the lexicographically smaller id, so the result is
/// deterministic regardless of `data.items` order.
pub fn match_item(data: &GameData, tokens: &[&str]) -> Option<ItemId> {
    let label = normalize(&tokens.join(" "));
    if label.is_empty() {
        return None;
    }

    // Best is `(id, score, normalized_name_len)`.
    let mut best: Option<(&str, f64, usize)> = None;
    for item in &data.items {
        let cand = normalize(&item.name);
        if cand.is_empty() {
            continue;
        }
        let score = normalized_levenshtein(&label, &cand);
        if score < MIN_SCORE {
            continue;
        }
        let cand_len = cand.chars().count();
        let replace = match best {
            None => true,
            Some((best_id, best_score, best_len)) => {
                score > best_score + 1e-9
                    || (score >= best_score - 1e-9
                        && (cand_len > best_len
                            || (cand_len == best_len && item.id.as_str() < best_id)))
            }
        };
        if replace {
            best = Some((item.id.as_str(), score, cand_len));
        }
    }

    match best {
        Some((id, score, _)) => {
            tracing::debug!(label = %label, resolved = %id, score, "match_item: resolved");
            Some(id.to_string())
        }
        None => {
            tracing::debug!(
                label = %label,
                "match_item: no item matched (min_score = {MIN_SCORE})",
            );
            None
        }
    }
}

/// Lowercase, strip non-alphanumeric except spaces, collapse whitespace.
/// Duplicated from [`crate::ocr::match_upgrade`] (which is Windows-gated) so
/// this matcher stays platform-independent and CI-testable.
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
    use crate::data::Item;

    fn item(id: &str, name: &str) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            icon_path: String::new(),
            category: None,
            subcategory: None,
            weight: None,
            price: None,
            rarity: None,
        }
    }

    fn fixture() -> GameData {
        GameData {
            data_version: "test".into(),
            scraped_at: "test".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: Vec::new(),
            items: vec![
                item("uvlight", "UV lamp"),
                item("copperwire", "Copper wire"),
                item("piezometer", "Piezometer"),
                item("oliveoil", "Olive oil"),
                item("gunoil", "Gun oil"),
                item("gunpowder", "Gunpowder"),
                item("oilcan", "Oil can"),
            ],
        }
    }

    #[test]
    fn exact_match_resolves() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["Piezometer"]).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Olive", "oil"]).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["UV", "lamp"]).as_deref(),
            Some("uvlight")
        );
    }

    #[test]
    fn tolerates_one_char_ocr_noise() {
        let data = fixture();
        // 'l' read as '1', 'o' as '0' — single substitutions on long-ish names.
        assert_eq!(
            match_item(&data, &["Piez0meter"]).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Copper", "wlre"]).as_deref(),
            Some("copperwire")
        );
    }

    #[test]
    fn distinguishes_near_neighbours() {
        let data = fixture();
        // "Gun oil" and "Gunpowder" share a prefix but resolve distinctly.
        assert_eq!(
            match_item(&data, &["Gun", "oil"]).as_deref(),
            Some("gunoil")
        );
        assert_eq!(
            match_item(&data, &["Gunpowder"]).as_deref(),
            Some("gunpowder")
        );
    }

    #[test]
    fn rejects_unrelated_and_empty() {
        let data = fixture();
        assert_eq!(match_item(&data, &["Kalashnikov"]), None);
        assert_eq!(match_item(&data, &[]), None);
        assert_eq!(match_item(&data, &["", "  "]), None);
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["olive", "OIL"]).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["Oil", "can."]).as_deref(),
            Some("oilcan")
        );
    }
}
