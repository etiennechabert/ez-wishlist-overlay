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
//! `"AR-308 DMR"`) while the catalog carries the full descriptive
//! `WFItemsStringTable` name (`"Cobra 20mm reflex sight"`, `"AR-15 M16A1
//! pistolgrip"`, `"AR-308 7.62x51mm Design marksman rifle"`). The short name is
//! a *different real in-game string*, and it IS in the paks — the gun-part
//! data table **`GunSmithItemAdv`** carries one per `gunsmith.*` tag. Those are
//! extracted offline into each gun-part's [`Item::scan_alias`] (see
//! `CLAUDE.md`), so we match the storage label against the real short name, not
//! a fuzzy reconstruction.
//!
//! So on a gunsmith-category scan, after the strict whole-name pass misses,
//! [`match_gunsmith`] scores the label against every gun-part's `scan_alias` by
//! the same confusion-aware distance and takes the best — **rejecting a tie**:
//! 27 short names are shared by several parts in the game itself (`"M9"` across
//! an M9's lower/grip/barrel/upper, `"AR-15 DD"` for the DD stock *and*
//! handguard), so when two parts match equally we resolve to nothing rather than
//! guess. Same "reject, don't guess" contract as the strict matcher.
//!
//! ## Category scope
//!
//! Each in-game box is **category-locked** (Collection → `misc`, Medical →
//! `medical`, Gunsmith storage / Magazine & Attachments → `gunsmith`), so
//! [`match_item`] takes a `scope` — the box's category. The strict pass only
//! considers items in it (a tile can't resolve across categories), and the
//! alias pass above runs only for `scope == Some("gunsmith")`. `scope == None`
//! matches the whole catalog (a generic case, or one saved before categories).
//! The runtime derives the scope from the `ScanTarget`'s container category;
//! the gate fixtures from [`tests::scan_scope`].

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

