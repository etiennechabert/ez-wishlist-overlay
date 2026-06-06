# Screenshot fixtures & labelling skill

This folder holds every in-game capture the OCR pipeline is tested against, plus
the ground-truth **label files** that say what each capture *should* read. It is
the single source for the OCR quality pipeline (`scripts/ocr-eval.ps1`).

```
screenshots/
  CLAUDE.md          # this skill
  hideout/   <UpgradeId>.webp        + <UpgradeId>.label.txt     # Facility Upgrade panels (15)
  box/       box.shotN.webp/.boxes.json + box.label.txt          # world container tablet  (3 shots)
  stash/     stash.shotNN.webp/.boxes.json + stash.label.txt     # Johnny's-Service junk-box terminal (10 shots)
```

Three asset types, each scored **independently** by the pipeline:

| Asset | In-game screen | OCR exercised | Label = ground truth for |
|---|---|---|---|
| **hideout** | Facility Upgrade panel | full pipeline (identify upgrade + read each item's `owned/needed` counter) | per-cell owned-count + identification |
| **box** | a world container tablet (category tab strip: All / Medical / … / Tool) | `read_tiles` + `merge_capture` (row-uniqueness dedup) over the scroll shots | the merged item tally (passes exactly) |
| **stash** | the "JOHNNY'S SERVICE" junk-box submit terminal ("only miscellaneous items can be stored here") | same as box | the item tally (captures have scroll gaps — scored, not gated) |

> Both box and stash are titled "JUNK BOX" in-game — they're different screens.
> `box` is the physical world container; `stash` is the player's submit terminal.

---

## When to use this skill

You're asked to **add, refresh, or validate** captures and their labels — e.g.
"I dropped a new hideout screenshot in, validate it", "re-label the box scan",
"the OCR reads this fixture wrong, check the label". Labels are hand-authored
and are the fixed reference the OCR is judged against — **never edit a label to
make a failing OCR read pass**; fix the OCR (see the `ocr-tune` skill) or, if the
label itself is wrong, correct it and say so.

---

## hideout/ — Facility Upgrade panels

**Source.** ~3K-per-eye **WebP (quality 99)** captures of the SteamVR compositor
mirror texture (`crates/app/src/vr/capture.rs`). In-app: press **Space** with the
desktop window focused (or the "Capture VR screenshot" button in the Debug
dialog); the frame lands in `<data_dir>/debug/vr_screenshots/` (cleared every
launch — copy out what you keep).

**Compress to WebP q99 before committing — never commit the raw capture.** A raw
mirror-texture PNG is ~9 MB; 15 of them is ~135 MB, which bloats the repo. Encode
each chosen capture to WebP **q99** and commit only that (~0.9 MB each, ~13 MB
total — a 90% cut):

```
cwebp -q 99 in.png -o BookcaseLv1.webp
# or Pillow: Image.open("in.png").save("BookcaseLv1.webp", "WEBP", quality=99, method=6)
```

q99 is a **hard floor, not a preference**: the chunky pixel-art digit counters
are fragile under compression — q99 reads ~45–48/59 on the owned-count floor (vs
48 lossless); q98 and below drop *below* the gate. If you ever need more accuracy
margin, **re-encode lossless** (`cwebp -lossless`, ~70–90 MB) — never lower the
quality. (The old lossy Steam-F12 JPGs were dropped for exactly this reason; the
OCR sweep only picks up `.webp`/`.png`.)

**Naming.** `<UpgradeId>.webp` where the stem equals an `Upgrade.id` in
`crates/app/src/assets/data.json` (e.g. `BookcaseLv1.webp`). One file per upgrade
— dedupe to the cleanest panel (head steady, full panel, both digits legible).

**Label file — `<UpgradeId>.label.txt`** (one line per requirement cell):

```
# BookcaseLv1 — ground truth (owned / needed)
misc_b_disinfectingwipes  2/2
misc_b_pipeline           2/1
misc_b_lightbulb          1/2
```

Format: `<item_id>  <owned>/<needed>`. `#` comments and blank lines ignored;
parsed by `debug_dump::load_labels`. The `item_id`s and their order are the
upgrade's `requirements` in `data.json`.

**To create / refresh a hideout label (and validate the database):**
1. The filename stem is the `Upgrade.id`, so look up that upgrade in `data.json`
   — its `requirements` (`item_id` + `quantity`), `cost`, and names.
2. **Validate the database against the screenshot — the panel is ground truth.**
   Check each item tile's name + `need` count (and the `€ cost`) against
   `data.json`. If the screenshot disagrees (wrong item, wrong `need`, wrong
   cost, stale recipe), **patch `data.json` to match the screenshot** (map names
   to `item_id`s per the next section; never rename a catalog id). This is how
   the hideout upgrade database stays correct. The `hideout_labels_match_data_json`
   test fails CI if any label's `(item_id, needed)` diverges from `data.json`, so
   a refreshed label that no longer matches the data forces this reconciliation.
3. Read each cell's `owned` (the left number of `owned/need`). If a digit is
   ambiguous, crop + upscale before deciding (the small counter font confuses
   `0/2/8` and `6/8`); ask the user if still blurry.
4. Write one `<item_id>  <owned>/<needed>` line per cell, in `requirements`
   order.

---

## box/ & stash/ — container scans

**Source.** Same mirror-texture capture, of a **container screen**. Each scan is
a series of overlapping **scroll shots** because the grid doesn't fit one frame.
Committed **WebP q95** (raw PNGs are ~11 MB each; the canonical test input is the
frozen OCR below, not the image, and q95 preserves every item name).

**Three files per shot+scan:**
- **`<scan>.shotN.webp`** — the 3096×3312 capture frame, in scroll order
  (`box.shot0..2`, `stash.shot00..09`).
- **`<scan>.shotN.boxes.json`** — the **frozen Windows.Media.Ocr output** (word
  boxes + image height) for that frame. `read_tiles`/`merge_capture` are pure and
  platform-independent, so these let the post-OCR pipeline be regression-tested
  on **every** target (Linux CI included) without re-running the Windows-only,
  nondeterministic engine. Regenerate after adding/replacing a capture:
  ```
  cargo test -p ez-wishlist-overlay regen_box_fixtures -- --ignored   # Windows only
  ```
  (`BOX_FIXTURE_DIR` / `STASH_FIXTURE_DIR` point it at a scratch copy.)
- **`<scan>.label.txt`** — the ground-truth tally, **one per scan** (not per
  shot): `<item_id>  <count>` lines. A box scan only means something *merged*,
  so the label is the full deduped contents, not a per-frame snapshot.

**Merge model.** Captures are merged by **row uniqueness**: each grid row is
identified by its multiset of recognized items (tolerant of one drifted/missing
tile), and a row already seen is dropped as overlap. This is immune to scroll
distance and clipped boundary rows — it replaced an earlier position-rigid stitch
that broke whenever a tile was dropped or a boundary row was half-captured. The
one cost: two *distinct* rows with the identical composition collapse to one and
under-count (the desktop review step renders the rows so the user can drop a bad
one before applying).

**Status.** `box` passes exactly (`box_scan_matches_label`, 22 tiles). `stash`
stays `#[ignore]`d (`stash_scan_matches_label`): row-uniqueness fixed the
dropped-tile desync, but its 10 shots have real **scroll gaps** — shots 00–04
share a row each (clean scroll) but 04→09 skip rows entirely, so some grid rows
appear in no capture at all and can't be recovered by any merge. `stash.label.txt`
is a **verified reference** of the contents (the contiguous 00–04 run + the
additional types seen in the lower shots); the eval still scores its partial tile
accuracy as an informational signal.

**To refresh a box/stash label:** read the item names off the capture frames,
map each to its `item_id` (next section), and write `<item_id>  <count>`. A box
scan matches `data.json`'s `Item.name`, so an in-game name that differs from
`data.json` must be patched in `crates/scraper/src/corrections.rs` (upstream has
mislabels/duplicates that break the name match) — not worked around in the label.

---

## units/ — per-item isolated-OCR fixtures

Each asset folder has a `units/` subfolder of **whole item tiles** — one image
per item (icon + name, plus the owned/needed counter for hideout). The
`unit_ocr_tests` tests (Windows) OCR each lone crop and assert the engine recovers
the item's name, validating that **each item reads correctly in an isolated
shape**, not just embedded in the full panel/scan.

- **Crop:** one whole tile per item, at full resolution. **Place crops from the
  committed geometry, not by eye:**
  - **box/stash** — from the `.boxes.json` word boxes (item-name centres). For
    the full stash set across all 10 scroll shots, the `dump_stash_unit_tiles`
    diagnostic (`box_scan.rs`, `--ignored`) runs the box-scan matcher over every
    shot and prints `(shot, item_id, name-bbox)` per tile; the cropper picks each
    item's best mid-screen occurrence and expands the bbox up for the icon. When
    a name was OCR-dropped (e.g. CD, Beard oil) crop by grid position; when the
    matcher locked onto the **product label on the icon** (gun oil, ceramic
    adhesive…), the tile name is *below* it, so bias the crop down.
  - **hideout** — the panels are tilted and some cells fall back to a bad count
    position, so derive per panel from the `.ocr-debug` dumps' per-cell count
    centres: `pitch` = median column spacing; the **row is a line** fit by
    Theil-Sen through the cells (robust to the 1–2 bad-fallback cells); crop each
    tile `≈ pitch` wide × `0.78·pitch` tall, centred on its column with a small
    **right bias** (the name sits right of the count strip because the icon is on
    the left), `(y − 0.56·pitch) .. (y + 0.22·pitch)` on the fitted line.
  - Commit as WebP with Pillow:
    `Image.open(cap).crop((l,t,r,b)).save(out, "WEBP", quality=95, method=6)`.
    Name `<item_id>.webp` (hideout, where an item recurs across panels:
    `<UpgradeId>__<item_id>.webp`).
- **Label — `units/labels.txt`** (`<file>  <expected OCR name>` per line):
  ```
  misc_copperwire.webp  Copper wire
  #hard  misc_b_uvlight.webp  UV lamp
  ```
  The expected text is the **in-game display name the tile shows** (so a box/stash
  unit may differ from `data.json`'s `Item.name`). A `#hard  <file>  <name>` line
  is a unit the engine can't yet read in isolation (a stylised "UV" → "liv", a
  tiny angled hideout name) — still OCR'd and reported as an **OCR-improvement
  target**, but it doesn't fail the gate. Plain lines are **gated** (must keep
  passing).
