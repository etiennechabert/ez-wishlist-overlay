# Native VR Captures

~3K-per-eye **WebP (quality 99)** captures of the Facility Upgrade panel
pulled directly from the SteamVR compositor mirror texture (see
`crates/app/src/vr/capture.rs`). Filenames follow the same convention as
`hideout_screenshots/`: `<UpgradeId>.webp` where the stem matches an
`Upgrade.id` in `crates/app/src/assets/data.json`.

Originally committed as lossless PNGs (~9 MB each, ~135 MB total); since
issue #110 they are WebP q99 (~0.9 MB each, ~13 MB total — a ~90% cut).
q99 is the highest lossy setting that still clears the digit-OCR floor:
the chunky pixel-art counters are fragile under compression, so q99
reads ~45-46/59 on `owned_count_accuracy_floor_on_native_pngs` (vs 48/59
on the lossless originals) — at the gate but passing. Lower qualities
(q98 and below) drop below the floor. **If you need a wider accuracy
margin, re-encode lossless** (`cwebp -lossless`, ~70-90 MB) rather than
lowering the floor.

## Why this folder exists

The JPGs in `hideout_screenshots/` cover identification and cell-ordering
regression on the OCR pipeline but are Steam F12 captures — JPEG
compression destroys the chunky pixel-art digit font and they're useless
for digit-template work. The captures here are the **ground truth** for:

1. **Per-digit template extraction** — `0.png` … `9.png` + `slash.png`
   under `crates/app/src/assets/ocr_templates/`. The digits visible on
   the cell counters (e.g. "3/5") are the source. The Y-side of the
   slash is `data.json`'s `requirements[i].quantity`, so once an
   upgrade is identified by the pipeline we know which digits to
   expect on the right and can label connected components accordingly.
2. **Digit-accuracy + identification regression** — each capture has a
   hand-labelled sibling `<UpgradeId>.label.txt` (owned/needed per
   cell). The `owned_count_accuracy_floor_on_native_pngs` and
   `identification_and_cell_ordering_on_native_pngs` tests in
   `crates/app/src/ocr/pipeline.rs` sweep the whole set on every run.

## Convention

- Filename: `<UpgradeId>.webp` (e.g. `BookcaseLv1.webp`), paired with a
  `<UpgradeId>.label.txt` ground-truth file.
- One file per upgrade-Id. Duplicates are deduped during curation; pick
  the cleanest panel (head steady, full panel visible, both digits in
  every cell legible).
- Committed (not gitignored) — they're the OCR pipeline's regression
  set. Only the `*.cell*.png` / `*.ocr-debug.*.txt` debug dumps the
  pipeline drops next to them in debug builds are gitignored.

## How they're produced

In-app: press **Space** while the desktop window has focus (or click the
"Capture VR screenshot" button in the Debug dialog). The capture is
written to `<data_dir>/debug/vr_screenshots/<timestamp>_<nanos>.png`
(run the app once; it logs the path in the green "Capture saved" toast
and the `captured compositor mirror` info-line). The `debug/` bundle is
cleared on every launch, so copy out anything you want to keep before
restarting.

After capturing a session's worth, identify and dedup with the OCR
pipeline (run via your own diagnostic; no built-in CLI yet), then encode
the chosen one per upgrade to WebP q99 and save it here as
`<UpgradeId>.webp` (e.g. `cwebp -q 99 in.png -o BookcaseLv1.webp`, or
Pillow `Image.save(..., "WEBP", quality=99, method=6)`).