/// Resolve one tile's OCR'd label `tokens` to an `Item.id`, scoped to the
/// container's item category.
///
/// Every in-game storage box is **category-locked** — the Collection boxes hold
/// `misc`, the Medical box `medical`, the Gunsmith → Storage and the Magazine &
/// Attachments box gun parts (`gunsmith`). `scope` is that category: the strict
/// whole-name pass only considers items in it, so a tile can never resolve
/// across categories (a garbled gun-part tile won't land on the misc "Magazine").
/// `None` matches across the whole catalog — a generic case, or one saved before
/// it carried a category.
///
/// `scope == Some("gunsmith")` additionally enables the gun-part short-name
/// (alias) pass (issue #183): that grid shows hand-authored short names, not the
/// full catalog names, so after the strict pass misses it scores the label
/// against each part's [`Item::scan_alias`].
///
/// Returns `None` when the label is empty or nothing resolves. On a strict
/// score tie, prefers the longer item name (more specific), then the
/// lexicographically smaller id, so the result is deterministic regardless of
/// `data.items` order.
pub fn match_item(data: &GameData, tokens: &[&str], scope: Option<&str>) -> Option<ItemId> {
    let label = normalize(&tokens.join(" "));
    if label.is_empty() {
        return None;
    }

    // --- Strict whole-name pass, scoped to the box's category ---
    // Best is `(id, score, normalized_name_len)`.
    let mut best: Option<(&str, f64, usize)> = None;
    for item in &data.items {
        if let Some(cat) = scope {
            if item.category.as_deref() != Some(cat) {
                continue;
            }
        }
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

    // --- Gun-part short-name pass (gunsmith-category boxes only) ---
    if scope == Some(GUNSMITH_CATEGORY) {
        if let Some(id) = match_gunsmith(data, &label) {
            return Some(id);
        }
    }

    tracing::debug!(
        label = %label,
        ?scope,
        "match_item: no item matched (min_score = {MIN_SCORE})",
    );
    None
}

/// The gunsmith-storage short-name resolution: score the tile `label` (already
/// [`normalize`]d) against every gun-part's [`Item::scan_alias`] — the exact
/// short name the storage grid shows, extracted from the game's `GunSmithItemAdv`
/// table — by the same confusion-aware distance, and take the best. **Rejects a
/// tie**: short names the game shares across parts (`"M9"`, `"AR-15 DD"`) resolve
/// to nothing rather than a coin-flip.
///
/// Compared whitespace-removed so OCR spacing slips ("AR308 Upper" vs
/// "AR308Upper") don't matter.
fn match_gunsmith(data: &GameData, label: &str) -> Option<ItemId> {
    let glued: String = label.chars().filter(|c| !c.is_whitespace()).collect();
    if glued.is_empty() {
        return None;
    }

    let mut best_score = ALIAS_MIN_SCORE - 1e-9;
    let mut best_id: Option<&str> = None;
    let mut tie = false;
    for item in gunsmith_items(data) {
        let Some(alias) = item.scan_alias.as_deref() else {
            continue;
        };
        let alias_glued: String = normalize(alias)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if alias_glued.is_empty() {
            continue;
        }
        let score = confusion_aware_score(&glued, &alias_glued);
        if score < ALIAS_MIN_SCORE {
            continue;
        }
        if score > best_score + 1e-9 {
            best_score = score;
            best_id = Some(item.id.as_str());
            tie = false;
        } else if score >= best_score - 1e-9 && best_id.is_some_and(|b| b != item.id.as_str()) {
            // Another distinct part matches just as well — the game reuses this
            // short name, so we can't tell them apart. Drop it.
            tie = true;
        }
    }

    match (best_id, tie) {
        (Some(id), false) => {
            tracing::debug!(label = %label, resolved = %id, score = best_score, "match_item: resolved (alias)");
            Some(id.to_string())
        }
        (Some(id), true) => {
            tracing::debug!(label = %label, candidate = %id, "match_item: gunsmith alias tie — dropped");
            None
        }
        (None, _) => None,
    }
}

/// Gun-part catalog items (`category == "gunsmith"`).
fn gunsmith_items(data: &GameData) -> impl Iterator<Item = &crate::data::Item> {
    data.items
        .iter()
        .filter(|i| i.category.as_deref() == Some(GUNSMITH_CATEGORY))
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
            category: Some("misc".into()),
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

    /// Catalog modelled on real gun parts — `name` is the full descriptive
    /// `WFItemsStringTable` name, `scan_alias` the short label the Gunsmith →
    /// Storage grid shows (from the game's `GunSmithItemAdv` table).
    fn gun_fixture() -> GameData {
        let mut data = fixture(); // keep the misc items, to prove scoping
        data.items.extend([
            gitem(
                "gunsmith_20rail_sight_cobra",
                "Cobra 20mm reflex sight",
                Some("Cobra"),
            ),
            gitem(
                "gunsmith_ar15_pistolgrip_m16a1",
                "AR-15 M16A1 pistolgrip",
                Some("M16A1"),
            ),
            gitem(
                "gunsmith_ar15_lowerreceiver_m16",
                "M16 5.56x45mm assault rifle",
                Some("M16"),
            ),
            gitem(
                "gunsmith_g3_stock_polygreen",
                "G3 polymer Green stock",
                Some("G3 Green Stock"),
            ),
            gitem(
                "gunsmith_ak74u_mount_b18",
                "AKS74U B18 mount",
                Some("AKS74U B18"),
            ),
            gitem("gunsmith_ak_mount_rsr", "AK74N/AKMN RSR mount", Some("RSR")),
            gitem(
                "gunsmith_ump_clip_25",
                "UMP45 .45acp 25rnd magazine",
                Some("UMP45 25rd"),
            ),
            // The AR308 family — three parts with three distinct short names; the
            // bare/acronym ones are NOT a subsequence of the full name, but the
            // alias resolves them exactly.
            gitem(
                "gunsmith_ar10_upperreceiver_ar308",
                "AR-10 AR308 7.62x51mm upper receiver",
                Some("AR308 Upper"),
            ),
            gitem(
                "gunsmith_ar10_muzzle_ar308",
                "AR-10 AR308 7.62x51mm compensator",
                Some("AR308"),
            ),
            gitem(
                "gunsmith_ar10_lowerreceiver_ar308",
                "AR-308 7.62x51mm Design marksman rifle",
                Some("AR-308 DMR"),
            ),
            // Two parts the game shows the SAME short name for ("AR-15 DD") — an
            // inherent collision that must resolve to nothing.
            gitem(
                "gunsmith_arstock_stock_dd",
                "AR-15 DD stock",
                Some("AR-15 DD"),
            ),
            gitem(
                "gunsmith_ar15_handguard_dd",
                "AR-15 DD handguard",
                Some("AR-15 DD"),
            ),
        ]);
        data
    }

    #[test]
    fn exact_match_resolves() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["Piezometer"], Some("misc")).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Olive", "oil"], Some("misc")).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["UV", "lamp"], Some("misc")).as_deref(),
            Some("uvlight")
        );
    }

    #[test]
    fn tolerates_one_char_ocr_noise() {
        let data = fixture();
        // 'l' read as '1', 'o' as '0' — single substitutions on long-ish names.
        assert_eq!(
            match_item(&data, &["Piez0meter"], Some("misc")).as_deref(),
            Some("piezometer")
        );
        assert_eq!(
            match_item(&data, &["Copper", "wlre"], Some("misc")).as_deref(),
            Some("copperwire")
        );
    }

    #[test]
    fn distinguishes_near_neighbours() {
        let data = fixture();
        // "Gun oil" and "Gunpowder" share a prefix but resolve distinctly.
        assert_eq!(
            match_item(&data, &["Gun", "oil"], Some("misc")).as_deref(),
            Some("gunoil")
        );
        assert_eq!(
            match_item(&data, &["Gunpowder"], Some("misc")).as_deref(),
            Some("gunpowder")
        );
    }

    #[test]
    fn rejects_unrelated_and_empty() {
        let data = fixture();
        assert_eq!(match_item(&data, &["Kalashnikov"], Some("misc")), None);
        assert_eq!(match_item(&data, &[], Some("misc")), None);
        assert_eq!(match_item(&data, &["", "  "], Some("misc")), None);
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let data = fixture();
        assert_eq!(
            match_item(&data, &["olive", "OIL"], Some("misc")).as_deref(),
            Some("oliveoil")
        );
        assert_eq!(
            match_item(&data, &["Oil", "can."], Some("misc")).as_deref(),
            Some("oilcan")
        );
    }

    #[test]
    fn resolves_confusable_short_name() {
        // "CD" misread as "CO" (d↔o): on a 2-char name one confusion is 50% of
        // the string, yet the confusion-aware cost still clears MIN_SCORE.
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        assert_eq!(
            match_item(&data, &["CO"], Some("misc")).as_deref(),
            Some("cd")
        );
        assert_eq!(
            match_item(&data, &["CD"], Some("misc")).as_deref(),
            Some("cd")
        );
    }

    #[test]
    fn unrelated_errors_on_short_name_still_reject() {
        let mut data = fixture();
        data.items.push(item("cd", "CD"));
        // Shares nothing confusable with "CD" — must not be forced to match.
        assert_eq!(match_item(&data, &["xy"], Some("misc")), None);
    }

    #[test]
    fn resolves_a_for_d_misread() {
        // Real stash capture (shot23): "WD-40" OCRs as "wa-40" — the pixel
        // font's `d` read as `a`. a↔d is confusable (one 0.3-cost edit on the
        // 4-char normalized "wd40" → 0.925), so the tile still resolves.
        let mut data = fixture();
        data.items.push(item("misc_b_wd40", "WD-40"));
        assert_eq!(
            match_item(&data, &["wa-40"], Some("misc")).as_deref(),
            Some("misc_b_wd40")
        );
        assert_eq!(
            match_item(&data, &["WD-40"], Some("misc")).as_deref(),
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
            match_item(&data, &["Size", "D", "battery1"], Some("misc")).as_deref(),
            Some("misc_b_battery_1")
        );
        assert_eq!(
            match_item(&data, &["Size", "D", "battery2"], Some("misc")).as_deref(),
            Some("misc_b_battery_2")
        );
        // Real stash capture: the trailing "1" OCRs as "l". l↔1 is confusable
        // (cost 0.3) but l↔2 is not (1.0), so "batteryl" lands on battery1 —
        // the symmetric names don't re-tie it. See the match_item module docs
        // + the battery-twins note in screenshots/CLAUDE.md.
        assert_eq!(
            match_item(&data, &["Size", "D", "batteryl"], Some("misc")).as_deref(),
            Some("misc_b_battery_1")
        );
    }

    // ---- Gunsmith → Storage short-name matching (issue #183) ----

    #[test]
    fn gunsmith_alias_resolves_short_labels() {
        let data = gun_fixture();
        // The exact short name resolves.
        assert_eq!(
            match_item(&data, &["Cobra"], Some("gunsmith")).as_deref(),
            Some("gunsmith_20rail_sight_cobra")
        );
        // "M16A1" is the pistolgrip; the bare "M16" rifle is a different alias.
        assert_eq!(
            match_item(&data, &["M16A1"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar15_pistolgrip_m16a1")
        );
        assert_eq!(
            match_item(&data, &["M16"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar15_lowerreceiver_m16")
        );
        // Multi-word short name; OCR spacing doesn't matter (compared glued).
        assert_eq!(
            match_item(&data, &["G3", "Green", "Stock"], Some("gunsmith")).as_deref(),
            Some("gunsmith_g3_stock_polygreen")
        );
        assert_eq!(
            match_item(&data, &["AKS74UB18"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ak74u_mount_b18")
        );
        assert_eq!(
            match_item(&data, &["RSR"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ak_mount_rsr")
        );
    }

    #[test]
    fn gunsmith_resolves_ar308_family_distinctly() {
        // Three AR308 parts with three distinct game short names — each resolves
        // to its own id, including the acronym ("DMR") and bare-stem ("AR308")
        // forms that aren't a substring of the full catalog name.
        let data = gun_fixture();
        assert_eq!(
            match_item(&data, &["AR308"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar10_muzzle_ar308")
        );
        assert_eq!(
            match_item(&data, &["AR308", "Upper"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar10_upperreceiver_ar308")
        );
        assert_eq!(
            match_item(&data, &["AR-308DMR"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar10_lowerreceiver_ar308")
        );
    }

    #[test]
    fn gunsmith_tolerates_ocr_glyph_noise_in_alias() {
        // Real frozen capture: "MP5A4 upper" OCRs the 5 as S ("MPSA4"); s↔5 is
        // confusable, so the alias still resolves.
        let mut data = gun_fixture();
        data.items.push(gitem(
            "gunsmith_mp5_upperreceiver_mp5a4",
            "MP5A4 9x19mm upper receiver",
            Some("MP5A4 Upper"),
        ));
        assert_eq!(
            match_item(&data, &["MPSA4", "upper"], Some("gunsmith")).as_deref(),
            Some("gunsmith_mp5_upperreceiver_mp5a4")
        );
        // The acronym OCR'd "AR-308 OUR" (D→O, M→U) still clears the alias floor.
        assert_eq!(
            match_item(&data, &["AR-308", "OUR"], Some("gunsmith")).as_deref(),
            Some("gunsmith_ar10_lowerreceiver_ar308")
        );
    }

    #[test]
    fn gunsmith_rejects_shared_short_names() {
        let data = gun_fixture();
        // The game shows "AR-15 DD" for both the DD stock and the DD handguard —
        // an inherent collision, so resolve to nothing rather than guess.
        assert_eq!(match_item(&data, &["AR-15", "DD"], Some("gunsmith")), None);
        // Nothing close enough to any alias.
        assert_eq!(match_item(&data, &["Kalashnikov"], Some("gunsmith")), None);
        assert_eq!(match_item(&data, &["NK"], Some("gunsmith")), None);
    }

    #[test]
    fn match_scope_locks_each_box_to_its_category() {
        let data = gun_fixture();
        // A misc box/stash scan never resolves a gun part — not by the strict
        // pass (scoped out of the gunsmith catalog) nor the alias pass (which is
        // gunsmith-only). So the catalog's gun parts can't leak into misc scans.
        assert_eq!(match_item(&data, &["Cobra"], Some("misc")), None);
        assert_eq!(match_item(&data, &["M16A1"], Some("misc")), None);
        assert_eq!(match_item(&data, &["AR308"], Some("misc")), None);
        // And the reverse: a gunsmith scan never resolves a misc item — the box
        // is gun-parts-only, so the strict pass is scoped out of the misc catalog.
        assert_eq!(match_item(&data, &["Piezometer"], Some("gunsmith")), None);
        // The same misc item DOES resolve on its own (misc) scan — proof this is
        // scoping, not a broken matcher.
        assert_eq!(
            match_item(&data, &["Piezometer"], Some("misc")).as_deref(),
            Some("piezometer")
        );
        // A scopeless (legacy / generic case) scan still matches across the
        // whole catalog — back-compat for containers saved before categories.
        assert_eq!(
            match_item(&data, &["Cobra", "20mm", "reflex", "sight"], None).as_deref(),
            Some("gunsmith_20rail_sight_cobra")
        );
    }
}
