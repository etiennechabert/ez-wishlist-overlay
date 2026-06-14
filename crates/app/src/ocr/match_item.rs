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
//!
//! ## Gun parts (Gunsmith → Storage), issue #183
//!
//! One container breaks the "label == `Item.name`" rule: the **Gunsmith →
//! Storage** grid shows a hand-authored *short* name (`"Cobra"`, `"M16A1"`,
//! `"AR308 Upper"`) while the catalog carries the full descriptive
//! `WFItemsStringTable` name (`"Cobra 20mm reflex sight"`, `"AR-15 M16A1
//! pistolgrip"`, `"AR-10 AR308 7.62x51mm upper receiver"`). The short name is
//! a different real in-game string with no clean pak source (older `WF_*`
//! blueprints inline it, the newer AdvanceGun blueprints resolve it via
//! string-table indirection that needs a forbidden `.usmap`), so we *match*
//! against the catalog name structurally rather than re-sourcing 600+ strings:
//!
//! The short label is the catalog name's distinctive tokens, in order, with the
//! generic ones (`"20mm"`, caliber, weapon-family prefix) dropped — i.e. a
//! concatenated, order-preserving **subsequence of the name's tokens**. So when
//! the caller flags a gunsmith-storage scan, after the strict pass misses we
//!   1. match the label against an item's [`Item::scan_alias`] (the exact short
//!      name, pinned only where structural matching is wrong or ambiguous — the
//!      AR308 family, the `DMR`/`Nrd` acronym/abbreviation cases), then
//!   2. structurally align the label to each gunsmith name's tokens, taking the
//!      lowest-cost unambiguous match.
//!
//! This stays scoped to gunsmith-storage scans (the caller's `gunsmith` flag),
//! so the misc box/stash matching is byte-for-byte unchanged, and it still
//! **rejects rather than guesses**: a short label that two parts share, or that
//! aligns only loosely, resolves to nothing (an unrecognized tile) instead of a
//! wrong tally — same contract as the strict matcher.

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
/// `o`↔`0` ("piez0meter"), `i`↔`l` ("wlre" for "wire"), `a`↔`d` ("wa-40" for
/// "WD-40", stash shot23). General per-glyph cost, NOT a per-item synonym.
const CONFUSABLE: &[(char, char)] = &[
    ('o', '0'),
    ('o', 'd'),
    ('d', '0'),
    ('a', 'd'),
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

/// Category whose storage grid shows the short gun-part names (issue #183). The
/// gunsmith pass is restricted to these items so a misc box/stash tile can never
/// resolve to a gun part.
const GUNSMITH_CATEGORY: &str = "gunsmith";

/// Alias-pass floor: confusion-aware score of the tile label against an item's
/// [`Item::scan_alias`] (the exact storage short name). Level with [`MIN_SCORE`]
/// — the alias is the real string, so this only absorbs OCR glyph noise.
const ALIAS_MIN_SCORE: f64 = 0.80;

/// Structural pass — per *candidate token*, the max confusion-aware distance
/// for it to be "consumed" by an equal-length slice of the label. `1.0` lets a
/// single OCR glyph error inside one token through (`mpsa4` for `mp5a4`) while
/// still requiring the rest of the token to land.
const STRUCT_TOK_TOL: f64 = 1.0;

/// Structural pass — reject the best alignment if its summed token-edit cost
/// exceeds this. Set just below one full edit (`1.0`) so only cheap
/// [`CONFUSABLE`] glyph noise (`0.3` each, ≤ 2 of them) passes: a *full,
/// unrelated* substitution in a short distinctive token would change the part
/// (a garbled `MP5` → `MPS` must NOT resolve to the `MP9` upper), so it's
/// dropped — same "reject rather than guess" contract as the strict matcher.
/// Genuine abbreviations the name can't form at all (`AR-308DMR`) are pinned via
/// [`Item::scan_alias`] instead.
const STRUCT_COST_CAP: f64 = 0.6;

/// Structural pass — reject as **ambiguous** when the runner-up's cost is within
/// this of the winner's *and* it skipped no more leading tokens. Two parts a
/// short label can't tell apart (an `"M4 Factory"` handguard vs pistol grip,
/// `"MP5A4"` across a dozen MP5 parts) resolve to nothing, never a coin-flip.
const STRUCT_MIN_MARGIN: f64 = 0.6;

/// Structural pass — shortest glued label we'll resolve. Real gun-part short
/// names are ≥ 3 chars (`"AFG"`); a 1–2 char OCR fragment aligns to too many
/// longer tokens to be trustworthy.
const STRUCT_MIN_LABEL: usize = 3;

/// Resolve one tile's OCR'd label `tokens` to an `Item.id`.
///
/// `gunsmith` enables the gun-part short-name passes (alias + structural) — set
/// only when scanning the Gunsmith → Storage container, so misc box/stash
/// matching is unaffected (issue #183). The strict whole-name pass runs first
/// regardless and is identical to the pre-#183 behaviour.
///
/// Returns `None` when the label is empty or nothing resolves. On a strict
/// score tie, prefers the longer item name (more specific), then the
/// lexicographically smaller id, so the result is deterministic regardless of
/// `data.items` order.
pub fn match_item(data: &GameData, tokens: &[&str], gunsmith: bool) -> Option<ItemId> {
    let label = normalize(&tokens.join(" "));
    if label.is_empty() {
        return None;
    }

    // --- Strict whole-name pass (all items; misc box/stash path, unchanged) ---
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
    if let Some((id, score, _)) = best {
        tracing::debug!(label = %label, resolved = %id, score, "match_item: resolved (strict)");
        return Some(id.to_string());
    }

    // --- Gun-part short-name passes (Gunsmith → Storage only) ---
    if gunsmith {
        if let Some(id) = match_gunsmith(data, &label) {
            return Some(id);
        }
    }

    tracing::debug!(
        label = %label,
        gunsmith,
        "match_item: no item matched (min_score = {MIN_SCORE})",
    );
    None
}

/// The gunsmith-storage short-name resolution: an exact [`Item::scan_alias`]
/// match first (the pinned overrides), then a structural token-subsequence
/// alignment of the short label against each gun-part's full name. `label` is
/// already [`normalize`]d.
fn match_gunsmith(data: &GameData, label: &str) -> Option<ItemId> {
    let glued: String = label.chars().filter(|c| !c.is_whitespace()).collect();
    if glued.is_empty() {
        return None;
    }

    // 1. Alias pass — the exact storage short name, pinned where structural
    //    matching is wrong/ambiguous. Compared glued so OCR spacing slips
    //    ("AR308 Upper" vs "AR308Upper") don't matter.
    let mut best_alias: Option<(&str, f64, usize)> = None;
    for item in gunsmith_items(data) {
        let Some(alias) = item.scan_alias.as_deref() else {
            continue;
        };
        let alias_glued: String = normalize(alias).chars().filter(|c| !c.is_whitespace()).collect();
        if alias_glued.is_empty() {
            continue;
        }
        let score = confusion_aware_score(&glued, &alias_glued);
        if score < ALIAS_MIN_SCORE {
            continue;
        }
        let len = alias_glued.chars().count();
        let replace = match best_alias {
            None => true,
            Some((bid, bscore, blen)) => {
                score > bscore + 1e-9
                    || (score >= bscore - 1e-9
                        && (len > blen || (len == blen && item.id.as_str() < bid)))
            }
        };
        if replace {
            best_alias = Some((item.id.as_str(), score, len));
        }
    }
    if let Some((id, score, _)) = best_alias {
        tracing::debug!(label = %label, resolved = %id, score, "match_item: resolved (alias)");
        return Some(id.to_string());
    }

    // 2. Structural pass — the label as a concatenated, order-preserving
    //    subsequence of a name's tokens. Rank by (cost, leading-skips, name
    //    length, id); reject when too costly or ambiguous.
    let glued_chars: Vec<char> = glued.chars().collect();
    if glued_chars.len() < STRUCT_MIN_LABEL {
        return None;
    }
    // (cost, skip, ntokens, id)
    let mut scored: Vec<(f64, u32, usize, &str)> = Vec::new();
    for item in gunsmith_items(data) {
        let toks = tokenize(&item.name);
        if toks.is_empty() {
            continue;
        }
        if let Some((cost, skip)) = structural_align(&glued_chars, &toks) {
            scored.push((cost, skip, toks.len(), item.id.as_str()));
        }
    }
    scored.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(b.3))
    });
    let &(cost, skip, _, id) = scored.first()?;
    if cost > STRUCT_COST_CAP {
        return None;
    }
    let ambiguous = scored
        .get(1)
        .is_some_and(|&(c2, s2, _, _)| (c2 - cost) < STRUCT_MIN_MARGIN && s2 <= skip);
    if ambiguous {
        tracing::debug!(label = %label, resolved = %id, cost, "match_item: gunsmith ambiguous — dropped");
        return None;
    }
    tracing::debug!(label = %label, resolved = %id, cost, skip, "match_item: resolved (structural)");
    Some(id.to_string())
}

