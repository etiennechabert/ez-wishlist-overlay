# ExfilZone hideout audit: upstream vs. current in-game

**Upstream:** `zelengeo/exfil-zone-assistant` → `src/data/hideout-upgrades.ts` (master).
**Reference:** Miraheze wiki [`/wiki/Hideout`](https://csez.miraheze.org/wiki/Hideout), last edited **25 May 2026, 06:22 UTC**.

## TL;DR

- The upstream `hideout-upgrades.ts` predates at least the **April 2026 wipe**, possibly the **September 2025 wipe**. PlumberKarl's tracker (the other community source) is also stale — the maintainer stopped playing on 25 Sep 2025 and the spreadsheet hasn't been refreshed for the 2026 wipe.
- The Miraheze wiki is the only actively-maintained source. It uses 0-indexed levels (`Lv0`, `Lv1`, `Lv2`); upstream uses 1-indexed (`Lv1`, `Lv2`, `Lv3`). So **wiki LvN = upstream Lv(N+1)**.
- Out of ~50 recipes, ~35 are **fully consistent** (modulo item-id naming). The remaining **~15 have real changes** — quantities, materials, or missing data.
- One whole area (`Gunsmith`, 4 levels in upstream) is **not on the wiki Hideout page**. Could be removed, renamed, or just not yet migrated to the wiki. **Needs separate verification.**
- `Workshop` in upstream has `exchange: {}` (empty placeholder, with a `TODO` comment). Wiki has 4 required items.

## Naming map (upstream item id → wiki name)

Most differences boil down to id-vs-display-name. None of these are material changes; the underlying item is the same in both sources.

| Upstream id | Wiki name |
|---|---|
| `misc_b_pipeline` | Valve |
| `misc_b_gastank` | Gas Can |
| `misc_b_screw` | Bolts |
| `misc_b_lightbulb` | Energy-Saving Lamp |
| `misc_b_gaspipewrench` | Wrench |
| `misc_b_wrench` | Small Wrench |
| `misc_b_transformer` | Voltage Transformer |
| `misc_b_electricdrill` | Power Drill / Electric Drill |
| `misc_b_glue_large` | Instant Glue |
| `misc_b_shampoo` | Pet Shampoo |
| `misc_b_marinestoragebattery` | Marine Battery |
| `misc_b_militaryusbdrive` | Classified USB |
| `misc_b_visionmodule` | Imaging Module |
| `misc_b_antiquebook` | Antiquarian Book |
| `misc_b_antiqueteaset` | Antique Tea Cup |
| `misc_b_iodophor` | Iodine |
| `misc_b_aspire` | Aspirin |
| `misc_b_uvlight` | UV Lamp |
| `misc_b_medicalkit` | Portable Med Kit |
| `misc_b_disinfectingwipes` | Disinfectant Wipes |
| `misc_b_asthmamedication` | Asthma Medicine |
| `misc_b_bandaid` | Band-Aids |
| `misc_b_dataline` | Data Cord |
| `misc_b_opticaldisc` | CD |
| `misc_b_wirecutting` | Wire Cutters |
| `misc_b_plier` | Pliers (red) |
| `misc_b_plier_large` | Large Pliers |
| `misc_bpu` | CPU |
| `misc_b_pcfan` | Fan |
| `misc_barcleaner` | Cleaner |
| `misc_b_tapemeasure` | Measuring Tape |
| `misc_b_storagebattery` | Vehicle Battery *(verify — possibly distinct items)* |

## Real changes (require data update)

### 🔴 Material substitutions

| Recipe | Upstream | Wiki (current) |
|---|---|---|
| **Storage Room A Lv1** | 1× Large Battery (`misc_b_batter_large`) | **1× Small Battery** |
| **Toilet Lv1** | 1× Toilet Paper (`misc_b_toiletpaper`) | **1× Toilet Roll** (distinct item — wiki shows Toilet Paper = 8 total used elsewhere) |
| **Intel Center Lv2** (wiki `Intel Centre 1`) | 4× Insulating Tape (`misc_b_insulatingtape`) | **4× Tape** (cassette tape — wiki distinguishes the two) |
| **Plant Stand Lv2** (wiki `Plant Stand 1`) | 2× Ceramic Adhesive (`misc_b_ceramic_adhesive`) | **2× Ceramic** (wiki has both items separately) |
| **Bitcoin Mine Lv1** (wiki `Bitcoin Mine 0`) | 6× Ceramic Adhesive | **6× RAM** |
| **Bitcoin Mine Lv2** (wiki `Bitcoin Mine 1`) | 4× Ceramic Adhesive | **4× RAM** |
| **Bitcoin Mine Lv3** (wiki `Bitcoin Mine 2`) | 6× Ceramic Adhesive | **6× RAM** |

### 🟠 Quantity changes

| Recipe | Upstream | Wiki (current) |
|---|---|---|
| **Coffee Maker Lv1** (wiki `Coffee Maker 0`) | 5× Nuts | **3× Nuts** |
| **Refrigerator Lv1** (wiki `Fridge 0`) | 2× Rust Cleaner | **1× Rust Cleaner** |
| **Bitcoin Mine Lv2** (wiki `Bitcoin Mine 1`) | 6× Fan | **8× Fan** |

### 🟡 Upstream has no data

| Recipe | Upstream | Wiki (current) |
|---|---|---|
| **Workshop Lv1** | `exchange: {}` (TODO placeholder) | 1× Measuring Tape, 1× Small Wrench, 2× Screwdriver, 2× WD-40 |

### ❓ Not on wiki — verify manually

- **Gunsmith Lv1–4** (4 levels, prices 10k/80k/200k/400k, items including Old Phone, New Phone, Recorder, Power Bank, Hammer, etc.) — wiki Hideout page does not list a Gunsmith hideout area at all. The Sept 2025 wipe introduced a "Gunsmith system" but it may have been folded into a workbench feature rather than a standalone hideout area. The wiki's "Items Needed" totals also exclude `New Phone` entirely, and `Old Phone: 1` matches only Intel Center Lv1's usage — strongly suggesting **Gunsmith is no longer a hideout area in the current wipe** and these 4 upstream entries should be removed. Verify in-game before deleting.

## Recipes confirmed consistent (after name normalization)

These match upstream after applying the naming map above:

- Water Collector Lv1, Lv2, Lv3 (wiki `Water 0/1/2`)
- Kitchen Area Lv1 (wiki `Kitchen`)
- Microwave Lv1, Lv2, Lv3 (wiki `Microwave 0/1/2`)
- Coffee Maker Lv2, Lv3 (wiki `Coffee Maker 1/2`)
- Refrigerator Lv2, Lv3 (wiki `Fridge 1/2`)
- TV Set Lv1, Lv2, Lv3 (wiki `TV Set 0/1/2`)
- Sofa Lv1, Lv2, Lv3 (wiki `Sofa 0/1/2`)
- Bookdesk Lv1, Lv2, Lv3 (wiki `Bookdesk 0/1/2`)
- Intel Center Lv1, Lv3, Lv4 (wiki `Intel Centre 0/2/3`)
- Storage Room B Lv1, C Lv1 (wiki `Storage B/C`)
- Medical Area Lv1 (wiki `Medical`)
- Plant Stand Lv1, Lv3 (wiki `Plant Stand 0/2`)
- Operating Bed Lv1, Lv2, Lv3 (wiki `Op Bed 0/1/2`)
- Med Desk Lv1, Lv2, Lv3 (wiki `Med Desk 0/1/2`)
- Toilet Lv2, Lv3 (wiki `Toilet 1/2`)
- Generator Lv1 (free), Lv2 *(probably — PDF truncated last item)*, Lv3 (wiki `Generator 0/1/2`)
- Bitcoin Mine Lv4 (wiki `Bitcoin Mine 3`)
- Shooting Range Lv1

## Suggested next steps for the scraper

1. **Don't blindly trust upstream for the listed changes** — overlay the wiki-derived corrections, or open a GitHub issue on `zelengeo/exfil-zone-assistant` linking to the wiki diff above so they fix it at the source.
2. **Item id stability**: if upstream just renames `misc_b_batter_large` → `misc_b_smallbattery` in a future commit, your existing scraper handles it cleanly because you key on the id, not the display name. But the new materials (RAM, Tape, Ceramic, Toilet Roll, etc.) are *new id additions* — check whether icons exist for those ids before assuming a rescrape will Just Work.
3. **Gunsmith decision**: if you confirm in-game that Gunsmith hideout area is gone, drop the 4 entries from your scraper output. If it's just moved, you'll need to find its new key.
4. **Wiki is the freshest source** but it's gated against programmatic scraping. Options if you want automation:
   - Use the MediaWiki API from a non-Anthropic-egress environment (your laptop, a runner, etc.) — the page id appears to be `oldid=2310` so `https://csez.miraheze.org/w/api.php?action=parse&page=Hideout&format=json&prop=wikitext` should work outside the sandbox.
   - Or have your scraper diff against a committed snapshot of the wiki wikitext that you refresh manually each wipe.
