# Source provenance

`data.json` + `icons/` are the **committed, hand-maintained dataset**. They were
originally bootstrapped from the upstream community catalog, and have since been
hand-corrected and refreshed against in-game ground truth (panel screenshots
under `screenshots/`, see `screenshots/CLAUDE.md`; recipes track the live
season). Patch them directly — there is no regeneration step (the old
`crates/scraper` upstream pipeline was retired in #162 because upstream lagged
seasons behind and carried wrong/duplicate item names; it lives on in git
history if ever needed).

Bootstrap origin (kept for attribution — `source_*` fields in `data.json`):

- **Upstream repo:** https://github.com/zelengeo/exfil-zone-assistant (MIT)
- **Commit:** `6b0b855985aca75ce0a87a478c355cc2afe2c76f`
- **Game/data version:** `2.0.4+6b0b855`
- **Scraped at:** 2026-05-28T07:24:18.795852Z