/// Gun-part catalog items (`category == "gunsmith"`).
fn gunsmith_items(data: &GameData) -> impl Iterator<Item = &crate::data::Item> {
    data.items
        .iter()
        .filter(|i| i.category.as_deref() == Some(GUNSMITH_CATEGORY))
}

/// Split a display name into normalized tokens (lowercase alnum runs).
fn tokenize(name: &str) -> Vec<Vec<char>> {
    normalize(name)
        .split_whitespace()
        .map(|t| t.chars().collect())
        .collect()
}

/// Can `label` (normalized, whitespace removed) be formed by concatenating an
/// order-preserving subsequence of `tokens`, each consumed token matching an
/// equal-length slice of the label within [`STRUCT_TOK_TOL`] confusion-aware
/// edits? Returns the best `(cost, skip)` where `cost` sums the per-token edit
/// distances and `skip` counts candidate tokens passed over *before the last
/// consumed token* (trailing generic tokens — caliber, "magazine" — are free),
/// minimizing `(cost, skip)` lexicographically. `None` if the label can't be
/// fully formed from the tokens.
fn structural_align(label: &[char], tokens: &[Vec<char>]) -> Option<(f64, u32)> {
    let n = label.len();
    let m = tokens.len();
    // best[pos][ti] = min (cost, skip) to consume label[pos..] using tokens[ti..].
    let mut best: Vec<Vec<Option<(f64, u32)>>> = vec![vec![None; m + 1]; n + 1];
    // Label fully consumed: any leftover tokens are free (not counted as skips).
    for slot in best[n].iter_mut() {
        *slot = Some((0.0, 0));
    }
    for pos in (0..n).rev() {
        for ti in (0..m).rev() {
            let mut cur: Option<(f64, u32)> = None;
            // Skip token ti.
            if let Some((c, s)) = best[pos][ti + 1] {
                cur = pick(cur, (c, s + 1));
            }
            // Consume token ti against an equal-length slice of the label.
            let tok = &tokens[ti];
            let lt = tok.len();
            if lt > 0 && pos + lt <= n {
                let dist = weighted_levenshtein(&label[pos..pos + lt], tok);
                if dist <= STRUCT_TOK_TOL {
                    if let Some((c, s)) = best[pos + lt][ti + 1] {
                        cur = pick(cur, (c + dist, s));
                    }
                }
            }
            best[pos][ti] = cur;
        }
    }
    best[0][0]
}

