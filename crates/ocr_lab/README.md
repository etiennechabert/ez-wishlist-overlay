# ocr_lab

Isolated test bed for the ez-wishlist-overlay screenshot → wishlist OCR pipeline.

The main app captures Steam VR screenshots of the in-game **Facility Upgrade** panel and needs to extract structured data from them (title, level, cost, item cells with names + X/Y progress). This crate is where that pipeline is developed and measured in isolation — without the rest of the app — against a labeled dataset under `ocr_data/`.

## Why a separate crate

The chunky pixel-art digit font in the game is hostile to general-purpose OCR. We need targeted preprocessing, a layout grid, and per-glyph template matching. Iterating those inside the live app is slow; iterating here with `cargo run -p ocr_lab -- <cmd>` is fast and the accuracy is measured against `ocr_data/labels.json` (31 hand-labeled screenshots).

Once the pipeline reaches acceptable accuracy here, the relevant modules (`prep`, `grid`, `templates`, `pipeline`) move into `crates/app`.

## Prerequisites

- **Rust** (workspace default).
- **Tesseract OCR**. The binary `tesseract.exe` must be in PATH OR at the standard Windows install path `C:\Program Files\Tesseract-OCR\tesseract.exe`. Install on Windows:

  ```
  winget install --id UB-Mannheim.TesseractOCR
  ```

  On macOS: `brew install tesseract`. On Debian: `apt install tesseract-ocr`.
- **Image dataset** in `ocr_data/` — each `.jpg` is a screenshot; `labels.json` holds the ground truth.

## Layout

```
crates/ocr_lab/
  src/
    main.rs        — CLI entry point + per-mode handlers
    engine.rs      — Tesseract CLI wrapper, returns word-level bboxes + conf
    prep.rs        — White/green text filter → invert (black on white)
    grid.rs        — Anchor-based layout: BACK + LEVEL UP + FROM RAID find cells
    templates.rs   — Per-digit template matching for X/Y progress reading
    pipeline.rs    — Full image → Panel struct pipeline
    score.rs       — Compare predicted Panels vs ground-truth labels.json
    consola.ttf    — Embedded monospace font for the OCR debug overlay
  Cargo.toml
ocr_data/          — Dataset (lives in the repo root, not in this crate)
  *.jpg              raw screenshots
  labels.json        ground truth (committed)
  templates/         per-digit reference PNGs (extracted via tm-extract)
  crops_debug/       pipeline panel crops dumped during runs (gitignored)
  templates_debug/   tm-extract debug prep crops (gitignored)
  digits/            digit OCR per-variant outputs (gitignored)
  digits_debug/      tm digit strip dumps from pipeline (gitignored)
  prepped/           prep mode threshold sweep outputs (gitignored)
  grids/             grid mode overlays (gitignored)
  ocr_debug/         ocr-debug overlays (gitignored)
  predictions.json   most recent pipeline output (gitignored)
```

## Current pipeline

What `cargo run -p ocr_lab -- score` (and `one`) actually executes today, end-to-end. Implemented in [pipeline.rs](src/pipeline.rs) — the `grid` and `ocr-debug` CLI modes are debug visualizers for a parallel BACK/LEVEL-UP-led layout in [grid.rs](src/grid.rs) that is **not yet wired into the scoring pipeline**.

1. **Preprocess** — `prep::process` with `prep::DEFAULT` (`white_lum=175`, `green_min=130`, `green_margin=30`, `dilate=false`). Keeps only white-ish and bright-green pixels (UI text + completed-item green) and inverts to black-on-white. Done **once** on the full screenshot; reused for both OCR passes and template matching.
2. **Pass 1 — full-image sparse OCR** (Tesseract, PSM Sparse). Looks for a panel anchor:
   - **Primary**: tokens reading `Need` / `to` / `submit` / `items` on the same row → take that row's bbox and pad asymmetrically (≈18× row-height sideways, 20× above to catch title + upgrade rows, 14× below to catch cost + cells + FROM RAID).
   - **Fallback**: high-confidence words clustered by Y gap; pick the largest cluster; pad by ≈8× median height.
3. **Crop + 2× upscale** the resulting panel rect with Lanczos3 → the "panel image" the rest of the pipeline parses.
4. **Pass 2 — panel OCR** (Tesseract, PSM Block) on the upscaled panel.
5. **Locate cells via FROM RAID** — pair every "FROM"-ish token with the nearest "RAID"-ish token on the same row. From the pairs, derive:
   - `cell_top` = min FROM-RAID y; `cell_h` = strip height; each cell extends **5× cell-h upward** from the strip to capture icon + name + X/Y row.
   - Cell centers = midpoint of each FROM+RAID pair; pitch = median gap between centers.
   - **Missing-cell inference**: if fewer than 4 cells were found, extrapolate one pitch left/right if it fits inside the panel image.
   - Fallback when zero pairs are found: equal-width 4-column split of the bottom half.
