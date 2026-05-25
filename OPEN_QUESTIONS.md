# Open questions & deferred decisions

Anything in this list is either non-blocking for the current phase or genuinely needs human input.

---

## Phase 3 — VR overlay

### OpenVR Rust binding — RESOLVED

We use [`openvr` 0.9.0](https://crates.io/crates/openvr) (which pulls `openvr_sys 2.1.4`). It exposes everything we need today — `CreateOverlay`, `SetOverlayWidthInMeters`, `SetOverlayTransformTrackedDeviceRelative`, `SetOverlayAlpha`, `SetOverlayRaw`, `GetDeviceToAbsoluteTrackingPose`, and `PollNextOverlayEvent` for the Phase 4 input loop. Newer IVROverlay v25+ niceties (curvature flags, etc.) aren't required for v1.

`openvr_sys`'s build script assumes MSVC on Windows (hard-coded `/DWIN32` cxxflag), so the shipped binary builds via `x86_64-pc-windows-msvc`. The desktop-only / cross-platform branches still build under gnullvm because the entire `vr/` Windows-side is `cfg(target_os = "windows")`.

### Anchor position & overlay orientation — placeholder defaults

Defaults in [`crates/app/src/vr/anchor.rs`](crates/app/src/vr/anchor.rs): 1.2 m forward, 0.6 m above eye line, 35° tilt around the X axis (front face looks down toward viewer). All three are public consts — tune after first headset test, or expose as VR settings sliders.

### `SetOverlayRaw` color format — RESOLVED

`tiny-skia`'s `Pixmap::data()` returns RGBA8 in literal `[R, G, B, A]` byte order (premultiplied). OpenVR's `SetOverlayRaw` expects RGBA8 in the same byte order. No channel swizzle needed. Premultiplied alpha could cause haloing if the overlay's background were transparent against a complex scene, but our background fills are mostly opaque (`rgba(20,20,24,220)`), so any artefacts are negligible — revisit if needed during the manual headset pass.

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

### Toolchain pin — partially resolved

The shipped Windows binary now requires `x86_64-pc-windows-msvc` because `openvr_sys`'s build.rs hard-codes the MSVC `/DWIN32` cxxflag. macOS / Linux iteration builds still work with any toolchain because the VR layer is `cfg(target_os = "windows")`-gated. CI uses MSVC for the Windows-check job; we still don't ship a `rust-toolchain.toml` so contributors can pick their own host toolchain.

Open: should we vendor a patched `openvr_sys` to drop the MSVC dependency, or live with MSVC as the official Windows build path? cargo-dist's eventual MSI release targets MSVC anyway, so this is purely a dev-experience question.

### Code signing

SPEC.md §12 says v1 ships unsigned and users will see SmartScreen. Reconsider once usage justifies cert cost. Until then, document the SmartScreen "More info → Run anyway" flow in the README install section.

### MSI installer ergonomics

`cargo-wix` needs a `wix/main.wxs`. Defer to Phase 5; mention now so it doesn't surprise us:
- Per-user vs per-machine install (per-user avoids elevation).
- Whether to add a Start Menu shortcut for the *scraper* (no — it's maintainer-only).
- Default associate `.json` with anything? (no).

---

## Spec ambiguities discovered during build

- §7.3 mentions text rendering "via fontdue" — the current CPU renderer still omits glyphs (progress is the bar fill only). Add fontdue blits once we know what font sizes feel right in-headset.
- §8.2 "Save errors logged, surfaced as a yellow banner". Implemented for *load* errors; save errors currently only log. Wire the save-thread to a channel that surfaces to the GUI banner.
- §10 #9 "controller bumper" pagination — not wired anywhere yet; for v1 just cap at `MAX_CELLS` (currently 36) and show a small "+N more" badge. Real pagination = post-v1.
- Tick cadence: the VR loop runs at ~90 Hz via a plain `std::thread::sleep(11 ms - elapsed)` rather than `WaitGetPoses` (which is for Scene apps). Good enough for overlays; revisit if the head-tracking feels laggy in headset.