/// Lexicographic-min `(cost, skip)` merge for [`structural_align`]'s DP.
fn pick(acc: Option<(f64, u32)>, cand: (f64, u32)) -> Option<(f64, u32)> {
    match acc {
        None => Some(cand),
        Some(a) => {
            let better = cand.0 + 1e-9 < a.0
                || ((cand.0 - a.0).abs() <= 1e-9 && cand.1 < a.1);
            Some(if better { cand } else { a })
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
            scan_alias: None,
        }
    }

    /// A `category == "gunsmith"` catalog item, optionally with a pinned
    /// `scan_alias` (the exact storage short name).
    fn gitem(id: &str, name: &str, alias: Option<&str>) -> Item {
        Item {
            category: Some(GUNSMITH_CATEGORY.into()),
            scan_alias: alias.map(Into::into),
            ..item(id, name)
        }
    }

    fn fixture() -> GameData {
        GameData {
            data_version: "test".into(),
            scraped_at: "test".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: Vec::new(),
            research: Vec::new(),
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

    /// Catalog modelled on the real gun-part names, with the short labels the
    /// Gunsmith → Storage grid shows (the OCR reads in `screenshots/gunsmith/`).
    fn gun_fixture() -> GameData {
        let mut data = fixture(); // keep the misc items, to prove scoping
        data.items.extend([
            gitem("gunsmith_20rail_sight_cobra", "Cobra 20mm reflex sight", None),
            gitem("gunsmith_20rail_sight_walther", "Walther 20mm reflex sight", None),
            gitem("gunsmith_ar15_pistolgrip_m16a1", "AR-15 M16A1 pistolgrip", None),
            gitem("gunsmith_ar15_lowerreceiver_m16", "M16 5.56x45mm assault rifle", None),
            gitem("gunsmith_g3_stock_polygreen", "G3 polymer Green stock", None),
            gitem("gunsmith_g3_stock_polyblack", "G3 polymer Black stock", None),
            gitem("gunsmith_ak74u_mount_b18", "AKS74U B18 mount", None),
            gitem("gunsmith_ak_mount_rsr", "AK74N/AKMN RSR mount", None),
            gitem("gunsmith_scar_stock_ssr", "SECA SSR stock", None),
            gitem("gunsmith_20rail_foregrip_stormgruff", "StormGruff 20mm foregrip", None),
            // The AR308 family — three parts sharing the "AR308" stem; the bare
            // and acronym labels are pinned because structural matching picks the
            // wrong sibling (the rifle's name leads with "ar308").
            gitem("gunsmith_ar10_upperreceiver_ar308", "AR-10 AR308 7.62x51mm upper receiver", None),
            gitem("gunsmith_ar10_muzzle_ar308", "AR-10 AR308 7.62x51mm compensator", Some("AR308")),
            gitem("gunsmith_ar10_lowerreceiver_ar308", "AR-308 7.62x51mm Design marksman rifle", Some("AR-308DMR")),
            // MP5A4 parts — bare "MP5A4" can't pick among them (ambiguous).
            gitem("gunsmith_mp5_upperreceiver_mp5a4", "MP5A4 9x19mm upper receiver", None),
            gitem("gunsmith_mp5_handguard_3rail", "MP5A4 3Rail handguard", None),
            gitem("gunsmith_mp5_handguard_mp5", "MP5A4 standard handguard", None),
            // Two "M4 Factory" parts — "M4 factory" is ambiguous between them.
            gitem("gunsmith_ar15_handguard_m4factory", "AR-15 M4 Factory handguard", None),
            gitem("gunsmith_ar15_pistolgrip_m4factory", "AR-15 M4 Factory pistolgrip", None),
        ]);
        data
    }

    #[test]
    fn exact_match_resolves() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["Piezometer"], false).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Olive", "oil"], false).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["UV", "lamp"], false).as_deref(),
            Some("uvlight")
        );
    }

    #[test]
    fn tolerates_one_char_ocr_noise() {
        let data = fixture();
        // 'l' read as '1', 'o' as '0' — single substitutions on long-ish names.
        assert_eq!(
            match_item(&data, &["Piez0meter"], false).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Copper", "wlre"], false).as_deref(),
            Some("copperwire")
        );
    }

    #[test]
    fn distinguishes_near_neighbours() {
        let data = fixture();
        // "Gun oil" and "Gunpowder" share a prefix but resolve distinctly.
        assert_eq!(
            match_item(&data, &["Gun", "oil"], false).as_deref(),
            Some("gunoil")
        );
        assert_eq!(
            match_item(&data, &["Gunpowder"], false).as_deref(),
            Some("gunpowder")
        );
    }

    #[test]
    fn rejects_unrelated_and_empty() {
        let data = fixture();
        assert_eq!(match_item(&data, &["Kalashnikov"], false), None);
        assert_eq!(match_item(&data, &[], false), None);
        assert_eq!(match_item(&data, &["", "  "], false), None);
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["olive", "OIL"], false).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["Oil", "can."], false).as_deref(),
            Some("oilcan")
        );
    }

    #[test]
    fn resolves_confusable_short_name() {
        // "CD" misread as "CO" (d↔o): on a 2-char name one confusion is 50% of
        // the string, yet the confusion-aware cost still clears MIN_SCORE.
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        assert_eq!(match_item(&data, &["CO"], false).as_deref(), Some("cd"));
        assert_eq!(match_item(&data, &["CD"], false).as_deref(), Some("cd"));
    }

    #[test]
    fn unrelated_errors_on_short_name_still_reject() {
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        // Shares nothing confusable with "CD" — must not be forced to match.
        assert_eq!(match_item(&data, &["xy"], false), None);
    }

    #[test]
    fn resolves_a_for_d_misread() {
        // Real stash capture (shot23): "WD-40" OCRs as "wa-40" — the pixel
        // font's `d` read as `a`. a↔d is confusable (one 0.3-cost edit on the
        // 4-char normalized "wd40" → 0.925), so the tile still resolves.
        let mut data = fixture();
        data.items.push(item("misc_b_wd40", "WD-40"));
        assert_eq!(
            match_item(&data, &["wa-40"], false).as_deref(),
            Some("misc_b_wd40")
        );
        assert_eq!(
            match_item(&data, &["WD-40"], false).as_deref(),
            Some("misc_b_wd40")
        );
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
    fn disambiguates_size_d_battery_twins() {
        // The two "Size D battery" items, near-symmetric names one digit apart.
        let mut data = fixture();
        data.items.push(item("misc_b_battery_2", "Size D battery2"));
        data.items.push(item("misc_b_battery_1", "Size D battery1"));

        // A clean read of either full name resolves to its own id.
        assert_eq!(
            match_item(&data, &["Size", "D", "battery1"], false).as_deref(),
            Some("misc_b_battery_1")
        );
        assert_eq!(
            match_item(&data, &["Size", "D", "battery2"], false).as_deref(),
            Some("misc_b_battery_2")
        );
        // Real stash capture: the trailing "1" OCRs as "l". l↔1 is confusable
        // (cost 0.3) but l↔2 is not (1.0), so "batteryl" lands on battery1 —
        // the symmetric names don't re-tie it. See the match_item module docs
        // + the battery-twins note in screenshots/CLAUDE.md.
        assert_eq!(
            match_item(&data, &["Size", "D", "batteryl"], false).as_deref(),
            Some("misc_b_battery_1")
        );
    }

    // ---- Gunsmith → Storage short-name matching (issue #183) ----

    #[test]
    fn gunsmith_structural_resolves_short_labels() {
        let data = gun_fixture();
        // A single distinctive token, generic tokens dropped.
        assert_eq!(
            match_item(&data, &["Cobra"], true).as_deref(),
            Some("gunsmith_20rail_sight_cobra")
        );
        // Mid-name token wins over a same-stem part that leads with it: "M16A1"
        // is the pistolgrip, NOT the bare "M16" rifle (which can't consume "a1").
        assert_eq!(
            match_item(&data, &["M16A1"], true).as_deref(),
            Some("gunsmith_ar15_pistolgrip_m16a1")
        );
        // The bare rifle still resolves on its own.
        assert_eq!(
            match_item(&data, &["M16"], true).as_deref(),
            Some("gunsmith_ar15_lowerreceiver_m16")
        );
        // Multi-token subsequence, an interior generic token ("polymer") skipped,
        // and the colour disambiguates green vs black.
        assert_eq!(
            match_item(&data, &["G3", "Green", "Stock"], true).as_deref(),
            Some("gunsmith_g3_stock_polygreen")
        );
        // Glued label spanning two tokens (no spaces in the OCR read).
        assert_eq!(
            match_item(&data, &["AKS74UB18"], true).as_deref(),
            Some("gunsmith_ak74u_mount_b18")
        );
        // An exact token deep in the name beats a fuzzy near-miss elsewhere:
        // "RSR" is the mount (exact), not "SSR" stock (1 unrelated edit).
        assert_eq!(
            match_item(&data, &["RSR"], true).as_deref(),
            Some("gunsmith_ak_mount_rsr")
        );
        // "AR308Upper" / "AR308 Upper" → the upper receiver (structural).
        assert_eq!(
            match_item(&data, &["AR308Upper"], true).as_deref(),
            Some("gunsmith_ar10_upperreceiver_ar308")
        );
        assert_eq!(
            match_item(&data, &["AR308", "Upper"], true).as_deref(),
            Some("gunsmith_ar10_upperreceiver_ar308")
        );
    }

    #[test]
    fn gunsmith_scan_alias_overrides_structural() {
        let data = gun_fixture();
        // Bare "AR308" structurally favours the rifle (its name leads with the
        // token), but the pinned alias routes it to the compensator.
        assert_eq!(
            match_item(&data, &["AR308"], true).as_deref(),
            Some("gunsmith_ar10_muzzle_ar308")
        );
        // The DMR acronym isn't spelled out in the name, so only the alias
        // resolves the rifle.
        assert_eq!(
            match_item(&data, &["AR-308DMR"], true).as_deref(),
            Some("gunsmith_ar10_lowerreceiver_ar308")
        );
    }

    #[test]
    fn gunsmith_rejects_ambiguous_short_labels() {
        let data = gun_fixture();
        // Two "M4 Factory" parts, one short label — resolve to nothing, never a
        // coin-flip.
        assert_eq!(match_item(&data, &["M4", "factory"], true), None);
        // Bare "MP5A4" matches a dozen MP5A4 parts equally.
        assert_eq!(match_item(&data, &["MP5A4"], true), None);
        // A 1–2 char fragment is below the structural floor.
        assert_eq!(match_item(&data, &["NK"], true), None);
    }

    #[test]
    fn gunsmith_pass_is_scoped_to_gunsmith_scans() {
        let data = gun_fixture();
        // With the flag off (a misc box/stash scan), gun-part short labels never
        // resolve — so misc matching is unaffected by the catalog's gun parts.
        assert_eq!(match_item(&data, &["Cobra"], false), None);
        assert_eq!(match_item(&data, &["M16A1"], false), None);
        assert_eq!(match_item(&data, &["AR308"], false), None);
        // Misc items still resolve via the strict pass even on a gunsmith scan.
        assert_eq!(
            match_item(&data, &["Piezometer"], true).as_deref(),
            Some("piezometer")
        );
    }
}