6. **Per-field parsing:**
   - **Level** — first `LV<digits>` token in the upper half of the panel.
   - **Title** — tallest words in the top quarter, banded by Y, joined left-to-right; skips header bleed.
   - **Cost** — first 4–7-digit token above the cells (with `digitize()` mapping common Tesseract misreads like `s→8`, `o→0`); on miss, targeted digit-only re-OCR (PSM Line, whitelist `0123456789`) on a mid-panel strip just above the cells, picking the largest number in `[1000, 999_999]`.
   - **Item name** — alphabetic words in the upper 2/3 of each cell, joined left-to-right.
   - **Item progress X/Y** — bottom 55–88% of each cell, **template-matched** (`templates::recognize`) against per-digit reference PNGs in `ocr_data/templates/`. Tesseract can't read the chunky pixel-art digit font reliably even with binarization + whitelist; pixel-wise template matching on the same preprocessed buffer does.
7. **Score** — `score::compare` diffs the predicted `Panel` against `labels.json` per image and prints accuracy.

Known gaps the README's "Typical iteration loop" exists to close:
- Templates currently cover only 0,1,2,8,/ — digits 3,4,5,6,7 still need extraction via `tm-extract`.
- The grid.rs BACK/LEVEL-UP-led layout (better for 3-cell panels and tilted VR views) is not yet used by `pipeline::run` — it's still on the Pass-1-anchor approach above.

## CLI Modes

All commands run from the repo root.

> **Tip:** when an image argument is omitted, every mode defaults to **`20260526091314_1.jpg`** (the Gunsmith reference image — covers 0/1/2/8/slash digits + 4-cell layout). You can also drop the `.jpg` extension; it's added automatically. Pass `all` instead of an image name to run `one`, `prep`, `grid`, or `ocr-debug` over every `.jpg` in `ocr_data/`.

### `score` — score the whole dataset

```
cargo run -p ocr_lab -- score
```

Runs the full pipeline on every image in `ocr_data/labels.json`, compares against ground truth, writes `ocr_data/predictions.json`, and prints a per-image diff + an accuracy summary (title %, level %, cost %, item-count %, item names %, item needed %, item collect %).

### `one` — run on a single image

```
cargo run -p ocr_lab -- one              # uses default image
cargo run -p ocr_lab -- one 20260526091333_1
```

Runs the full pipeline on one image, prints the parsed `Panel` JSON and the score row vs ground truth (if labeled).

### `prep` — sweep preprocessing thresholds

```
cargo run -p ocr_lab -- prep             # uses default image
cargo run -p ocr_lab -- prep 20260526091506_1
```

Generates 31 preprocessed variants of one image (luminance threshold ∈ {0, 5, 10, …, 150} in steps of 5, no dilation) saved as PNGs under `ocr_data/prepped/`, then OCRs each variant and scores how many expected tokens it found vs ground truth. Use to pick the best `PrepParams` for downstream code.

Filename pattern: `<stem>.dil0_lum<NNN>_grn<NNN>m<NN>.png`. Dilation has been dropped from the sweep — in the chunky-pixel game font, dilation merges adjacent letters more than it helps fill thin slashes.

### `digit` — sweep digit-strip preprocessing on a manual crop

```
cargo run -p ocr_lab -- digit 20260526091314_1.jpg 1145,1015,110,55 2/8
```

Args: `<image> <x,y,w,h> [expected]`. Crops the rect, sweeps prep thresholds × upscale factor × PSM, runs OCR with a digit+slash whitelist on each variant, prints what each variant read, marks matches against the expected string. Used to debug the per-cell progress reader.

### `tm-extract` — extract digit templates from one cell

```
cargo run -p ocr_lab -- tm-extract 20260526091314_1.jpg 1145,1015,110,55 "2/8"
```

Preprocesses the crop, finds connected components, asserts that the component count matches the expected string length, and saves each component as a template PNG in `ocr_data/templates/` (`2.png`, `slash.png`, `8.png`). Adds to the template set used by `tm` and the pipeline.

The component-count-must-match check guards against accidentally labeling a wrong cell. If the crop has noise (cell-row separator lines, FROM RAID label), tighten the rect.

### `tm` — read a digit strip via template matching

