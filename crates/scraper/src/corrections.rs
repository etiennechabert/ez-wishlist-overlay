//! Local corrections for known upstream item-name bugs.
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

/// Upstream item id → corrected `name`. See module docs.
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
    // `misc_b_1battery` and `misc_1batterie_2` are TWO real items the game labels
    // "Size D Battery1" / "Size D Battery2"; upstream collapsed both to a bare
    // "Size D battery". The stash holds Battery1 (`misc_b_1battery`) — verified in
    // screenshots/stash/stash.shot00, where the tile OCRs "Size D batteryl" (the
    // trailing 1 misread as l) and elsewhere as just "Battery" (suffix dropped).
    // Because that digit is OCR-fragile, "batteryl" sits one edit from BOTH
    // "battery1" and "battery2", so the matcher ties; giving the OBSERVED id its
    // true, longer name lets match_item's longest-name tie-break land the tile on
    // `misc_b_1battery` (stash 42->44/55).
    //
    // DELIBERATELY leave `misc_1batterie_2` as upstream "Size D battery": we have
    // no capture of Battery2 (don't guess an unobserved name), AND naming it the
    // symmetric "Size D Battery2" would re-tie the two candidates and break the
    // resolution above. Do NOT "fix" it for consistency without a Battery2 capture
    // and a matcher that can read the trailing digit.
    ("misc_b_1battery", "Size D Battery1"),
];

/// Corrected display name for an upstream item: the [`NAME_CORRECTIONS`] entry
/// when one exists, otherwise the upstream name unchanged.
pub fn correct_name(id: &str, upstream_name: &str) -> String {
    NAME_CORRECTIONS
        .iter()
        .find(|(cid, _)| *cid == id)
        .map_or_else(|| upstream_name.to_string(), |(_, fixed)| fixed.to_string())
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
        assert_eq!(
            correct_name("misc_b_1battery", "Size D battery"),
            "Size D Battery1"
        );
    }

    #[test]
    fn leaves_battery2_twin_uncorrected() {
        // Load-bearing asymmetry: the Battery2 twin must keep the bare upstream
        // name so the matcher's longest-name tie-break resolves OCR-mangled
        // battery tiles to the observed Battery1 id. See NAME_CORRECTIONS comment.
        assert_eq!(
            correct_name("misc_1batterie_2", "Size D battery"),
            "Size D battery"
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