- **The committed set is curated:** only tiles that OCR cleanly are gated, so the
  test is a regression guard. Coverage grows as tiles are added; the `#hard` ones
  are leads for the `ocr-tune` loop, not blockers.

---

## Mapping items → `item_id` (and data-validation pitfalls)

The hideout panel and the items dictionary at the end of `data.json` are the
**ground truth** for the scraped catalog, which is partly stale. When labelling,
map each on-screen item to its `item_id` by display name, watching for:

- **Module id ≠ module name ≠ header text.** Three distinct strings: `module.id`
  (slug like `RestRoom`), `module.name` (display name like `Toilet`), and the
  panel **header** (sometimes short like "Kitchen" or mirroring the id). The
  header is **unreliable** — anchor on `module.name` and the in-panel **row
  labels** (the canonical display name; `module.name` and every `upgrade.name`
  must equal the row label verbatim). Known header≠row divergences: `KitchenArea`
  ("Kitchen"/"Kitchen Area"), `Moreitem` ("Moreitem"/"Procurement System"),
  `Quality` ("Quality"/"Procurement Quality"), `Storagevaluable`
  ("Storagevaluable"/"Storage"), `TerminalStorage` ("Terminal Storage"/"Starter's
  Storage Expansion"). If a new module's row label disagrees with `module.name`,
  **patch `data.json` to match the row label.**
- **Upstream display names are sometimes wrong, but never rename a catalog id to
  match a label.** `misc_b_pipeline` has name "Valve" (id ≠ name) — keep the
  upstream slug; the scraper rebuilds the catalog every run and would revert a
  renamed id, orphaning the requirement (issue #89). Same rule for
  `misc_b_storagebattery` ("Car Battery" in JSON, larger item in-game) and
  `misc_b_batter_large` ("Storage Battery" in JSON, the small one in-game) —
  compare icons and ask the user.
- **Two items can share a display name** — disambiguate by the upstream icon
  filename suffix and the in-game label; never collapse them. Still live:
  `misc_b_gastank` + `misc_b_tape_large` are both "Gas can". Resolved:
  `misc_1batterie_2` and `misc_b_1battery` were both upstream "Size D battery"; the
  game distinguishes them with an **id↔name inversion** — `misc_1batterie_2` =
  "Size D battery1" (yellow pack), `misc_b_1battery` = "Size D battery2" (white
  pack) — now corrected in `crates/scraper/src/corrections.rs`.
- **Digit OCR ambiguity:** `0/2/8` and `6/8` confuse in the small counter font.
  Crop + upscale before assuming; ask the user if still blurry.
- **"Storage Zone Upgraded: 0/3" at a panel's top is a global status counter,
  not a per-upgrade requirement** — don't encode it.

Adding an item the dictionary lacks: check upstream
`zelengeo/exfil-zone-assistant` (`public/data/misc.json`) for the `id` + icon,
convert the icon to a 128×128 PNG under `crates/app/src/assets/icons/<id>.png`,
insert the entry alphabetically, and confirm a suspicious id with the user first.

---

## The OCR quality pipeline (how labels get used)

- **Score all three assets:** `./scripts/ocr-eval.ps1` — wires up the Windows
  MSVC build, runs the gates, and prints a per-asset scorecard (hideout
  owned-count noise band + id, box/stash graded tile accuracy). Writes the full
  JSON for the before/after compare.
- **Before/after a change:** `./scripts/ocr-eval-compare.ps1 -Baseline a.json
  -Candidate b.json` → KEEP / REVERT / NOISE. Hideout is compared against its
  run-to-run noise band; box/stash are deterministic.
- **Hard gates** (run by normal `cargo test … ocr`): hideout
  `identification_and_cell_ordering_on_native_pngs` (15/15) +
  `owned_count_accuracy_floor_on_native_pngs` (≥45); box `box_scan_matches_label`
  (exact); and `hideout_labels_match_data_json` — a pure data check (every
  target, no OCR) asserting each hideout label's `(item_id, needed)` matches
  `data.json`, so the screenshots validate the upgrade database. The underlying
  scoring diagnostic is `ocr::pipeline::fixture_tests::eval_report_json`.

**Don't commit without explicit instruction** — the user reviews each diff
first. After a `data.json` change, validate it:
`python3 -c "import json; json.load(open('crates/app/src/assets/data.json'))"`.