```
cargo run -p ocr_lab -- tm 20260526091314_1.jpg 1145,1015,110,55
```

Loads all templates from `ocr_data/templates/`, preprocesses + connected-components on the crop, matches each component to the best template, prints per-component top-3 scores and the recognized string.

### `grid` — visualize the layout grid

```
cargo run -p ocr_lab -- grid             # uses default image
cargo run -p ocr_lab -- grid 20260526091333_1
```

Runs the full grid detection (BACK + LEVEL UP → panel bounds + tilt; "Need to submit items" → top anchor; FROM RAID labels → cell columns) and draws colored rectangles on a copy of the original image:

- 🔴 RED — title chip area
- 🟠 ORANGE — LV chip
- 🟦 CYAN — "Need to submit items" anchor
- 🔵 BLUE — cost number
- 🟢 GREEN — each item cell
- 🟡 YELLOW — each cell's X/Y progress strip
- 🟣 MAGENTA — BACK / LEVEL UP buttons

Saves `<stem>.grid.png` (full resolution) and `<stem>.grid_small.png` (≤ 1100 px) in `ocr_data/grids/`. Use the small one for visual inspection.

Set `OCR_LAB_GRID_DUMP=1` to log every OCR token Tesseract found in the preprocessed+upscaled image plus how many FROM/RAID anchors were paired into cells.

### `ocr-debug` — visualize raw OCR output on the preprocessed image

```
cargo run -p ocr_lab -- ocr-debug        # uses default image
cargo run -p ocr_lab -- ocr-debug 20260526091333_1
```

Preprocesses + 2× upscales the image (same params the grid uses), runs OCR, and draws a colored bounding box + the recognized text label at each detected word's position. Colors encode confidence (red < 50 < yellow < 75 ≤ green). Saves `<stem>.ocr_debug.png` and a `<stem>.ocr_debug_small.png` thumbnail under `ocr_data/ocr_debug/`.

Use this to figure out WHY the grid is finding (or missing) something — you see exactly what Tesseract read at each pixel position.

## Typical iteration loop

1. **Pick the prep threshold** — `prep` mode on a representative image, look at the sweep output, choose the variant that catches text without bleeding icons.
2. **Verify the grid** — `grid` mode on a few diverse images (1-row Storage Room C, 3-cell Moreitem, 4-cell Quality, tilted view). Confirm BACK/LEVEL UP, anchor, cells, and progress strips land where they should. Use `ocr-debug` to debug grid misses.
3. **Extract templates** — for each unique digit in the dataset, find a clean cell, then run `tm-extract` to save the template. The templates live in `ocr_data/templates/` (10 digits + slash = 11 PNGs once the dataset is fully covered).
4. **Score the pipeline** — `score` runs everything end-to-end and prints accuracy. Iterate on whichever metric is lagging.

## Environment variables

- `OCR_LAB_DUMP_PASS1=1` — log every word from the pipeline's full-image OCR pass.
- `OCR_LAB_DUMP_PASS2=1` — log every word from the pipeline's cropped-panel OCR pass.
- `OCR_LAB_DUMP_CROPS=<dir>` — dump cropped panel PNGs from the pipeline to `<dir>`.
- `OCR_LAB_DUMP_DIGITS=<dir>` — dump per-cell binarized digit strips from the pipeline to `<dir>`.
- `OCR_LAB_GRID_DUMP=1` — log all OCR tokens + FROM/RAID pair detection during `grid` mode.
- `RUST_LOG=ocr_lab=debug` — verbose tracing.

## Dataset format

`ocr_data/labels.json`:

```json
{
  "schema_version": 1,
  "notes": "...",
  "images": {
    "20260526091314_1.jpg": {
      "title": "Gunsmith",
      "level": "LV1",
      "description": "Weapon parts storage capacity +10kg",
      "cost": 80000,
      "items": [
        { "name": "Valve",       "collected": 2, "needed": 2 },
        { "name": "Boxed Nails", "collected": 2, "needed": 8 },
        { "name": "Ceramic adhesive", "collected": 1, "needed": 8 },
        { "name": "Floppydisk",  "collected": 0, "needed": 2 }
      ]
    }
  }
}
```

Items appear left-to-right matching the on-screen order.

## Adding a new image to the dataset

1. Drop the `.jpg` in `ocr_data/`.
2. Run `cargo run -p ocr_lab -- one <new_image>.jpg` — it'll fail to score (no ground truth) but print what it OCR'd.
3. Add a labeled entry to `labels.json` (use any existing image as a template).
4. Re-run `score` to confirm.
