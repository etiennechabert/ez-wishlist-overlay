# Screenshot fixtures & labelling skill

This folder holds every in-game capture the OCR pipeline is tested against, plus
the ground-truth **label files** that say what each capture *should* read. It is
the single source for the OCR quality pipeline (`scripts/ocr-eval.ps1`).

```
screenshots/
  CLAUDE.md          # this skill
  hideout/   <UpgradeId>.webp        + <UpgradeId>.label.txt     # Facility Upgrade panels (15)
  box/       box.shotN.webp/.boxes.json + box.label.txt          # world container tablet  (3 shots)
  stash/     stash.shotNN.webp/.boxes.json + stash.label.txt     # Johnny's-Service junk-box terminal (38 shots)
  research/  tree.shotN.webp + <node>.webp + research.label.txt  # Neumann's RESEARCH tree (19 nodes; not OCR-gated yet)
  gunsmith/  gunsmith.shotN.webp/.boxes.json + gunsmith.label.txt # Neumann's Gunsmith → Storage gun-parts container (4 shots; #175/#183)
  magbox/    magbox.shotN.webp/.boxes.json + magbox.label.txt    # "Magazine & Attachments" world box (6 gaze-crop shots; floor-gated, #197)
```

Five asset types, each scored **independently** by the pipeline:

