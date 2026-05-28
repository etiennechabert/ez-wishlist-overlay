# Hideout Screenshot Validation

This directory holds in-game screenshots of the Contractors Showdown "Facility Upgrade" panel. They are **ground truth** for `crates/app/src/assets/data.json`, which was scraped from an upstream source (`zelengeo/exfil-zone-assistant`) that is partly stale.

## When to use this skill

The user drops new screenshots into `hideout_screenshots/` and asks to "validate" / "check" / "patch" the data against them. Each screenshot pictures one facility's upgrade panel.

## The workflow (one screenshot at a time)

1. **Identify the upgrade.** Two text regions hold the name; they don't always agree, and only one is canonical:
   - **Header block** at panel top-left (e.g. `Kitchen` / `LV0`). The `LV<digit>` is the current level. **The name on this line is unreliable** — for several modules it's an abbreviation (`Kitchen` ↔ `Kitchen Area`) or the literal module ID (`Moreitem`, `Quality`, `Storagevaluable`, `Terminal Storage`). Do not treat it as authoritative.
   - **Row labels** (the three or four `<name> / <description>` rows below the header — all rows show the same name). This text is the **canonical display name** and must strictly equal `module.name` and every `upgrade.name` in `data.json`. The OCR pipeline (when it ships) matches against this.

   Find the highlighted row (orange up-arrow). That row's level is the upgrade the screenshot validates. A green check ✓ on a row means already owned; padlocks 🔒 mean locked higher levels.
2. **Read the requirements** at the bottom of the panel: 1–4 item tiles (icon + name + `have/need` counter) plus a `€ <amount>` cost. Green counter = requirement met. If a digit is hard to read, crop with `sips --cropToHeightWidth H W --cropOffset Y X <input> --out /tmp/crop.jpg` and Read the crop; if still ambiguous, ask the user.
3. **Find the matching JSON entry.** Module ids in `data.json` don't always match the on-screen module name — e.g. `Toilet` is `RestRoom`, `Bitcoin Mine` is `CryptoMining`, `Intel Center` is `Intelligent`. Use `grep` on the upgrade id (`<ModuleId>Lv<N>`) or the module display name. **Required invariant for OCR**: `module.name` and every `upgrade.name` in the matched entry must equal the **row label** verbatim. Five known modules where the header diverges from the row label (header is wrong, row is right): `KitchenArea` (header "Kitchen" / row "Kitchen Area"), `Moreitem` ("Moreitem" / "Procurement System"), `Quality` ("Quality" / "Procurement Quality"), `Storagevaluable` ("Storagevaluable" / "Storage"), `TerminalStorage` ("Terminal Storage" / "Starter's Storage Expansion"). If a new module ever ships where the row label disagrees with `module.name`, **patch `data.json` to match the row label** — the row label is the source of truth.
4. **Map each screenshot item to an `item_id`.** Search the items dictionary at the end of `data.json` by display name. The item dictionary is mostly trustworthy but has a few mislabels (see Pitfalls).
5. **Show the user a diff table** — current JSON vs screenshot — and wait for confirmation before patching. Only run `open hideout_screenshots/<file>.jpg` when you are *about to ask the user a question* — that pops Preview on macOS so they can verify what you read. Don't `open` on routine duplicates or non-questions; it spawns clutter. (`Read` is for you, `open` is for them.) Description in JSON is often more specific than the screenshot's short label (e.g. JSON `"Character experience gain +2%"` vs screenshot `"Increased EXP Gain"`); prefer keeping the more specific one. Note when JSON wording differs but is synonymous.
6. **Apply the patch with `Edit`.** Add a `"cost": <number>` field (new; not present pre-2026-05-26) and rewrite the `requirements` array.
7. **Rename the screenshot to `<UpgradeId>.jpg`** with `git mv` (e.g. `KitchenAreaLv1.jpg`). This is the pairing the next reviewer will look for. If the screenshot is a re-capture of an upgrade already validated and matches the JSON exactly, **delete it with `git rm`** — only qualified named screenshots should remain.
8. After the last screenshot, validate JSON with `python3 -c "import json; json.load(open('crates/app/src/assets/data.json'))"` and commit. Don't merge or push unless explicitly asked.

## Schema additions made during this work

- `cost` (integer €) on each upgrade — scraper missed this; populate when you have a screenshot.
- Three new modules were created from screenshots: `Quality` ("Procurement Quality"), `Moreitem` ("Procurement System"), `Storagevaluable` ("Storage"). Convention: module `id` = short on-screen header word, module `name` = upgrade row label.

## Adding new items

If a screenshot shows an item not in the dictionary:
1. Check upstream `https://raw.githubusercontent.com/zelengeo/exfil-zone-assistant/master/public/data/misc.json` for the `id` and icon path.
2. Download the icon (`.webp` under `/images/items/misc/`), convert to 128×128 PNG with `sips`, save to `crates/app/src/assets/icons/<id>.png`.
3. Insert the item entry alphabetically in the items dictionary section of `data.json`.
4. Ask the user about the proposed id before adding if the upstream id is suspicious.

Items added this way so far: `misc_b_ram`, `misc_b_ionbattery`, `misc_b_tapeplayer`.

## Pitfalls / known quirks

- **Upstream display names are sometimes wrong.** `misc_b_storagebattery` is called "Car Battery" in JSON but in-game is the larger "Storage Battery"-style item; `misc_b_batter_large` is called "Storage Battery" but in-game is what's labeled "Small storage battery". When in doubt, compare icons and ask the user — they know the game.
- **Two items can share the same display name** (e.g. `misc_1batterie_2` and `misc_b_1battery` both "Size D battery"). Disambiguate by upstream icon filename suffix (`sizedbattery1` vs `sizedbattery2`).
- **`misc_b_pipeline` was renamed to `misc_b_valve`** because the icon and game label are both "Valve". If you find more cases of id ↔ display-name drift, ask before renaming — the id rename has to touch every reference + the icon file.
- **Module id ≠ module name ≠ header text.** Three distinct strings exist: `module.id` (internal slug like `RestRoom`), `module.name` (the display name like `Toilet`), and the panel **header text** (which is sometimes short like "Kitchen" or even mirrors the module ID like "Storagevaluable"). The header is **unreliable** and must not be used for validation; always anchor on `module.name` and the in-panel **row labels** instead. Verified header/row divergences as of 2026-05-27: `KitchenArea`, `Moreitem`, `Quality`, `Storagevaluable`, `TerminalStorage`.
- **"Storage Zone Upgraded: 0/3" header at the top of some panels is NOT a per-upgrade prerequisite** — it's a global status counter. Don't try to encode it as a requirement.
- **Digit OCR ambiguity**: `0` vs `2` vs `8` and `6` vs `8` are the common confusions in the small counter font. Always crop+upscale before assuming, and ask if the upscale is still blurry.
- **Don't commit without explicit instruction.** The user reviews each diff first.
