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

- **`<scan>.label.txt`** — `<item_id>  <count>` tally. `big.label.txt` is the
  complete, verified contents (the test asserts against it). `junkbox.label.txt`
  is a **non-authoritative reference** — its captures can't be fully stitched
  (see Status), so it documents the verified contiguous run (shots 00–04) plus
  the additional item types seen in the lower shots; its test stays ignored.

## Status

Box-scan fixed for the tablet layout in issue #109. `read_tiles` is now
layout-aware and tilt-robust (de-shears the perspective, clusters into grid-row
blocks, drops both the tab strip and per-item subtitles via a category-word
rule, and emits a stable reading order). `process_box_image` also writes a
`<shot>.box-scan.txt` recognition dump next to the PNG when `ocr_debug` is on.

- **`big_container_scan_matches_label`** — **passing.** All 22 tiles read and
  stitched; matches `big.label.txt`. (Required two upstream-name corrections,
  applied in `crates/scraper/src/corrections.rs`: `misc_b_tapeplayer` →
  "Tape player", `misc_barcleaner` → "Cleaner".)
- **`junkbox_scan_matches_label`** — **still `#[ignore]`d, by the captures, not
  the code.** Each shot now reads its full 15-tile grid, but the 10 shots can't
  be stitched into one sequence: shots 00→04 share a row each (a clean scroll),
  but 04→09 have **scroll gaps** (no shared row), and the OCR **drops whole
  tiles** (e.g. shot02 loses "Tape"), which shifts columns and breaks the rigid
  overlap alignment. Un-ignoring needs better captures (every row overlapping,
  lossless) and/or a gap-tolerant stitch. The label also surfaced several more
  upstream name divergences/duplicates (Pet Shampoo, Boxed Nails, Band-aids,
  "Size D battery", "Gas can") — noted in `junkbox.label.txt`, out of scope here.
