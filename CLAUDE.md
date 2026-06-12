# CLAUDE.md

## Dataset provenance & refreshing it from the game files

**Principle:** the *shipped binary* never touches the game (anti-cheat-safe at
runtime — that's the user promise). *Sourcing the dataset* is a separate
**dev-time, offline** activity that does read the game's files. Keep these
distinct. `data.json` is the **canonical, hand-maintained dataset** — patch it
directly. It was originally bootstrapped from the upstream community project
**exfil-zone-assistant** (`source_repo` in `data.json`; the scraper that pulled
it was retired in #162 — upstream lagged seasons and carried wrong/duplicate
names); the game's own files are the **authoritative ground truth** for
validating/refreshing it.

> Don't commit raw extracted game tables, and don't publish **unreleased-season**
> data. This file documents *where to look and how* — not the data itself.

### Where the data lives (Contractors Showdown — Steam app 2719160, **UE 5.5**)
Install: `…\steamapps\common\Contractors Showdown\Contractors_Showdown\Content\Paks`.
Paks are **pak v11, UNENCRYPTED** (no AES key); file data is **Oodle**-compressed.

- **pakchunk1** — hideout/"Warfare" data:
  - `…/Warfare/FunctionalArea/FunctionalAreaUpgradeDataTable` (+ `_S2`…`_S5`) — **upgrade recipes** (items + counts + cost), per season
  - `FunctionalAreaUpgradeAreaInfo` / `…Struct` — module→area hierarchy + row schema
  - `…/Warfare/ValuableItem/ValuableItem_B_<X>` — ~160 item blueprints (== app `misc_b_<x>`);
    each carries the item **weight** + its **icon ref** (see parsing below)
  - `…/Warfare/WFStringTable/WFItemsStringTable` — **item tag → display name**
  - `…/Warfare/UI/ItemIcons/{MiscIcons,Food_Icons,Backpack_icons,box}/` — per-item
    **icon textures** (BC7/DXT 128px); `…/Warfare/UI/TextureAtlas/128x/**` — PaperSprite
    sheets for items without a standalone texture (S4 batch, shop pages)
  - ⚠️ `…/Warfare/ItemInfoWeight/WarfareItemInfo` is the **UI widget**, not a stats
    table. **Sell prices are NOT in the paks** — each blueprint's `SellInfo` maps a
    GUID the **Nakama backend** resolves at runtime; `data.json` prices stay curated.
- **pakchunk0** — `…/Localization/Game/{en,…}/Game.locres` (UI strings; note the
  misc-item names are NOT here — they're in `WFItemsStringTable`).

### How to extract (FModel — the safe path)
1. FModel (`fmodel.app`); Add Undetected Game → the Paks dir; **UE = `GAME_UE5_5`**;
   **AES = blank**. It auto-downloads a genuine Oodle DLL.
2. ⚠️ **Never** generate a `.usmap` (Dumper-7 / UE4SS inject into the running game →
   **BattlEye ban**). Not needed:
3. Use **"Export Raw Data"** for the DataTables/StringTables (works without a usmap;
   "Save Properties (.json)" does NOT — unversioned properties). `Game.locres`
   exports to JSON directly.

### Parsing the raw `.uexp` (no usmap)
- **Recipe row:** `AreaName_*` FString starts each row; `int32` cost (e.g. 10000);
  `int32` array length; then per item `FString tag` + `int32 count`; trailing
  non-`valuable.` FStrings are prereqs (`task.mall.N`, prior area, `Player`).
  FString = `int32 len` + bytes + null.
- **StringTable:** namespace FString, then alternating `key`(ends `_name`)/`value`.
- **Item bridge:** recipe tag `valuable.x.y` → key `valuable.x.y_name` in
  `WFItemsStringTable` → display name → app item by normalized name.
  App `misc_b_<x>` == game blueprint `ValuableItem_B_<X>`.
- **Blueprint weight (2026-06-12 re-derivation):** in the `.uexp` tail, the weight
  is the f32 in `[0.005,60]` followed by 4 zero bytes and a `0x35/0x36` marker
  byte (after the legacy-name FString + import-pair soup). 141/150 app weights
  byte-confirmed; 9 oddball blueprints are ambiguous — their curated values kept.
- **Blueprint icon:** the `.uasset` name table names the icon (`Icon_valuable_*` →
  `ItemIcons` texture, else a `TextureAtlas/128x` PaperSprite — rect = the four
  consecutive integer-valued f64s `BakedSourceUV`+`Dimension` in the sprite `.uexp`).
  All 150 misc icons + 11 backpacks + both junk boxes were re-extracted this way
  (tooling: `C:\Users\etien\fontx` getpak/tex2png + `gamedata/export_icons.py`).

### Gotchas
- Recipes are **seasonal** (`_S2`…`_S5`); `data.json` currently tracks ~**S5**.
  Re-confirm the live season against `screenshots/hideout/` each game update.
- **Name drift:** app "Nails" vs game "Boxed Nails"; `misc_b_storagebattery`="Car
  Battery". **Id drift:** the size-D-battery twins were renamed in #162 — game
  blueprints `ValuableItem_1batterie_2` ("Size D battery1") /
  `ValuableItem_B_1battery` ("Size D battery2") have id↔name-inverted slugs; the
  app ids are `misc_b_battery_1` / `misc_b_battery_2`.
  **Module aliases:** app `Intelligent`=game `IntelCenter`,
  `Generator`≈`PowerGenerator`, storage modules `StorageZoneLock/Storagevaluable/
  TerminalStorage/WorkshopZone`≈`StorageExpansion/Workshop/RestArea`.
- The OCR count font (`crates/app/src/ocr`) is **Quantico-Bold**, also identified
  from these paks.

### Validation oracle — status (WIP, do NOT trust a naive diff)
A local oracle (parse recipes → bridge tags → diff `data.json`) exists in scratch.
A quick version mis-flags many recipes because it (a) infers upgrade *levels* from
`AreaName` file-order instead of the real DataTable **row keys**, merging some rows
(KitchenArea/RestArea), and (b) has item-bridge gaps on drift names. It currently
flags even *panel-verified* recipes (e.g. `MedDeskLv1`, fixed in #116) as wrong —
proof it's not trustworthy yet. For a real bug list: resolve FName row keys from
the `.uasset` name table, tighten the bridge, and confirm season alignment.
