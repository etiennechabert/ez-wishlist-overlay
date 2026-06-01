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
//! `box_screenshots_native/`. This mirrors how hideout module data is handled:
//! screenshots are the ground truth and the dataset is patched to match — never
//! a synonym/fallback shim in the match path. Keep the table minimal: one entry
//! per *observed* divergence, not speculative fixes for every upstream duplicate.

/// Upstream item id → corrected `name`. See module docs.
const NAME_CORRECTIONS: &[(&str, &str)] = &[
    // Upstream names this "Civil radio", colliding with the actual civil radio
    // (`misc_b_civilradio`). In-game the tile reads "Tape player" (a cassette
    // deck — see box_screenshots_native/big.shot1).
    ("misc_b_tapeplayer", "Tape player"),
    // Upstream names this "Household Cleaner", colliding with
    // `misc_householdleaner`. In-game the tile reads just "Cleaner" (spray
    // bottle — see big.shot1 and junkbox.shot07).
    ("misc_barcleaner", "Cleaner"),
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
