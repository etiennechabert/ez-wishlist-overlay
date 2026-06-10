//! Resolve an OCR'd box-screen tile label to a specific `Item.id` in `data.json`.
//!
//! Sibling of [`crate::ocr::match_upgrade`], but for the *box container*
//! screen: each tile shows an item icon with its name below. We OCR the label,
//! normalize it, and pick the closest `Item.name` by a **confusion-aware**
//! normalized edit distance — tolerating the character errors the game's pixel
//! font routinely produces (a `1` read as `l`, a `D` as `O`, an `o` as `0`),
//! never tolerating dataset drift (no synonym shims / id fallbacks; the in-game
//! label is canonical and equals `Item.name`). The confusion handling is a
//! general per-glyph cost, not a per-item alias.
//!
//! Unlike `match_upgrade`, the caller has already isolated one tile's label, so
//! we compare the whole label against each candidate rather than sliding a
//! window across a token stream. Kept platform-independent (no OCR / image
//! types) so the matcher is unit-tested on every target, not just Windows.

// Reached for real only by the Windows-gated box-scan reader (`read_tiles`),
// plus the unit tests below (every target). The non-test, non-Windows build
// therefore sees the whole matcher as dead; keep it compiled cross-target for
// the tests and allow `dead_code` module-wide rather than per item.
#![allow(dead_code)]

use crate::data::{GameData, ItemId};

/// Minimum normalized-Levenshtein score (1.0 = identical) for a tile label to
/// resolve to an item. Starts level with [`crate::ocr::match_upgrade`]'s 0.80;
/// item names run longer than module names, so a single OCR character error
/// costs less here while this floor still firmly rejects near-miss neighbours.
/// Tune against real box-screen captures — if labels truncate in narrow tiles
/// we may need a prefix-aware variant rather than a looser threshold.
const MIN_SCORE: f64 = 0.80;

/// OCR glyph confusions for the game's pixel font, as unordered `(a, b)` pairs
/// over the normalized alphabet (lowercase letters + digits). A substitution
/// between a confusable pair costs [`CONFUSE_COST`] instead of a full `1.0`, so
/// a label whose only error is a glyph the OCR routinely mistakes still resolves
/// — without loosening [`MIN_SCORE`] for genuine mismatches. Observed in real
/// captures: `1`↔`l` ("batteryl" for "Battery1"), `d`↔`o` ("co" for "CD"),
/// `o`↔`0` ("piez0meter"), `i`↔`l` ("wlre" for "wire"). General per-glyph cost,
/// NOT a per-item synonym.
const CONFUSABLE: &[(char, char)] = &[
    ('o', '0'),
    ('o', 'd'),
    ('d', '0'),
    ('l', '1'),
    ('i', '1'),
    ('i', 'l'),
    ('s', '5'),
    ('b', '8'),
    ('z', '2'),
    ('g', '9'),
];

/// Substitution cost for a [`CONFUSABLE`] pair (vs `1.0` for an unrelated
/// substitution). `0.3` lets a *single* confusion clear [`MIN_SCORE`] even on a
/// two-character name (`1 − 0.3/2 = 0.85`), while two unrelated errors on a
/// short name still fall short.
const CONFUSE_COST: f64 = 0.3;

fn confusable(a: char, b: char) -> bool {
    CONFUSABLE
        .iter()
        .any(|&(x, y)| (a == x && b == y) || (a == y && b == x))
}

