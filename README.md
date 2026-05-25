<p align="center">
  <img src="crates/app/assets/icon.png" alt="EZ Wishlist Overlay" width="160">
</p>

<h1 align="center">EZ Wishlist Overlay</h1>

<p align="center">
  A Windows desktop + SteamVR overlay companion for <strong>Contractors Showdown: ExfilZone</strong>.<br>
  Tracks hideout upgrade and quest-task item requirements, aggregates them into a single wishlist,<br>
  and surfaces that list in a VR overlay you can glance at by looking up.
</p>

<p align="center">
  <a href="https://github.com/etiennechabert/ez-wishlist-overlay/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License"></a>
  <img src="https://img.shields.io/badge/Rust-stable-DEA584.svg?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/Platform-Windows%20%2B%20SteamVR-0078D4.svg?logo=windows&logoColor=white" alt="Platform">
  <a href="https://github.com/etiennechabert/ez-wishlist-overlay/actions/workflows/ci.yml"><img src="https://github.com/etiennechabert/ez-wishlist-overlay/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://sonarcloud.io/summary/new_code?id=etiennechabert_ez-wishlist-overlay"><img src="https://sonarcloud.io/api/project_badges/measure?project=etiennechabert_ez-wishlist-overlay&metric=security_rating" alt="Security Rating"></a>
  <a href="https://sonarcloud.io/summary/new_code?id=etiennechabert_ez-wishlist-overlay"><img src="https://sonarcloud.io/api/project_badges/measure?project=etiennechabert_ez-wishlist-overlay&metric=reliability_rating" alt="Reliability Rating"></a>
  <a href="https://sonarcloud.io/summary/new_code?id=etiennechabert_ez-wishlist-overlay"><img src="https://sonarcloud.io/api/project_badges/measure?project=etiennechabert_ez-wishlist-overlay&metric=sqale_rating" alt="Maintainability Rating"></a>
</p>

> Out-of-process and anti-cheat-safe. The app never touches the game executable, memory, files, or network traffic — it only talks to SteamVR via the public OpenVR API.

See [SPEC.md](./SPEC.md) for the engineering spec and [OPEN_QUESTIONS.md](./OPEN_QUESTIONS.md) for deferred decisions.

---

## Current status

