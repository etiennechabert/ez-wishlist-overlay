# Open questions & deferred decisions

Anything in this list is either non-blocking for the current phase or genuinely needs human input.

---

## Phase 3 — VR overlay

### OpenVR Rust binding (the big one)

The crates.io ecosystem for OpenVR is sparse:

- `ovr_overlay` only publishes `0.0.0` on crates.io; the active development is on GitHub (`Niedzwiedzw/ovr_overlay`). Using it means a git dependency — pin a commit.
- `openvr` (the original crate) hasn't been updated in years and may not reflect the modern `IVROverlay` v25+ surface.
- Raw `openvr-sys` + hand-written wrapper is the most work but the most stable.
- A new option to evaluate: build a tiny C shim that calls `IVROverlay_*` and bind it via `bindgen`. Possibly overkill.

**Recommendation:** start with the GitHub `ovr_overlay` pin to a known commit; fall back to a hand-rolled binding if it doesn't expose `SetOverlayRaw` cleanly. Document the chosen approach in `SPEC.md §2` once decided.

### Anchor position & overlay orientation

SPEC.md §7.2 says "HMD-relative, 1.2m forward, tilted so the surface faces the user". The exact transform matrix needs trial-and-error on a real headset. Expose tweakable constants in `vr/pose.rs` (or a new `vr/anchor.rs`) so iteration is cheap.

### `SetOverlayRaw` color format

OpenVR's `SetOverlayRaw` expects RGBA8 in a specific channel order — `tiny-skia`'s pixmap stores premultiplied BGRA on little-endian platforms (need to verify). May require a channel swizzle before submission.

---

## Phase 4 — VR input

### Haptic action identifier

`TriggerHapticVibrationAction` requires an action bound via the OpenVR Input system, which needs an action manifest JSON shipped alongside the binary. Confirm: do we author one, or is there a default "haptic" action we can use?

### Cycle-to-0 UX guardrail

Current rule: clicking an item already at target resets to 0 with a distinct haptic. If the user fat-finger clicks twice (debounce should catch ≤100ms, but bumper-click can be slower), the second click resets after the first incremented to target. Consider an additional ≥300ms guard between "reached target" and "accepts reset click", with an animation/flash to indicate "ready to reset".

---

## Data pipeline

### Task objective parsing — remaining coverage

After the regex passes, 65 task objectives don't resolve to a structured `(item_id, quantity)` requirement (see `crates/app/src/assets/unparsed-objectives.log`). The bulk fall into two categories:

1. **Category submissions** ("Turn in 3 Electric Items", "Turn in 9 Intel Items") — the game accepts any item from a category, not a specific item. Modeling these requires either a virtual "category item" abstraction or per-task allow-lists derived from `subcategory`. **Suggested approach:** add a `category_alias` map in `crates/scraper/src/tasks.rs` for the ~10 known categories ("Electric", "Intel", "Household", etc.) that maps to the upstream item `subcategory` value, and treat the requirement as "any item with subcategory X, qty N". The app's `active_items()` would need a small extension.

2. **Specific-key submissions** ("Turn in hotel 208 Key") — these are real items but the names don't lowercase-match because the upstream catalog uses different capitalization or short codes. **Suggested approach:** extend `resolve_item_name` with a fuzzy match (Levenshtein) gated to keys (`subcategory == "Key"`) so we don't over-match elsewhere.

Neither is urgent — tasks without resolved requirements are dropped, the user simply can't track those specific quests via the overlay. Hideout coverage is complete.

### In-app data refresh

Currently the data is baked in at compile time. After a wipe, users must download a new release. Acceptable for v1 but worth revisiting: a `--data-from <path>` runtime override would let power-users pre-test new data without a release cut.

---

## Distribution / build infra

### Toolchain pin

Right now there's no `rust-toolchain.toml`. The repo builds with both MSVC and gnullvm. For CI reproducibility, eventually pin one — gnullvm is friendlier for contributors on machines without VS Build Tools, MSVC is the conventional Windows release path.

### Code signing

SPEC.md §12 says v1 ships unsigned and users will see SmartScreen. Reconsider once usage justifies cert cost. Until then, document the SmartScreen "More info → Run anyway" flow in the README install section.

### MSI installer ergonomics

`cargo-wix` needs a `wix/main.wxs`. Defer to Phase 5; mention now so it doesn't surprise us:
- Per-user vs per-machine install (per-user avoids elevation).
- Whether to add a Start Menu shortcut for the *scraper* (no — it's maintainer-only).
- Default associate `.json` with anything? (no).

---

## Spec ambiguities discovered during build

- §7.3 mentions text rendering "via fontdue" — the current CPU renderer omits text (numbers in the progress chip are visual via the bar fill only). Add fontdue glyph blitting in Phase 3 once we know exact font sizes feel right in-headset.
- §8.2 "Save errors logged, surfaced as a yellow banner". Implemented for *load* errors; save errors currently only log. Wire the save-thread to a channel that surfaces to the GUI banner.
- §10 #9 "controller bumper" pagination — not wired anywhere yet; for v1 just cap at `MAX_CELLS` (currently 36) and show a small "+N more" badge. Real pagination = post-v1.