/// Weighted Levenshtein distance where a [`CONFUSABLE`] substitution costs
/// [`CONFUSE_COST`]; every other edit (substitution, insert, delete) costs `1.0`.
fn weighted_levenshtein(a: &[char], b: &[char]) -> f64 {
    let mut prev: Vec<f64> = (0..=b.len()).map(|j| j as f64).collect();
    let mut cur = vec![0.0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = (i + 1) as f64;
        for (j, &cb) in b.iter().enumerate() {
            let sub_cost = if ca == cb {
                0.0
            } else if confusable(ca, cb) {
                CONFUSE_COST
            } else {
                1.0
            };
            cur[j + 1] = (prev[j] + sub_cost)
                .min(prev[j + 1] + 1.0)
                .min(cur[j] + 1.0);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Confusion-aware similarity in `[0, 1]` (1.0 = identical). Mirrors
/// `strsim::normalized_levenshtein` (distance ÷ longer length) but with the
/// confusion-aware cost above, so [`MIN_SCORE`] keeps its meaning for clean
/// matches while a single font-confusion error no longer rejects a short name.
fn confusion_aware_score(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let maxlen = a.len().max(b.len());
    if maxlen == 0 {
        return 1.0;
    }
    1.0 - weighted_levenshtein(&a, &b) / maxlen as f64
}

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
        let score = confusion_aware_score(&label, &cand);
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

/// True when [`match_item`] resolves some contiguous window of `tokens` to
/// `id`.
///
/// Used by the isolated-OCR unit gate (`unit_ocr_tests` in
/// [`crate::ocr::pipeline`]) as production parity: a unit crop usually catches
/// text beside the name (the category subtitle under it, a neighbouring
/// tile's label), and the shipped box-scan reader isolates the name line
/// before matching — the unit read has no line geometry, so it instead asks
/// whether the name is recoverable from *some* window of the read. This also
/// credits reads the confusion-aware matcher already resolves in production
/// (CD OCRs as "co" yet lands on the CD item), which a raw-text comparison
/// rejects.
pub fn any_window_resolves(data: &GameData, tokens: &[&str], id: &str) -> bool {
    (0..tokens.len()).any(|start| {
        (start + 1..=tokens.len())
            .any(|end| match_item(data, &tokens[start..end]).as_deref() == Some(id))
    })
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

    #[test]
    fn resolves_confusable_short_name() {
        // "CD" misread as "CO" (d↔o): on a 2-char name one confusion is 50% of
        // the string, yet the confusion-aware cost still clears MIN_SCORE.
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        assert_eq!(match_item(&data, &["CO"]).as_deref(), Some("cd"));
        assert_eq!(match_item(&data, &["CD"]).as_deref(), Some("cd"));
    }

    #[test]
    fn unrelated_errors_on_short_name_still_reject() {
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        // Shares nothing confusable with "CD" — must not be forced to match.
        assert_eq!(match_item(&data, &["xy"]), None);
    }

    #[test]
    fn confusion_aware_score_costs_known_confusions_less() {
        // d↔o is a confusable pair: 1 − 0.3/2 = 0.85 (clears 0.80).
        assert!((confusion_aware_score("co", "cd") - 0.85).abs() < 1e-9);
        assert!((confusion_aware_score("cd", "cd") - 1.0).abs() < 1e-9);
        // An unrelated 1-char substitution on a 2-char name: 1 − 1/2 = 0.5.
        assert!((confusion_aware_score("cx", "cd") - 0.5).abs() < 1e-9);
        // Insertions stay full cost: "cd" vs "cde" = 1 − 1/3.
        assert!((confusion_aware_score("cd", "cde") - (1.0 - 1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn window_resolves_name_among_surrounding_text() {
        // A unit crop's read carries the category subtitle (and sometimes a
        // neighbour's label) around the name; some contiguous window must
        // still resolve. "co" is the production read of the CD tile (d↔o).
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        data.items.push(item("ram", "RAM"));
        assert!(any_window_resolves(&data, &["co", "intel"], "cd"));
        assert!(any_window_resolves(&data, &["ram", "electric"], "ram"));
        assert!(any_window_resolves(
            &data,
            &["gunpowder", "olive", "oil", "valuable"],
            "oliveoil"
        ));
        // The whole read resolving elsewhere isn't enough — the window must
        // land on the unit's own id.
        assert!(!any_window_resolves(&data, &["olive", "oil"], "ram"));
        assert!(!any_window_resolves(&data, &[], "ram"));
    }

    #[test]
    fn disambiguates_size_d_battery_twins() {
        // The two "Size D battery" items, with the game's id↔name inversion:
        // `misc_b_1battery` = "Size D battery2", `misc_1batterie_2` = "battery1".
        let mut data = fixture();
        data.items.push(item("misc_b_1battery", "Size D battery2"));
        data.items.push(item("misc_1batterie_2", "Size D battery1"));

        // A clean read of either full name resolves to its own id.
        assert_eq!(
            match_item(&data, &["Size", "D", "battery1"]).as_deref(),
            Some("misc_1batterie_2")
        );
        assert_eq!(
            match_item(&data, &["Size", "D", "battery2"]).as_deref(),
            Some("misc_b_1battery")
        );
        // Real stash capture: the trailing "1" OCRs as "l". l↔1 is confusable
        // (cost 0.3) but l↔2 is not (1.0), so "batteryl" lands on battery1 —
        // the symmetric names don't re-tie it. See match_item module + the
        // crates/scraper/src/corrections.rs NAME_CORRECTIONS note.
        assert_eq!(
            match_item(&data, &["Size", "D", "batteryl"]).as_deref(),
            Some("misc_1batterie_2")
        );
    }
}