| Asset | In-game screen | OCR exercised | Label = ground truth for |
|---|---|---|---|
| **hideout** | Facility Upgrade panel | full pipeline (identify upgrade + read each item's `owned/needed` counter) | per-cell owned-count + identification |
| **box** | a world container tablet (category tab strip: All / Medical / … / Tool) | `read_tiles` + `merge_capture` (row-uniqueness dedup) over the scroll shots | the merged item tally (passes exactly) |
| **stash** | the "JOHNNY'S SERVICE" junk-box submit terminal ("only miscellaneous items can be stored here") | same as box | the item tally (gap-free 38-shot series; name divergences keep it scored, not gated) |
| **gunsmith** | Neumann's Gunsmith → Storage (gun parts, 30 KG cap) | same as box, but `read_tiles` runs scoped to the `gunsmith` category, enabling the gun-part **short-name** matcher (issue #183) | a golden snapshot of resolved parts (gappy gaze-crop series — floor + spot-checks, not exact) |
| **magbox** | the "Magazine & Attachments" world box (rarity-tab grid) | same as gunsmith (category scope `gunsmith`); exercises the dense-grid column split (`split_tiles` Otsu valley, #197) | distinct magazines resolved (gappy same-gaze burst — floor + spot-checks; recognition-garbled tiles miss) |

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
mirror texture (`crates/app/src/vr/capture.rs`). In-app: click the "Capture VR
screenshot" button in the Debug dialog; the frame lands in
`<data_dir>/debug/vr_screenshots/` (cleared every launch — copy out what you
keep).

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
  (`box.shot0..2`, `stash.shot00..37`). Prefer a **row-by-row** scroll (one grid
  row per shot, so each row is visible in ~3 consecutive frames): the redundancy
  leaves no scroll gaps, gives hard tiles several chances to read, and lets the
  row-uniqueness merge be validated against re-seen rows.
- **`<scan>.shotN.boxes.json`** — the **frozen OCR-engine output** (PP-OCRv4
  since #181; word
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

**Status.** `box` passes exactly (`box_scan_matches_label`, 24 tiles). `stash`
stays `#[ignore]`d (`stash_scan_matches_label`) — but no longer for coverage:
the 2026-06-11 row-by-row series is **gap-free** (40 rows × 5 columns, every
interior row in 3 consecutive frames) and `stash.label.txt` is a hand-read of
the complete grid (200 tiles). What still blocks an exact tally is
**recognition**: in-game labels that diverge from `data.json` names ("Windproof
Matches"/"Matches", "Band-aids"/"Adhesive bandages", "Pet Shampoo"/"Shampoo",
"Boxed Bolts"/"Bolts", "Boxed Nuts"/"Nuts" — fix `Item.name` in `data.json`,
not the label) plus residual glyph misreads (battery2 → battery1). The eval
scores its graded tile accuracy as an informational signal.

**To refresh a box/stash label:** read the item names off the capture frames,
map each to its `item_id` (next section), and write `<item_id>  <count>`. A box
scan matches `data.json`'s `Item.name`, so an in-game name that differs from
`data.json` must be fixed by patching `Item.name` in `data.json` itself (the
upstream catalog this dataset was bootstrapped from had mislabels/duplicates
that break the name match) — not worked around in the label.

---

## research/ — Neumann's RESEARCH tree (panel-verified dataset, not OCR-gated)

The gunsmith merchant Neumann's blueprint research tree (Basic category, 19
nodes `a1–a9 / b1–b3 / c1–c4 / d1–d3` = the game's `task.research.<node>` ids).
Unlike the other assets these are **dataset ground truth first**: the future
`data.json` `research` section is validated against them the way hideout labels
validate upgrade recipes. No OCR gate consumes them yet (a "scan research tree"
mode is a candidate later phase).

- **`tree.shot0/1.webp`** — the full tree, top/bottom scroll positions (node
  layout + connector edges; note `b3→a8` and `c4→a7` merge into the centre
  spine, so it's a DAG, not a tree).
- **`<node>.webp`** — that node's detail pane: the unlocked part (name, class,
  price, weight) and the OBJECTIVE strip of required submissions with `X/N`
  counters, every one tagged FROM RAID. Panes are readable on *locked* nodes
  too, so the whole dataset can be re-captured after a game update without
  progressing the tree.
- **`research.label.txt`** — the transcription: per node its parents, the
  unlocked item (game `gunsmith.*` tag from `WFItemsStringTable`), and each
  requirement as `<item ref> <needed>`. Misc requirements use `data.json`
  `item_id`s (all resolve by exact display name; sole alias: tile "Badge" =
  `misc_badge` "Sheriff's Badge"); gun-part requirements keep their game tag
  until app ids are minted. Same rule as everywhere: the pane is ground truth —
  if `data.json` disagrees, patch the data.

These captures are the box-scan **gaze-crop** debug WebPs (`~1424×927`), not
full 3096×3312 mirror frames — fine for ground truth + icon cutouts; recapture
full frames only if an OCR gate ever needs them.

## gunsmith/ — Neumann's Gunsmith → Storage (gun-parts container)

The gunsmith's own stash (in-game path **Gunsmith → Storage**): a weight-capped
(30 KG) container of gun parts with no sorting and no content overview (epic
#175). `gunsmith.shot0..3.webp` is a 4-shot scroll series captured 2026-06-14
(weight strip `26.93 / 30 KG`); the box-scan engine parses the weight
(`observed_weight`) and now **resolves the gun-part tiles** via the short-name
matcher (issue #183).

**Why it needs its own matcher.** The storage grid shows hand-authored *short*
gun-part names (`Cobra`, `M16A1`, `AR-308 DMR`, `AKS74U B18`) while the catalog
carries the full `WFItemsStringTable` names (`Cobra 20mm reflex sight`, …). Those
short names ARE in the paks — the game's **`GunSmithItemAdv`** table holds one
per `gunsmith.*` tag — so they're extracted offline into each part's
`Item.scan_alias` (see the data-provenance section in the repo-root `CLAUDE.md`).
`read_tiles` runs scoped to the `gunsmith` category (the container's category — set from
the `ScanTarget` in `main.rs`), and after the strict pass misses, matches the
tile against each part's `scan_alias` by the same confusion-aware distance. See
the `crate::ocr::match_item` module docs. Misc box/stash matching is untouched
(`gunsmith = false`).

**Gate — `gunsmith_storage_scan_resolves_parts`** (cross-platform, off the
frozen `.boxes.json`, PP-OCRv4 since #181): the scan resolves **41 distinct
parts** (floor 38; was **0** before #183 — and only ~21 on the old Windows
engine, so #182's PP-OCR migration is what unlocked clean gun-part reads), every
one genuinely in the `gunsmith` catalog, plus spot-checked tiles across part
classes. It's a floor, not an exact tally:

- **This is a gappy gaze-crop series, NOT row-by-row.** The 4 shots overlap with
  scroll gaps (~1424×927 debug crops, not full 3096×3312 mirror frames), so the
  row-uniqueness merge yields a *partial, dedup-sensitive* tally — when the
  matcher changes what resolves, a row's recognized-composition changes and the
  merge keeps different rows. `gunsmith.label.txt` is the **golden snapshot** of
  what currently resolves (a regression reference + the `.ocr-result.txt` input),
  not the storage's true contents.
- Short names the game **shares across parts** (27 of them — `M9`, `AR-15 DD`,
  `AR-15 M4`) stay unrecognized **by design**: the alias matcher rejects a tie
  rather than guess. A handful of parts (3 of 614) have no extractable short name
  and likewise won't resolve.
- **To gate an exact/graded tally**, recapture full frames row-by-row (the stash
  #163 convention) so the merge is gap-free, then score it in `eval_report_json`
  the way box/stash are. Until then the floor gate is the guard.

## magbox/ — the "Magazine & Attachments" world box (floor-gated)

A second world container (distinct from `box`): the in-game box titled
**"MAGAZINE & ATTACHMENTS ONLY"**, a rarity-tabbed (All / Unusual / Rare /
Legendary / Common) 5-column grid of magazines *and* weapon attachments. Its
items are gunsmith catalog entries (`gunsmith *_clip_*` magazines, plus the
muzzle/sight/grip/etc. attachment parts), so it scans with the gun-part matcher
(its `gunsmith` category scope) like `gunsmith/`.

`magbox.shot0..5.webp` is a 6-shot gaze-crop burst captured 2026-06-18 (weight
strip `8.45 / 12`). It exposed — and drove the fix for — a **dense-grid
segmentation bug** (#197): this box's long, cell-filling names
(`AK74 P-Mag 5.45x39mm 30rnd magazine`) leave an inter-column gap of only
~0.8–1.4·med_h while intra-name word gaps are ~0.3–0.5·med_h — *both* below the
old flat `1.5·med_h` split threshold — so an entire row collapsed into one
unmatchable blob (`"AK74 … AKM AKS5 … AR-10 …"`; 0–2 tiles/shot even up close).
[`split_tiles`] now picks the threshold at the **Otsu valley** between the two
gap clusters (floored at `0.6·med_h`, capped at the old `1.5·med_h`), which
splits the columns here while reproducing the old splits exactly on the
well-spaced box/stash/gunsmith fixtures (their gates are unchanged).

The scan now resolves **7/12** distinct magazines (`magbox_scan_resolves_magazines`
gates a floor of 6). It's a floor, not exact, for two reasons in
`magbox.label.txt`: (1) the 5 misses are **recognition** garble on distant tiles
(`MPS`→MP5, mashed AR-10 / AR-15 / PKP) plus one false positive (a garbled tile →
the misc `Magazine`) — an `ocr-tune` follow-up, not a segmentation one; (2) these
are the **magazine half only**, all at one gaze (a partial, dedup-sensitive view)
— a fuller row-by-row recapture that scrolls through the **attachment** rows would
gate it exactly and exercise the attachment-label work. `magbox.label.txt` is a
presence snapshot of the 12 magazines (count `1` = present, not a copy-count), and
flags one catalog gap it surfaced: the **XM5 6.8x51mm 30rnd magazine** has no
`data.json` entry yet.

## units/ — per-item isolated-OCR fixtures

Each asset folder has a `units/` subfolder of **whole item tiles** — one image
per item (icon + name, plus the owned/needed counter for hideout). The
`unit_ocr_tests` tests (Windows) OCR each lone crop and assert the engine recovers
the item's name, validating that **each item reads correctly in an isolated
shape**, not just embedded in the full panel/scan.

- **Crop:** one whole tile per item, at full resolution. **Place crops from the
  committed geometry, not by eye:**
  - **box/stash** — from the `.boxes.json` word boxes (item-name centres). For
    the full stash set across all scroll shots, the `dump_stash_unit_tiles`
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
- **Units are cumulative per-item coverage, not a snapshot of the current
  container.** When a capture refresh drops an item from the box/stash, keep its
  existing unit tile from the earlier series (e.g. stash `misc_b_harddrive`,
  `misc_blimbingrope`) — the item can reappear, and the tile keeps validating
  that the engine still reads it. Only replace a tile when the new series offers
  a better crop of the same item.

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
- **`data.json` is the canonical dataset — patch it directly, deliberately.**
  There is no regeneration step (the upstream scraper was retired in #162; ids
  and names are ours to fix end to end, as done for the size-D-battery twins →
  `misc_b_battery_1`/`misc_b_battery_2` + matching icon files + recipe refs).
  An id ≠ name is not by itself a bug: `misc_b_pipeline` is named "Valve" —
  verify against in-game captures before renaming anything, and remember an id
  rename orphans any persisted owned-counts under the old id (state.rs prunes
  them). The battery-family slugs lie outright (resolved against the game's
  `WFItemsStringTable`): `misc_b_storagebattery` is the game's
  `valuable.batteries.carbattery` = **"Vehicle battery"**, while
  `misc_b_batter_large` is `valuable.batteries.storagebattery` = "Storage
  Battery" — names follow the game string, never the slug.
- **Two items can share a display name** — disambiguate by the upstream icon
  filename suffix and the in-game label; never collapse them. Still live:
  `misc_b_gastank` + `misc_b_tape_large` are both "Gas can". Resolved by patching
  `data.json` directly: the size-D-battery twins are now `misc_b_battery_1` =
  "Size D battery1" (yellow pack) / `misc_b_battery_2` = "Size D battery2" (white
  pack). OCR note on those twins: a tile can read "Size D batteryl" (1→l);
  match_item's confusion-aware distance keeps l↔1 cheap but l↔2 full-cost, so it
  still resolves to battery1 — the symmetric names are safe. A fully dropped
  digit ("Size D battery") is genuinely ambiguous from the label alone.
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
- **Committed per-image results:** a `<stem>.ocr-result.txt` sidecar next to
  **every committed image** — one file per image, written so a human can glance
  at "this screenshot → what the OCR made of it" without running anything. Two
  generators, split by whether the read needs the live (Windows-only,
  non-deterministic) OCR engine:
  - **Deterministic — box/stash** (frozen `.boxes.json`, no engine; regenerated
    by a pure cross-platform Rust test:
    `cargo test -p ez-wishlist-overlay write_box_scan_results -- --ignored`):
    - `box/<scan>.shotN.ocr-result.txt` — per-**image**: what that one scroll
      frame's OCR read. A high-level summary first (item tiles recognized, texts
      dropped as chrome, the items found), then a row-by-row trace of every OCR
      text and the catalog item it resolved to (or "no match").
    - `box/box.ocr-result.txt`, `stash/stash.ocr-result.txt` — the **merged**
      scan: captured-vs-label, with a per-item `OK / MISSING / EXTRA` breakdown so
      the misses (e.g. stash's unmatchable name divergences) are visible at a
      glance.
  - **Live-engine — hideout + units** (`./scripts/ocr-eval.ps1` then
    `./scripts/ocr-write-results.ps1 -Json score.json`, Windows):
    - `hideout/<UpgradeId>.ocr-result.txt` — per-cell PASS/FAIL of the owned-count
      read vs the label + identification.
    - `<asset>/units/<item>.ocr-result.txt` — the isolated-OCR read of each unit
      crop (expected display name vs what the engine read, PASS/FLAKY/FAIL).
    These collapse reads across `-Runs` (FLAKY when they varied), since the engine
    is non-deterministic.

  All of these are **auto-generated read-outs, NOT ground truth** — never
  hand-edit them and never treat them as labels (the `.label.txt` files are the
  only ground truth). They're committed so the repo records, at a glance, which
  capture the OCR currently reads; refresh them after an OCR change. Distinct from
  the gitignored timestamped `*.ocr-debug.*.txt` / `*.cell*.png` in-flight dumps.
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
