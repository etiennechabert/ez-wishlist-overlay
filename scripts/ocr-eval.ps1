# OCR quality pipeline — validate + score the three screenshot assets
# (hideout / box / stash) independently, in one command.
#
# The OCR logic is exercised against the committed fixtures under
# `screenshots/<asset>/` (see `screenshots/CLAUDE.md`). This wires up the
# Windows MSVC dev-shell for you (see memory/toolchain.md) and runs two phases:
#
#   1. VALIDATE — the hard pass/fail gates, per asset:
#        hideout : identification_and_cell_ordering_on_native_pngs  (15/15)
#                  owned_count_accuracy_floor_on_native_pngs        (>= 45)
#        box     : box_scan_matches_label                           (exact)
#      (stash has no gate — its captures can't be stitched; it is scored only.)
#
#   2. SCORE — the `eval_report_json` diagnostic, which scores each asset
#      independently and writes one combined JSON to -Out:
#        hideout : owned-count noise band (min/median/max over -Runs runs) + id
#        box / stash : graded tile accuracy (tiles_correct / tiles_total).
#      The JSON is the input to `scripts/ocr-eval-compare.ps1` (before/after).
#
# Usage:
#   ./scripts/ocr-eval.ps1                        # 5 scoring runs -> C:\zt\ocr-eval\score.json
#   ./scripts/ocr-eval.ps1 -Runs 3 -Out cand.json
#
# Exit code: 0 if the gates pass, 1 if any gate fails or the eval can't run.

[CmdletBinding()]
param(
    [int]$Runs = 5,
    [string]$Out = 'C:\zt\ocr-eval\score.json'
)

$ErrorActionPreference = 'Stop'

$vsInstall = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools'
$devShellDll = Join-Path $vsInstall 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll'
$libclang = 'C:\Program Files\LLVM\bin'
$target = 'x86_64-pc-windows-msvc'

if (-not (Test-Path $devShellDll)) {
    Write-Error ("VS Dev Shell not found at $devShellDll. Install VS Build Tools " +
        '2022 (see memory/toolchain.md) or edit this script if your path differs.')
    exit 1
}
if (-not (Test-Path $libclang)) {
    Write-Error ("LLVM not found at $libclang (openvr_sys bindgen needs " +
        'libclang.dll). `winget install LLVM.LLVM`, or edit this script.')
    exit 1
}

# PATH: VS Installer (for vswhere.exe) + user + machine so cargo/rustc resolve
# even from Claude Code's trimmed shell.
$env:Path = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer;' +
    [System.Environment]::GetEnvironmentVariable('Path', 'User') + ';' +
    [System.Environment]::GetEnvironmentVariable('Path', 'Machine')
$env:LIBCLANG_PATH = $libclang
# Worktree builds overflow MAX_PATH in openvr_sys' CMake step; a short target
# dir avoids it. Respect a caller-set CARGO_TARGET_DIR if present.
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = 'C:\zt' }

Import-Module $devShellDll
Enter-VsDevShell -VsInstallPath $vsInstall -SkipAutomaticLocation `
    -DevCmdArguments '-arch=amd64 -host_arch=amd64' | Out-Null

# LLVM >= 21 bindgen workaround: feed the MSVC INCLUDE dirs to clang as
# -isystem (forward-slash + quoted so shlex/clang accept paths with spaces).
# Harmless on LLVM 20. See memory/toolchain.md.
$env:BINDGEN_EXTRA_CLANG_ARGS = (($env:INCLUDE -split ';' |
    Where-Object { $_ } |
    ForEach-Object { '-isystem "' + ($_ -replace '\\', '/') + '"' }) -join ' ')

# cargo writes progress to stderr; PS 5.1 + Stop wraps those as terminating
# errors when this script is redirected from outside. Drop strict mode for the
# native cargo calls below.
$ErrorActionPreference = 'Continue'

Write-Output '== VALIDATE (hard gates) =='
$gateTests = @(
    'ocr::pipeline::fixture_tests::identification_and_cell_ordering_on_native_pngs',
    'ocr::pipeline::fixture_tests::owned_count_accuracy_floor_on_native_pngs',
    'ocr::pipeline::hideout_data_validation::hideout_labels_match_data_json',
    'ocr::box_scan::tests::box_scan_matches_label',
    'ocr::pipeline::unit_ocr_tests'
)
# Test-name filters go AFTER `--` (libtest matches any); cargo rejects
# multiple positional TESTNAMEs before `--`.
& cargo test -p ez-wishlist-overlay --target $target -- --nocapture @gateTests
$gatesExit = $LASTEXITCODE

Write-Output ''
Write-Output '== SCORE (per-asset eval) =='
$outDir = Split-Path -Parent $Out
if ($outDir) { New-Item -ItemType Directory -Force -Path $outDir | Out-Null }
$env:OCR_EVAL_RUNS = "$Runs"
$env:OCR_EVAL_OUT = $Out
& cargo test -p ez-wishlist-overlay --target $target `
    ocr::pipeline::fixture_tests::eval_report_json -- --ignored --nocapture
$scoreExit = $LASTEXITCODE

if ($scoreExit -ne 0 -or -not (Test-Path $Out)) {
    Write-Error "eval_report_json did not produce $Out (exit $scoreExit)"
    exit 1
}

$r = Get-Content -Raw -Path $Out | ConvertFrom-Json
$h = $r.hideout.totals
$bx = $r.'box'
$st = $r.stash

function Format-Pct($n, $d) { if ($d -gt 0) { '{0,3:N0}%' -f (100.0 * $n / $d) } else { '  -' } }

Write-Output ''
Write-Output "================ OCR quality scorecard (runs=$($r.runs)) ================"
Write-Output ('  hideout  owned-count {0}/{1} [{2}-{3}]   id {4}/{5}   wrong-writes {6}' -f `
        $h.correct_median, $h.labelled, $h.correct_min, $h.correct_max, `
        $h.identified, $h.fixtures, $h.wrong_writes_max)
Write-Output ('  box      tiles {0}/{1} ({2})   exact={3}' -f `
        $bx.tiles_correct, $bx.tiles_total, (Format-Pct $bx.tiles_correct $bx.tiles_total), $bx.all_exact)
Write-Output ('  stash    tiles {0}/{1} ({2})   exact={3}   (informational, not gated)' -f `
        $st.tiles_correct, $st.tiles_total, (Format-Pct $st.tiles_correct $st.tiles_total), $st.all_exact)
Write-Output '  ----  per-item isolated-OCR units (hand-cropped tiles) ----'
foreach ($u in $r.units) {
    Write-Output ('  units {0,-8} {1}/{2} gated ({3})   {4} #hard' -f `
            $u.asset, $u.gated_ok, $u.gated_total, (Format-Pct $u.gated_ok $u.gated_total), $u.hard_total)
}
Write-Output '========================================================================'
Write-Output ('  gates: {0}      full report: {1}' -f $(if ($gatesExit -eq 0) { 'PASS' } else { 'FAIL' }), $Out)

if ($gatesExit -ne 0) { exit 1 }
exit 0
