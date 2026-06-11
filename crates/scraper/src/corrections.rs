//! Local corrections for known upstream item-data bugs (names and ids).
//!
//! Upstream (exfil-zone-assistant) is a fan-maintained catalog and occasionally
//! ships an item `name` that doesn't match the in-game label — including
//! outright duplicates where two distinct items share one name. The box-screen
//! OCR ([`crate`]'s consumer) matches the *in-game* label against `Item.name`,
//! so a wrong name makes that item unmatchable, and a duplicate name makes the
//! matcher resolve the wrong id.
//!
//! Each entry below is verified against an in-game capture committed under
//! `screenshots/box/` & `screenshots/stash/`. This mirrors how hideout module data is handled:
//! screenshots are the ground truth and the dataset is patched to match — never
//! a synonym/fallback shim in the match path. Keep the table minimal: one entry
//! per *observed* divergence, not speculative fixes for every upstream duplicate.

/// Item id (after [`correct_id`]) → corrected `name`. See module docs.
const NAME_CORRECTIONS: &[(&str, &str)] = &[
    // Upstream names this "Civil radio", colliding with the actual civil radio
    // (`misc_b_civilradio`). In-game the tile reads "Tape player" (a cassette
    // deck — see screenshots/box/box.shot1).
    ("misc_b_tapeplayer", "Tape player"),
    // Upstream names this "Household Cleaner", colliding with
    // `misc_householdleaner`. In-game the tile reads just "Cleaner" (spray
    // bottle — see screenshots/box/box.shot1 and screenshots/stash/stash.shot07).
    ("misc_barcleaner", "Cleaner"),
    // Upstream names this "Nails"; in-game the stash tile reads "Boxed Nails"
    // (the OCR sees "Boxed" + "Nail(s)" — see screenshots/stash/stash.shot02-04),
    // so the bare "Nails" name scored below threshold and the tile went
    // unmatched. The hideout panel uses item_id (count read positionally), so
    // the rename only affects the box-scan matcher + the display name.
    ("misc_b_nail", "Boxed Nails"),
    // The size-D-battery twins (ids remapped from upstream's inverted slugs —
    // see [`ID_CORRECTIONS`]): upstream collapsed both to a bare "Size D
    // battery"; in-game they are distinct, verified against captures:
    //   misc_b_battery_1 -> "Size D battery1"  (yellow SiLurGor 2-pack)
    //   misc_b_battery_2 -> "Size D battery2"  (white SiLurGor 2-pack)
    //
    // OCR note: a tile can OCR "Size D batteryl" (trailing 1 read as l).
    // match_item's confusion-aware distance makes l<->1 cheap but l<->2
    // full-cost, so "batteryl" still resolves to "Size D battery1" — the
    // symmetric names are safe. A *fully dropped* digit ("Size D battery", no
    // suffix) is genuinely ambiguous and not recoverable from the label alone.
    ("misc_b_battery_1", "Size D battery1"),
    ("misc_b_battery_2", "Size D battery2"),
];

/// Upstream item id → our item id. Applied at catalog load, before any other
/// correction — everything downstream (emitted item ids, [`NAME_CORRECTIONS`]
/// keys, icon filenames) speaks the corrected id.
///
/// Upstream ids mirror the game's internal blueprint slugs and we normally
/// preserve them 1:1. The size-D-battery twins are the exception: the game's
/// own slugs are id↔name *inverted* against its display labels
/// (`misc_1batterie_2` is "Size D battery1", `misc_b_1battery` is "Size D
/// battery2"), which kept causing name/icon mix-ups (#126). We rename both so
/// that id, display name and icon file all say the same thing.
const ID_CORRECTIONS: &[(&str, &str)] = &[
    ("misc_1batterie_2", "misc_b_battery_1"), // "Size D battery1" (yellow pack)
    ("misc_b_1battery", "misc_b_battery_2"),  // "Size D battery2" (white pack)
];

/// Corrected display name for an upstream item: the [`NAME_CORRECTIONS`] entry
/// when one exists, otherwise the upstream name unchanged.
pub fn correct_name(id: &str, upstream_name: &str) -> String {
    NAME_CORRECTIONS
        .iter()
        .find(|(cid, _)| *cid == id)
        .map_or_else(|| upstream_name.to_string(), |(_, fixed)| fixed.to_string())
}

/// Our id for an upstream item: the [`ID_CORRECTIONS`] entry when one exists,
/// otherwise the upstream id unchanged.
pub fn correct_id(upstream_id: &str) -> &str {
    ID_CORRECTIONS
        .iter()
        .find(|(uid, _)| *uid == upstream_id)
        .map_or(upstream_id, |(_, fixed)| fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrects_known_divergences() {
        assert_eq!(
            correct_name("misc_b_tapeplayer", "Civil radio"),
            "Tape player"
        );
        assert_eq!(
            correct_name("misc_barcleaner", "Household Cleaner"),
            "Cleaner"
        );
        assert_eq!(correct_name("misc_b_nail", "Nails"), "Boxed Nails");
    }

    #[test]
    fn remaps_battery_twin_ids() {
        // Upstream's slugs are id↔name inverted; ours match the labels.
        assert_eq!(correct_id("misc_1batterie_2"), "misc_b_battery_1");
        assert_eq!(correct_id("misc_b_1battery"), "misc_b_battery_2");
        assert_eq!(correct_id("misc_b_nail"), "misc_b_nail");
    }

    #[test]
    fn disambiguates_size_d_battery_twins() {
        // Both ship from upstream as a bare "Size D battery"; names are keyed
        // by the corrected ids (correct_id runs first, at catalog load).
        assert_eq!(
            correct_name("misc_b_battery_1", "Size D battery"),
            "Size D battery1"
        );
        assert_eq!(
            correct_name("misc_b_battery_2", "Size D battery"),
            "Size D battery2"
        );
    }

    #[test]
    fn passes_through_uncorrected_names() {
        assert_eq!(
            correct_name("misc_b_civilradio", "Civil radio"),
            "Civil radio"
        );
        assert_eq!(correct_name("misc_oilcan", "Oil can"), "Oil can");
    }
}
