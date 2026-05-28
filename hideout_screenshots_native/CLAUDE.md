# Native VR Captures

Lossless ~3K-per-eye PNGs of the Facility Upgrade panel pulled directly
from the SteamVR compositor mirror texture (see
`crates/app/src/vr/capture.rs`). Filenames follow the same convention as
`hideout_screenshots/`: `<UpgradeId>.png` where the stem matches an
`Upgrade.id` in `crates/app/src/assets/data.json`.

## Why this folder exists

The JPGs in `hideout_screenshots/` cover identification and cell-ordering
regression on the OCR pipeline but are Steam F12 captures — JPEG
compression destroys the chunky pixel-art digit font and they're useless
for digit-template work. The PNGs here are the **ground truth** for:

1. **Per-digit template extraction** — `0.png` … `9.png` + `slash.png`
   under `crates/app/src/assets/ocr_templates/`. The digits visible on
   the cell counters (e.g. "3/5") are the source. The Y-side of the
   slash is `data.json`'s `requirements[i].quantity`, so once an
   upgrade is identified by the pipeline we know which digits to
   expect on the right and can label connected components accordingly.
2. **Future digit-accuracy regression test**, once a sibling labels file
   exists alongside (covering owned/needed values per cell).

## Convention

- Filename: `<UpgradeId>.png` (e.g. `BookcaseLv1.png`).
- One file per upgrade-Id. Duplicates are deduped during curation; pick
  the cleanest panel (head steady, full panel visible, both digits in
  every cell legible).
- Gitignored by default (PNGs are ~10 MB each). Selectively commit
  individual fixtures via `git add -f` when the integration test grows
  to assert digit accuracy.

## How they're produced

In-app: press **Space** while the desktop window has focus (or click the
"Capture VR screenshot" button in the Debug dialog). The capture is
written to `<data_dir>/vr_screenshots/<timestamp>_<nanos>.png` (run the
app once; it logs the path in the green "Capture saved" toast and the
`captured compositor mirror` info-line).

After capturing a session's worth, identify and dedup with the OCR
pipeline (run via your own diagnostic; no built-in CLI yet) and copy the
chosen one per upgrade here, renamed to `<UpgradeId>.png`.
