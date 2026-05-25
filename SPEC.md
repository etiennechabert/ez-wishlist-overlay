# EZ Wishlist Overlay Tracker — Engineering Spec

A standalone Windows desktop + SteamVR overlay app that tracks hideout upgrade progress for *Contractors Showdown: ExfilZone*. The user enables which upgrades they're working on; the VR overlay shows the aggregated list of items still needed, viewable by looking up; clicking icons in VR increments collected counts.

This spec is the source of truth. If something here is ambiguous, prefer the simpler interpretation and add a note in `OPEN_QUESTIONS.md` rather than guessing.

---

## 1. Goals & Non-Goals

### Goals
- Track multiple hideout upgrades and quest tasks simultaneously, aggregate their item requirements into a unified wishlist
- Desktop GUI to enable/disable tracking and mark items complete, with separate views for hideout upgrades and quest tasks
- SteamVR overlay showing remaining items across both categories, viewable by looking up
- Click icons in the overlay to increment collected count (cycle back to 0 at target)
- Single-binary distribution on Windows with an MSI installer
- Zero interaction with the game process — fully out-of-process and anti-cheat-safe

### Non-Goals (v1)
- Screen capture or OCR of in-game inventory
- Network interception of game traffic
- Reading game memory or files
- Tracking non-collection task objectives (kill counts, tracker placements, area reaches) — only item-collection requirements are tracked. Tasks without item requirements are excluded from the scraped dataset.
- Enforcing task prerequisites (we show them as info, but let the user track whatever they want)
- Mac / Linux support (Rust portability is preserved, but Windows is the only release target)
- Voice input
- Cloud sync between machines
- Auto-detection of completed items from screenshots

### Anti-cheat constraints (absolute)
The app MUST NOT:
- Open a handle to the game process
- Inject into the game process
- Hook any game-related DLLs or syscalls
- Read or write game memory
- Capture network traffic
- Modify game files

The app MAY:
- Read its own files in `%APPDATA%`
- Talk to SteamVR via the public OpenVR API
- Render into an OpenVR overlay (this is the supported public API used by OVR Toolkit, XSOverlay, etc.)
- Capture HMD pose via OpenVR (also public API)

---

## 2. Tech Stack

| Concern | Crate | Notes |
|---|---|---|
| GUI framework | `eframe` + `egui` | Latest stable. Single-window desktop app. |
| VR overlay | `openvr` 0.9.0 (wraps `openvr_sys` 2.1.4) | Builds Valve's bundled OpenVR C++ SDK via cmake + bindgen at compile time. Requires MSVC + libclang for the build. |
| Overlay texture rendering | `tiny-skia` + `image` | CPU rasterizer. The overlay is a static-ish grid that re-renders at <2Hz, so GPU is overkill and the wgpu↔OpenVR DXGI shared-handle path is fiddly. Render to a `Vec<u8>` (RGBA) and submit via `SetOverlayRaw`. Revisit if perf becomes an issue. |
| State serialization | `serde` + `serde_json` | JSON on disk, human-readable. |
| Async runtime | `tokio` | Only used for the scraper tool and debounced disk writes. |
| HTTP client | `reqwest` | Used by scraper (build-time tool), not runtime. |
| TS data parsing | `boa_engine` (or shell out to `node`) | Build-time only; for reading TypeScript object literals from the upstream repo. |
| Git ops (optional) | `gix` or shell-out to `git` | Build-time only; for shallow-cloning the upstream repo. |
| Embedded assets | `rust-embed` | Bundle `data.json` and icons into the binary. |
| Logging | `tracing` + `tracing-subscriber` | File log in `%APPDATA%/ez-wishlist-overlay/logs/`. |
| Error handling | `anyhow` for app code, `thiserror` for library-style errors in the scraper. |
| Config dirs | `directories` | Resolves `%APPDATA%` correctly. |

**Pin all versions in `Cargo.toml` at the start of the project.** Update via `cargo update` deliberately, not implicitly.

**MSRV:** latest stable Rust at project start. No nightly features.

---

## 3. Workspace Layout