- ✅ **Phase 1 — Data pipeline.** `crates/scraper` ingests the upstream [ExfilZone Assistant](https://github.com/zelengeo/exfil-zone-assistant) repo and produces `data.json` + normalized PNG icons.
- ✅ **Phase 2 — Desktop GUI.** egui app with tabbed Hideout / Tasks panes, aggregated preview pane, atomic persistence, About dialog. Launches and runs.
- ✅ **Phase 3 — VR overlay.** OpenVR session, pitch-driven show/hide with 350 ms dwell + 150 ms fade, RGBA submission via `SetOverlayRaw`. The overlay anchors in world space at show-time (yaw + position captured from the HMD, pitch/roll dropped) so you can look back down and read it without it following your gaze. Live re-render on state change.
- ✅ **Phase 4 — VR interaction.** Laser-pointer clicks on cells with controller haptic feedback; +1 per click, cycles back to 0 when the target count is hit. Debounced per item.
- ⏳ **Phase 5 — Distribution.** MSI installer is shipping via the [`release.yml`](.github/workflows/release.yml) workflow on tag push; PR builds validate the MSI to keep it green. Code signing + auto-update channel still pending.

---

## Install (end users)

1. **Install Steam, then SteamVR through Steam.** SteamVR is what we talk to over OpenVR — without it the desktop app still runs but the overlay does nothing. Install Steam from [store.steampowered.com](https://store.steampowered.com/), then in the Steam client go to *Library → Tools → SteamVR → Install*.
2. **Download the latest `.msi`** from [Releases](https://github.com/etiennechabert/ez-wishlist-overlay/releases). The portable `.exe` is also there if you'd rather not install — just double-click to run.
3. **Click through Windows SmartScreen.** The MSI is unsigned in v1, so first launch shows *"Windows protected your PC"*. Click **More info** → **Run anyway**. (Going away once we have a code-signing cert — for now the warning is normal, not malware.)
4. **Installer:** standard Welcome → License → Install dir → Finish. Tick *"Launch EZ Wishlist Overlay"* on the last page to start it right away.
5. **First run:** the app opens. If SteamVR isn't running yet, the header shows *"VR: not running"* — start SteamVR (or put on your headset to autolaunch it) and the indicator flips to *"VR: connected"* within a few seconds.
6. **In headset:** look up past ~60° (tweakable in Settings) to bring up the overlay — it anchors in world space in front of you, so you can look back down to read and interact with it without it following your gaze. Point a controller at a cell and click to bump its collected count (+1 per click, wraps to 0 when the target is reached); a short haptic pulse confirms the input. Desktop-side clicks also work and re-render the overlay within a frame.

The MSI installs per-machine under `%ProgramFiles%\EZ Wishlist Overlay\`. User state (tracked upgrades, collected counts, settings) lives in `%APPDATA%\etienneb\ez-wishlist-overlay\`. Uninstall via *Settings → Apps* leaves user state in place; delete the `%APPDATA%` folder by hand if you want a fresh start.

---

## Repo layout

```
crates/
  app/            # the shipped binary (egui desktop + future VR overlay)
  scraper/        # build-time tool that pulls + transforms upstream data
crates/app/src/assets/   # embedded data.json + icons (committed; regenerated by scraper)
LICENSES/                # third-party license texts we ship
SPEC.md
OPEN_QUESTIONS.md
```

---

## Build

### Prerequisites

This repo currently builds against `stable-x86_64-pc-windows-gnullvm` with the [LLVM-MinGW](https://github.com/mstorsjo/llvm-mingw) toolchain on PATH. Install with:

```powershell
winget install MartinStorsjo.LLVM-MinGW.UCRT
rustup default stable-x86_64-pc-windows-gnullvm
```

(MSVC also works once Visual Studio Build Tools with the C++ workload is installed. There's no toolchain pin — pick whichever you prefer.)

The VR layer depends on the [`openvr`](https://crates.io/crates/openvr) crate, whose `openvr_sys` build script compiles Valve's OpenVR C++ SDK via cmake + bindgen. On Windows that also requires:

```powershell
winget install Kitware.CMake
winget install LLVM.LLVM       # for libclang on PATH (bindgen)
```

The VR layer is gated behind `cfg(target_os = "windows")` — macOS and Linux builds skip it entirely and the app falls back to "VR: unavailable on this OS" so the desktop GUI stays iterable cross-platform.

### Run the desktop app

From a **VS Developer PowerShell**:

```powershell
cargo app           # release build
cargo app-dev       # debug build, faster compiles for UI iteration
```

From a **plain PowerShell** (uses the bundled launcher to enter the VS Dev Shell + set `LIBCLANG_PATH`):

```powershell
./scripts/run-app.ps1            # debug build
./scripts/run-app.ps1 -Release   # release build
```

Both aliases pin the MSVC target — the default gnullvm toolchain can't build `openvr_sys` (it rejects MSVC's `/DWIN32` cxxflag), so VR-enabled builds go through MSVC. See [`scripts/run-app.ps1`](./scripts/run-app.ps1) for the underlying setup.

The window opens at 1200×800. State is persisted to `%APPDATA%\etienneb\ez-wishlist-overlay\data\state.json` on Windows, or the platform equivalent (`~/Library/Application Support/...` on macOS) elsewhere.

### Cargo aliases

The workspace ships a set of aliases in [`.cargo/config.toml`](./.cargo/config.toml) so common commands work the same on Windows, macOS, and Linux without any extra tooling:

| Alias              | Expands to                                      |
| ------------------ | ----------------------------------------------- |
| `cargo app`        | `run -p ez-wishlist-overlay --release --target x86_64-pc-windows-msvc` |
| `cargo app-dev`    | `run -p ez-wishlist-overlay --target x86_64-pc-windows-msvc`           |
| `cargo scrape`     | `run -p scraper --release`                      |
| `cargo t`          | `test --workspace`                              |
| `cargo c`          | `check --workspace --all-targets`               |
| `cargo l`          | `clippy --workspace --all-targets -- -D warnings` |
| `cargo fmt-check`  | `fmt --all -- --check`                          |

---

## Refreshing game data (maintainers only)

The scraper is a separate binary that's run manually when the upstream repo publishes a new wipe's data.

```powershell
# Clones upstream into %TEMP%, parses + transforms, writes assets, then cleans up.
cargo scrape

# Or, if you already have a local clone:
cargo scrape -- --upstream C:\path\to\exfil-zone-assistant
```

After running the scraper, rebuild the app — `data.json` and `icons/` are embedded via `rust-embed` at compile time.

Output:

- `crates/app/src/assets/data.json`
- `crates/app/src/assets/icons/<item_id>.png`
- `crates/app/src/assets/SOURCE.md` (provenance: upstream commit, version, timestamp)
- `crates/app/src/assets/unparsed-objectives.log` (task-objective strings the regex couldn't split into item + quantity; review these before shipping)

See `cargo run -p scraper -- --help` for full CLI options (`--repo`, `--ref`, `--upstream`, `--no-network`, `--skip-icons`, `--keep-temp`, `--verbose`).

---

## Releasing

Releases are produced by [`.github/workflows/release.yml`](./.github/workflows/release.yml). To cut one:

```bash
git tag v0.1.0 && git push --tags
```

The workflow runs on a Windows runner:

1. Installs LLVM (for `openvr_sys`'s bindgen step) and the WiX toolset.
2. Builds `cargo build --release -p ez-wishlist-overlay`.
3. Runs `cargo wix -p ez-wishlist-overlay --no-build` against [`crates/app/wix/main.wxs`](./crates/app/wix/main.wxs) to produce a per-machine MSI.
4. Attaches both the standalone `ez-wishlist-overlay-<version>-x86_64.exe` (portable) and the `.msi` installer to a GitHub Release.

The `UpgradeCode` GUID in `main.wxs` is fixed — never change it once a release ships, or upgrades for existing installs break. The `Product/@Id='*'` regenerates per build so each MSI has a unique `ProductCode`.

`workflow_dispatch` is enabled, so you can fire a dry build from the Actions tab without tagging — artifacts are uploaded to the workflow run but skip the Release upload step.

---

## Credits

Hideout, task, and item data are sourced from [ExfilZone Assistant](https://www.exfil-zone-assistant.app/) by [pogapwnz](https://ko-fi.com/J3J41GATK0), used under the MIT license. The bundled MIT text is at [`LICENSES/exfil-zone-assistant-MIT.txt`](./LICENSES/exfil-zone-assistant-MIT.txt). If you find this app useful, check theirs too — it covers a lot more than hideout/quests (combat simulators, weapon databases, guides).

Game content © Caveman Studio.
