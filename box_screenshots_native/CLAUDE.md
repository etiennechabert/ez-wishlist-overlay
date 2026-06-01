# Box-scan native captures (regression fixtures)

Real in-game **container-screen** captures for the box-scan OCR
(`crates/app/src/ocr/box_scan.rs`), pulled from the SteamVR compositor
mirror texture. These are the ground-truth fixtures the box-scan is
regression-tested against — the live captures otherwise get flushed from
`<data_dir>/debug/vr_screenshots/` on every app launch.

## What's here

Two scans, each a series of overlapping scroll captures:

| Scan | Files | In-game screen |
|---|---|---|
| `junkbox` | `junkbox.shot00.webp` … `shot09.webp` (10) | the JUNK BOX — tiles have a per-item **category subtitle** (Tool / Household / …), no tab strip |
| `big` | `big.shot0.webp` … `shot2.webp` (3) | a secondary container tablet — a **category tab strip** (All / Medical / … / Tool), names only |

Per capture there are three files:

- **`<stem>.webp`** — the 3096×3312 mirror-texture frame, **WebP q95**. The raw
  PNGs are ~11 MB each (141 MB total); q95 is ~1 MB each (~14 MB total) and
  keeps the captures self-contained in git. It's mildly lossy, but the canonical
  test input is the frozen OCR below (not the image), and q95 was verified to
  produce the identical box-scan output and preserve every item name vs the
  lossless PNG — only OCR *noise* words differ. Regenerate the OCR from these
  with `regen_box_fixtures` (Windows, `--ignored`).
- **`<stem>.boxes.json`** — the **frozen Windows.Media.Ocr output** (word boxes
  + image height) for that capture. `read_tiles` / `stitch` are pure and
  platform-independent, so these let the post-OCR pipeline be regression-tested
  on **every** target (Linux CI included) without re-running the Windows-only,
  nondeterministic engine. Regenerate after adding/replacing a capture:

  ```
  cargo test -p ez-wishlist-overlay regen_box_fixtures -- --ignored   # Windows only
  ```

  (`BOX_FIXTURE_DIR=<dir>` points it at a scratch copy — handy for comparing OCR
  across image formats.)

- **`<scan>.label.txt`** — ground-truth tally (`<item_id>  <count>`), the
  correct answer the scan should produce. `big.label.txt` is complete;
  `junkbox.label.txt` is a **best-effort draft** (the box is large — verify it
  against the in-game box before relying on it).

## Status

The regression tests (`big_container_scan_matches_label`,
`junkbox_scan_matches_label` in `box_scan.rs` `mod tests`) are **`#[ignore]`d**:
the box-scan currently mis-reads both real layouts —

- **JUNK BOX → 0 items**: the per-item category subtitles are mistaken for the
  fixed tab strip, and the whole grid is dropped.
- **Big → 6 of ~22**: the stitch refuses later shots because `read_tiles` emits
  the overlap row in an unstable order across captures.

Un-ignore them once `read_tiles` is made layout-aware and produces a stable
reading order (tracked in the box-scan fix issue). The labels here are the
target.