```
ez-wishlist-overlay/
├── Cargo.toml                  # workspace
├── crates/
│   ├── app/                    # the shipped binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── state.rs        # AppState, persistence
│   │       ├── data.rs         # types: Upgrade, Task, Item, etc.
│   │       ├── gui/
│   │       │   ├── mod.rs
│   │       │   ├── hideout_pane.rs
│   │       │   ├── tasks_pane.rs
│   │       │   ├── preview_pane.rs
│   │       │   └── about_dialog.rs
│   │       ├── vr/
│   │       │   ├── mod.rs
│   │       │   ├── overlay.rs  # IVROverlay setup + texture submission
│   │       │   ├── pose.rs     # HMD pitch tracking, visibility hysteresis
│   │       │   ├── input.rs    # laser-pointer mouse events → grid hit-test
│   │       │   └── render.rs   # wgpu icon-grid renderer
│   │       └── assets/         # embedded via rust-embed
│   │           ├── data.json
│   │           └── icons/*.png
│   └── scraper/                # build-time tool, not shipped
│       ├── Cargo.toml
│       └── src/main.rs
├── data-sources/               # raw upstream snapshots (commit hash + timestamps) for reproducibility
├── dist/                       # cargo-dist + cargo-wix configs
├── SPEC.md                     # this file
├── OPEN_QUESTIONS.md
└── README.md
```

The `scraper` is a separate binary intentionally — it's only run by maintainers when game data changes, and we don't want its dependencies in the shipped app.

---

## 4. Data Model

### 4.1 Types (in `crates/app/src/data.rs`)

```rust
pub type UpgradeId = String;    // e.g. "workbench_lvl2"
pub type TaskId = String;       // e.g. "ark_11" (matches Assistant's URL slugs)
pub type ItemId = String;       // stable slug, e.g. "bolts"
pub type ModuleId = String;     // e.g. "workbench", "medstation"
pub type VendorId = String;     // e.g. "handshake", "lab_rat"

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameData {
    pub data_version: String,   // e.g. "2026-Q1-wipe"
    pub scraped_at: String,     // RFC3339
    pub source: String,         // URL we scraped from
    pub modules: Vec<HideoutModule>,
    pub vendors: Vec<Vendor>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HideoutModule {
    pub id: ModuleId,
    pub name: String,
    pub upgrades: Vec<Upgrade>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upgrade {
    pub id: UpgradeId,
    pub name: String,
    pub level: u32,
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vendor {
    pub id: VendorId,
    pub name: String,           // e.g. "Handshake", "Lab Rat"
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub vendor_id: VendorId,
    pub prerequisites: Vec<TaskId>,   // shown in UI as info, not enforced
    pub requirements: Vec<Requirement>, // only item-collection objectives
    pub source_url: String,           // link back to Assistant page
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub item_id: ItemId,
    pub quantity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub name: String,
    pub icon_path: String,            // relative to assets/icons/
}
```

Tasks with empty `requirements` (pure kill/plant/extract objectives) are excluded from the scraped dataset. They have nothing to track.

### 4.2 Runtime state (in `crates/app/src/state.rs`)

```rust
pub struct AppState {
    pub data: Arc<GameData>,                  // immutable, loaded once
    pub tracked_upgrades: HashSet<UpgradeId>,
    pub completed_upgrades: HashSet<UpgradeId>,
    pub tracked_tasks: HashSet<TaskId>,
    pub completed_tasks: HashSet<TaskId>,
    pub collected: HashMap<ItemId, u32>,
    pub version: u64,                          // bumped on every mutation
}
```

`collected` is shared across both categories — collecting 5 bolts contributes toward both a tracked upgrade and a tracked task that need them. This is correct: in the game, an item in your stash counts toward anything.

Wrap in `Arc<RwLock<AppState>>`. Both the GUI thread (toggle tracking/done, +/- buttons) and the VR thread (clicks on overlay) mutate state; both must take the write lock briefly. The `version` counter lets the VR thread decide cheaply whether to re-render the overlay texture.

### 4.3 Persisted user state

Separate from runtime state — only the user-mutable fields go to disk:

```rust
#[derive(Serialize, Deserialize)]
struct PersistedState {
    schema_version: u32,         // start at 1
    data_version: String,        // for compat checking
    tracked_upgrades: HashSet<UpgradeId>,
    completed_upgrades: HashSet<UpgradeId>,
    tracked_tasks: HashSet<TaskId>,
    completed_tasks: HashSet<TaskId>,
    collected: HashMap<ItemId, u32>,
}
```

On load: if `data_version` mismatches the bundled data, drop orphaned tracked/completed entries (IDs that don't resolve to a current upgrade or task), surface a warning in the GUI's header bar. Keep `collected` counts as-is — items rarely get renamed, user effort shouldn't vanish on a wipe.

### 4.4 Derived view

```rust
pub struct ActiveItem {
    pub item_id: ItemId,
    pub icon_path: String,
    pub name: String,
    pub needed: u32,           // sum across all tracked, not-completed sources
    pub collected: u32,
    pub sources: Vec<String>,  // e.g. ["Workbench L2", "Task: Tracking Device"]
                               // shown as tooltip in GUI, hidden in VR overlay
}

impl AppState {
    pub fn active_items(&self) -> Vec<ActiveItem> {
        // For each upgrade in tracked_upgrades but NOT in completed_upgrades:
        //   For each requirement: add quantity to a per-item total.
        //   Append upgrade name to that item's sources list.
        // Same loop for tracked_tasks / completed_tasks.
        // Build ActiveItem list with collected counts pulled from self.collected.
        // Sort: incomplete first (by name), then completed (grayed, by name).
    }
}
```

Recompute on demand. Cheap (max a few hundred entries). No caching.

---

## 5. Data Pipeline

**Critical update on data acquisition:** ExfilZone Assistant is open source at `github.com/zelengeo/exfil-zone-assistant` under the **MIT license**. The hideout upgrade and task data live as TypeScript/JSON files in `src/lib/`, with icons in `public/images/`. **We do not scrape HTML.** We pull source files directly from the upstream repo. This is vastly more robust, immune to website redesigns, and explicitly permitted by their MIT license (with attribution).

The data pipeline is a **standalone binary** (`crates/scraper`), run manually by maintainers when the upstream repo updates (typically once per wipe). It writes `data.json` + `icons/` into `crates/app/src/assets/`, which the app binary then embeds at compile time via `rust-embed`. The app does no network requests at runtime.

### Inputs

Confirmed paths in `github.com/zelengeo/exfil-zone-assistant` (MIT licensed, maintained by pogapwnz) as of investigation:

- **Hideout upgrades:** `src/data/hideout-upgrades.ts` — a single TypeScript `export const hideoutUpgrades = { ... }` object literal. Keys are `<AreaId>Lv<level>`. Each upgrade has `areaId`, `categoryId`, `level`, `upgradeName`, `upgradeDesc`, `price`, `exchange: { item_id: qty, ... }` (the structured requirements we want), `levelConditions`, `relatedQuests`, `levelUpIcon`.
- **Tasks:** `src/data/tasks.ts` — `export const tasksData: TasksDatabase = { ... }` keyed by task id (e.g. `"ark_11"`). Each task has `id`, `name`, `gameId`, `description`, `objectives: string[]` (free-text), `corpId`, `type: TaskType[]`, `map`, `reward`, `preReward`, `requiredTasks` (prerequisites), `requiredLevel`, `tips`, `videoGuides`, `order`. **CRITICAL:** there is no structured `requirements` field for submission tasks — the items + quantities only appear in the human-readable `objectives` strings (e.g. `"Turn in 9 Intel Items Found In Raid"`).
- **Corps (vendors):** also in `src/data/tasks.ts`, `export const corps: Record<string, Corp>`. `corpId` on each task maps here.
- **Item catalogs:** `public/data/{ammunition,armor,attachments,backpacks,face-shields,grenades,helmets,holsters,keys,magazines,medical,misc,provisions,task-items,weapons}.json` — already JSON. Each entry has `id`, `name`, `category`, `subcategory`, `images.icon` (path under `public/`), `stats.{price,weight,rarity,...}`. Task items additionally have `stats.taskIds: string[]` linking them back to the tasks they belong to.
- **Icons:** `public/images/items/<category>/<filename>.webp` (referenced by `images.icon`); hideout icons under `public/images/hideout/`; task/corp icons under `public/images/tasks/`.
- **Game version / data version:** check `package.json` (`version` field) and the most recent commit timestamp. There is no game-version constant we can scrape; we use `<package_version>+<commit_short>` as our `data_version`.

#### Task-requirement extraction strategy (v1)

Because the upstream doesn't store structured submission requirements, the scraper must parse `objectives` strings. Strategy:

1. **Direct link via `task-items`:** for every item in `task-items.json` with non-empty `stats.taskIds`, that item is required by each listed task with quantity 1 (or the number parsed from the matching objective if found).
2. **Regex parse for quantities:** apply patterns like `^(?:Turn in|Submit|Deliver|Find|Collect|Retrieve|Provide)\s+(\d+)\s+(.+?)(?:\s+(?:Items?|in raid|Found In Raid))?$` (case-insensitive) to each objective string. If the matched item-name fuzzy-matches a known item from any catalog, record `(item_id, qty)`.
3. **Fallback:** if no quantity, default to 1. If no item-name match, log a parse warning and skip that requirement.
4. **Coverage report:** the scraper prints, at the end, "Parsed N submission tasks; M required items extracted; K objectives unparsed (logged to scraper.log)". This lets the maintainer review coverage before shipping.
5. Tasks whose `type` contains only non-submission verbs (`eliminate`, `extract`, `reach`, `mark`, `place`, `photo`, `signal`) AND that yielded zero parsed requirements after step 1-3 are dropped from the dataset (nothing to track).

Document any objective strings the regex misses in `OPEN_QUESTIONS.md` for follow-up.

### Process
1. Sparse-clone or shallow-clone the upstream repo into a temp directory (`git clone --depth=1 https://github.com/zelengeo/exfil-zone-assistant.git`), or pull individual files via `https://raw.githubusercontent.com/zelengeo/exfil-zone-assistant/master/<path>` for surgical updates
2. Locate the data files by inspecting `src/lib/` — find the hideout upgrade and task data structures
3. Parse the data. Strategy depends on what's there:
   - If it's a JSON file (e.g. a Mongo seed): parse with `serde_json` directly
   - If it's a TypeScript file with plain object literals: use a small JS evaluator (e.g. `boa_engine` crate) or extract the relevant object via regex + careful parsing. As a last resort, write a small `node` script that imports the TS module and dumps JSON to stdout — call it from the Rust scraper binary.
4. Filter tasks: drop any with zero item-collection requirements (kill/plant/extract-only objectives)
5. Map upstream item / upgrade / task IDs to our internal IDs (prefer 1:1 mapping; document any rule)
6. Copy each unique icon from `public/images/` to `crates/app/src/assets/icons/<item_id>.png`. Re-encode through the `image` crate to normalize format and strip metadata.
7. Write `crates/app/src/assets/data.json` (pretty-printed, sorted keys, for diff readability)
8. Record the upstream commit hash and game version (from `next.config.ts` or similar) in `data.json` as `source_commit` and `game_version`

### Outputs
- `crates/app/src/assets/data.json`
- `crates/app/src/assets/icons/*.png`
- A short `crates/app/src/assets/SOURCE.md` documenting the upstream commit, game version, and run timestamp

### Constraints
- Datasync is run manually by maintainers, not at app runtime
- Failures should be loud (exit non-zero, print which file/parse failed)
- Produce partial output rather than nothing if some entries fail to parse — log clearly what was skipped

### Licensing & attribution
- Icons and data originate from ExfilZone Assistant under MIT
- The MIT license requires preserving the copyright notice and license text — bundle their LICENSE file at `LICENSES/exfil-zone-assistant-MIT.txt` and reference it in our About dialog
- The MIT license does NOT require our project to also be MIT, but matching the upstream is the friendly default

## 6. Desktop GUI

Window title: "EZ Wishlist Overlay". Default size: 1200×800. Resizable, min 800×600.

### Layout
```
┌─────────────────────────────────────────────────────────────────┐
│ Header: data version │ status │ open data folder │ about │ reset│
├──────────────────────────────────┬──────────────────────────────┤
│ [Hideout]  [Tasks]               │ Preview pane (right, 40%)    │
│                                  │                              │
│ — Hideout tab content —          │ Active items in overlay:     │
│ ▼ Workbench                      │                              │
│   ☐ Track ☐ Done  Level 1        │ [icon] Bolts        12/20    │
│     ▸ 5× Bolts, 3× Screws        │   ↳ Workbench L2,            │
│   ☐ Track ☐ Done  Level 2        │     Task: Tracking Device    │
│ ▼ Medstation                     │ [icon] Screws        8/15    │
│   ...                            │ [icon] Wire         0/3      │
│                                  │ ...                          │
└──────────────────────────────────┴──────────────────────────────┘
```

### Left pane: tabbed
Two tabs at the top: **Hideout Upgrades** and **Tasks**. Tab state is purely UI (not persisted). Both feed the same preview pane and overlay.

#### Hideout tab (`gui/hideout_pane.rs`)
- Collapsible group per `HideoutModule`
- Each `Upgrade`: two checkboxes side by side (Track / Done), then upgrade name + level
- Below each upgrade row, a one-line summary of required items (collapsed; expandable for detail)
- Track and Done are mutually exclusive in display, both stored: marking Done auto-unchecks Track and vice versa

#### Tasks tab (`gui/tasks_pane.rs`)
- Collapsible group per `Vendor`
- Search/filter box at the top (filter task names — useful since the task list is long)
- Each `Task`: two checkboxes (Track / Done), task name, vendor tag
- Below each row, a one-line summary of required items + prerequisites list (e.g. "Requires: Tracking Device")
- An "Open in browser" small link icon → opens `task.source_url` (the Assistant page) for full walkthrough
- Prerequisites are info-only — we don't prevent tracking a task whose prereqs aren't done

### Preview pane (`gui/preview_pane.rs`)
- Shows exactly what `active_items()` returns — aggregated across upgrades AND tasks
- Each row: icon (32×32), name, `collected/needed` with progress bar
- Below each item, a small grey caption listing sources (e.g. "Workbench L2 • Task: Tracking Device") so the user can see why it's on the list
- Fully-collected items shown grayed at the bottom
- `+`/`-` buttons next to each row to adjust collected count from the desktop (useful for testing without VR, and for setting a starting count from your existing stash)
- Click count text to type a specific value

### Header bar
- "Data: 2026-Q1-wipe" badge (color-coded: green if matches persisted state, amber if mismatched)
- SteamVR status indicator: dot + "Connected" / "Not running" / "Overlay error: <msg>"
- "Open data folder" button → opens `%APPDATA%/ez-wishlist-overlay/` in Explorer
- "About" button → opens about/credits dialog (see section 6.1)
- "Reset progress" button → confirmation dialog → clears all tracked/completed sets and collected counts

### 6.1 About / Credits dialog

A simple modal with:

```
EZ Wishlist Overlay v<X.Y.Z>
Data version: <game_version> (synced from upstream <commit_short>)

A free, open-source companion for Contractors Showdown: ExfilZone.
Tracks hideout upgrades and quest tasks across desktop and VR.

— Credits —

Hideout, task, and item data are sourced from ExfilZone Assistant
by pogapwnz, used under the MIT license. ExfilZone Assistant is
an excellent web companion covering combat simulators, weapon
databases, guides, and more. If you find this app useful, check
theirs too.

  [ Open ExfilZone Assistant ↗ ]   (links to https://www.exfil-zone-assistant.app/)
  [ Support pogapwnz on Ko-fi ↗ ]  (links to https://ko-fi.com/J3J41GATK0)

Game by Caveman Studio.

— License —

EZ Wishlist Overlay is open source under <our LICENSE>.
ExfilZone Assistant data and icons are used under MIT —
see LICENSES/exfil-zone-assistant-MIT.txt for the full text.
Game content © Caveman Studio.

  [ GitHub ↗ ]   [ Report an issue ↗ ]
```

Tone: genuine recommendation, not a banner ad. The "Support pogapwnz" link is appropriate because we are literally building on their work for free — a natural courtesy.

### Interactions
- All mutations go through methods on `AppState` that bump `version` and schedule a debounced save
- No undo in v1 (logged in `OPEN_QUESTIONS.md` as a deferred feature)

---

## 7. VR Overlay

### 7.1 Setup
On app start, in a dedicated thread:
1. Initialize OpenVR (`vr::VR_Init` with `EVRApplicationType::Overlay`)
2. If init fails (SteamVR not running), retry every 5 seconds; do not crash the desktop app
3. Create an overlay via `IVROverlay::CreateOverlay` with key `"com.etienneb.ez-wishlist-overlay.main"`
4. Configure:
   - Width in meters: 0.6 (tweakable in config later)
   - Anchor: HMD-relative, positioned ~1.5m forward and ~0.8m above eye line (so it's overhead when looking up)
   - Flags: `VROverlayFlags_MakeOverlaysInteractiveIfVisible`
   - Input method: `VROverlayInputMethod_Mouse`

### 7.2 Visibility (`vr/pose.rs`)

Trigger on **angle of look-up**, not absolute world position. The overlay should appear when the user deliberately tilts their gaze upward.

**Pitch convention:** measured as the angle between the HMD forward vector and the horizontal plane. `0°` = looking straight ahead, `+90°` = looking straight up, `-90°` = looking straight down.

**Hysteresis thresholds:**
- Pitch ≥ **60°** → show the overlay
- Pitch ≤ **45°** → hide the overlay
- Between 45° and 60°: hold the previous state

**Dwell delay** to filter out brief glances:
- Must sustain pitch ≥ 60° for **350ms** before the overlay fades in
- Hide is immediate once pitch drops below 45° (no dwell on the way out — instant responsiveness when looking down)

**Anchor:** HMD-relative (yaw + position follow the head; the overlay stays "above" the user). Position it 1.2m forward of the HMD along the head's yaw, tilted so the surface faces the user when they're looking up at it. Distance and exact pitch of the overlay surface are tweakable constants in `vr/pose.rs`.

**Fade:** 150ms alpha fade in/out via `SetOverlayAlpha`. When hidden, skip rendering work entirely.

Poll HMD pose every VR frame (~90Hz) via `WaitGetPoses` / `GetDeviceToAbsoluteTrackingPose`. Extract pitch from the rotation matrix.

These thresholds are placeholders — expose them as compile-time constants in `vr/pose.rs` for easy tuning during the manual VR test pass.

### 7.3 Rendering (`vr/render.rs`)
- Maintain a CPU pixel buffer (`Vec<u8>`, RGBA8) sized 1024×1024 (tweakable)
- Re-render only when:
  - `AppState.version` has changed since last render, OR
  - Overlay is transitioning from hidden to visible
- Use `tiny-skia` for compositing (rect fills, text via `fontdue` or `tiny-skia-text`, icon blits)
- Layout: grid of cells (default 6 columns, wrap to N rows), each 160×160 with 8px padding
- Each cell renders:
  - The item icon (scaled to fit) — pre-decoded from the embedded WebP/PNG at startup, cached as RGBA `tiny_skia::Pixmap`
  - Text `collected / needed` overlaid bottom-right
  - If `collected >= needed`: 50% alpha + desaturate
- After rendering, hand the buffer to OpenVR via `SetOverlayRaw(handle, ptr, w, h, depth=4)`. This avoids any GPU device interop.

### 7.4 Input (`vr/input.rs`)

Single-click interaction model:

- In the VR thread main loop, call `PollNextOverlayEvent` each iteration
- Handle `VREvent_MouseButtonDown` (left/primary button only — no right-click in v1):
  - Read `event.data.mouse.x`, `event.data.mouse.y` (overlay-local coordinates)
  - Hit-test against the grid layout used in the last render
  - If hit, apply the cycle rule:
    - If `collected[item_id] < needed`: `collected[item_id] += 1`
    - If `collected[item_id] >= needed`: `collected[item_id] = 0` (cycle back, lets the user fix overshoot or restart)
  - Bump `state.version`
- Haptic feedback: short pulse via `TriggerHapticVibrationAction` on the controller that sent the click. Distinct pattern when cycling back to 0 (e.g. double-pulse) so the user has confidence the reset was intentional.
- Debounce: ignore the same grid cell receiving repeat clicks within 100ms (prevents accidental double-taps from controller jitter).

### 7.5 Lifecycle
- On clean shutdown: `DestroyOverlay`, `VR_Shutdown`
- On VR thread panic: log, then attempt reinit after a delay; do not bring down the GUI thread

---

## 8. Persistence

### 8.1 Locations
- State: `%APPDATA%/ez-wishlist-overlay/state.json`
- Logs: `%APPDATA%/ez-wishlist-overlay/logs/app-YYYY-MM-DD.log` (rotated daily, keep 7 days)
- No config file in v1 (no user-tunable settings yet)

### 8.2 Save strategy
- Every mutation bumps `state.version` and schedules a save 500ms in the future
- If another mutation lands before the save fires, reset the timer (debounce)
- Save = write to `state.json.tmp`, then atomic rename to `state.json`
- Save errors logged, surfaced as a yellow banner in the GUI

### 8.3 Load
- Read on app start
- If file is missing: start with empty state
- If file is corrupt: rename to `state.json.corrupt-<timestamp>`, start fresh, show a warning banner with the backup path
- If `schema_version` is higher than supported: refuse to load, show clear error

---

## 9. Threading Model

| Thread | Owns | Reads | Writes |
|---|---|---|---|
| Main / GUI (egui) | Event loop, window | `AppState` (read) | `AppState` (mutate), triggers save |
| VR | OpenVR handle, CPU pixel buffer for overlay | `AppState` (read) | `AppState` (mutate via clicks) |
| Save (tokio task) | The save debounce | `AppState` (read snapshot) | Disk only |

All `AppState` access goes through `Arc<RwLock<AppState>>`. Lock granularity: per operation, never held across `.await` points. Mutations are short and synchronous.

Channels (`crossbeam::channel` or `tokio::sync::mpsc`) for cross-thread notifications:
- VR thread → GUI: connection state changes ("connected", "disconnected", "error: ...")
- GUI → save task: "state changed at version N"

---

## 10. Edge Cases (must handle, with tests where practical)

1. **SteamVR not running at startup** — GUI fully functional, VR thread retries.
2. **SteamVR closes mid-session** — VR thread detects, marks status, retries.
3. **Same item required by multiple tracked sources** — quantities sum in `active_items()`. The `sources` field lets users see what's contributing.
4. **User over-collects** — not possible via VR clicks (cycle resets to 0 at target). Possible via desktop +/- buttons; if it happens, `collected` may exceed `needed`. Display clamps to `needed` in the overlay; preview pane shows actual value.
5. **Upgrade or task marked done after partial collection** — items disappear from active view if no other tracked source needs them; collected counts persist.
6. **Data version change after wipe** — load drops orphaned tracked/completed IDs (both upgrades and tasks), surfaces warning, keeps collected counts.
7. **Corrupt state.json** — backed up with timestamp, fresh start, banner shows backup path.
8. **No tracked upgrades or tasks** — overlay shows a placeholder ("Nothing tracked. Enable upgrades or tasks in the desktop app.") instead of an empty grid.
9. **Many tracked items → grid overflows** — paginate or scroll. v1: cap visible cells at 36, show "+N more" indicator and a paging button (controller bumper).
10. **User clicks an item already at target** — cycle resets `collected` to 0 (intentional, lets user recover from accidents). Distinct haptic pattern confirms the reset.
11. **HMD pose stale or invalid** — treat as "not looking up", hide overlay.
12. **Task with no item requirements appears in scraped data** — scraper bug; the scraper must exclude these. If one slips through, the app filters it at load time and logs a warning.
13. **Task prerequisites not met** — not enforced; user can track any task. UI shows the prerequisite list as info.

---

## 11. Error Handling Conventions

- `anyhow::Result` for top-level returns in app code
- All errors logged via `tracing` before being surfaced
- User-facing errors shown as banners in the GUI header (never modal dialogs except for destructive confirms)
- VR errors: status indicator + log entry; never crash the GUI
- Panics: install `std::panic::set_hook` to log before unwinding; if the VR thread panics, GUI continues

---

## 12. Build & Distribution

### Local dev
- `cargo run -p app` from the workspace root
- `cargo run -p scraper -- --output crates/app/src/assets/` to regenerate data from the upstream repo

#### `scraper` CLI surface

```
scraper [OPTIONS]

OPTIONS:
  --output <DIR>      Output directory for data.json + icons/ + SOURCE.md.
                      Default: crates/app/src/assets/
  --repo <URL>        Upstream git URL.
                      Default: https://github.com/zelengeo/exfil-zone-assistant
  --ref <REF>         Branch / tag / commit to check out.
                      Default: master
  --upstream <DIR>    Use a pre-cloned upstream at DIR instead of cloning fresh.
                      Useful for iterating without re-downloading.
  --keep-temp         Don't delete the cloned upstream directory on success.
  --skip-icons        Skip icon copying (data.json only). Faster for iteration.
  --no-network        Require --upstream; refuse to clone. CI-friendly.
  -v, --verbose       Verbose logging (parse warnings, per-task decisions).
```

The tool exits non-zero on hard failures (clone fail, unreadable files, no upgrades found). Per-task parse failures are reported as warnings and counted in the final summary, not fatal.

### Release
- `cargo-dist` configured for `x86_64-pc-windows-msvc` only (v1)
- GitHub Actions workflow on tags `v*` triggers build → upload artifacts → create release
- MSI installer via `cargo-wix`:
  - Install location: `%ProgramFiles%/EZ Wishlist Overlay/`
  - Start menu shortcut
  - No uninstall residue in `%APPDATA%` by default (user data stays unless explicitly purged via an "Uninstall and delete data" option — v2)
- **Unsigned in v1.** Document the SmartScreen prompt in the README. Revisit signing if usage justifies cert cost.

### Versioning
- Semver. `0.x.y` until v1.0 (which requires VR + GUI both stable + cargo-dist pipeline green).
- Each release notes the bundled `data_version`.

---

## 13. Implementation Phases

Each phase ends with a runnable, demoable artifact. Don't skip phases or merge them.

### Phase 1 — Datasync + data shape
- Build the `scraper` binary
- Clone the upstream ExfilZone Assistant repo, locate and parse the hideout + tasks data
- Filter task list to only item-collection tasks
- Produce `data.json` + icons committed to repo
- Bundle `LICENSES/exfil-zone-assistant-MIT.txt`
- Validate by writing a CLI test command in the app that pretty-prints the loaded data
- **Deliverable:** committed data files, scraper README explaining how to re-sync after upstream updates

### Phase 2 — Desktop GUI, no VR
- egui app with tabbed left pane (Hideout / Tasks), preview pane, about dialog
- Search/filter for tasks
- Persistence working (save/load/debounce)
- All edge cases for data versioning handled
- **Deliverable:** standalone desktop app, fully usable as a manual tracker for both hideout upgrades and tasks

### Phase 3 — VR overlay, read-only
- OpenVR init, overlay creation, pose-driven show/hide
- Renders the same `active_items()` view as the preview pane
- No interaction yet — overlay is display-only
- **Deliverable:** can put on headset, look up, see the list update live as desktop changes

### Phase 4 — VR interaction
- Laser-pointer click increments (primary button only)
- Gray-out on complete
- Cycle back to 0 when clicking an already-complete item (with distinct haptic)
- Haptic feedback on every click
- **Deliverable:** fully functional v1

### Phase 5 — Distribution
- `cargo-dist` + `cargo-wix` configs
- GitHub Actions release workflow
- README, screenshots, attribution
- **Deliverable:** downloadable MSI from GitHub Releases

### Phase 6 (post-v1) — Polish
- Overlay positioning configurability
- Color themes
- Per-upgrade priority ordering
- Pagination polish for many items
- Telemetry-free crash reporting (opt-in)

---

## 14. Testing

- **Unit tests:** `active_items()` aggregation logic, persistence round-trip, data-version migration, hysteresis state machine
- **Integration tests:** load fixture `data.json` → mutate state → assert overlay-input handler produces the right state change
- **Manual VR checklist** (in `TESTING.md`):
  - Cold start with SteamVR off → start SteamVR → overlay appears
  - Cold start with SteamVR on → overlay appears within 2s
  - Look up/down repeatedly → no flicker at boundary
  - Click each item type → counts increment, haptic fires
  - Click an item already at target → resets to 0 with distinct haptic
  - Look up while climbing stairs → overlay does NOT appear (350ms dwell + 80° threshold filters this)
  - Mark an upgrade done in desktop → matching items disappear from overlay within 1s
- Fixtures live in `crates/app/tests/fixtures/`

No CI-driven VR testing — OpenVR can't be mocked usefully. Manual checklist is the gate before tagging a release.

---

## 15. Out of scope, do not implement in v1

- Screenshot capture
- OCR
- Vision LLM calls
- Multi-language UI (English only)
- Settings dialog
- Cloud sync
- Stash value calculator
- Trade route calculator
- Map / key location features
- Non-collection task objectives (kills, planting, area reaches)
- Mac/Linux builds

If a feature isn't in this spec, push back to the spec rather than adding it inline.

---

## 16. Conventions

- `cargo fmt` and `cargo clippy -- -D warnings` clean on every commit
- Commit messages: conventional commits style (`feat:`, `fix:`, `chore:`, etc.)
- Branches: `main` is always releasable; feature branches PR'd in
- `unsafe` blocks: allowed only in `vr/` for FFI, must have a `// SAFETY:` comment explaining the invariant
- No `unwrap()` or `expect()` in main app code paths. Tests are fine.
- Public APIs documented with `///` doc comments

---

## 17. References

- OpenVR docs: https://github.com/ValveSoftware/openvr/wiki
- IVROverlay overview: https://github.com/ValveSoftware/openvr/wiki/IVROverlay_Overview
- `cargo-dist`: https://opensource.axo.dev/cargo-dist/
- `cargo-wix`: https://github.com/volks73/cargo-wix
- egui: https://www.egui.rs/
- wgpu: https://wgpu.rs/

---

## 18. Project setup commands (first session)

```bash
# Create workspace
cargo new --vcs git ez-wishlist-overlay
cd ez-wishlist-overlay
# Configure as workspace in root Cargo.toml
mkdir -p crates/app crates/scraper
cargo new --lib crates/app   # convert to bin in Cargo.toml: [[bin]] name = "ez-wishlist-overlay"
cargo new crates/scraper

# Set MSRV and dependencies — see section 2
# Commit initial structure
git add . && git commit -m "chore: initial workspace structure"
```

After that, work through phases in order.
