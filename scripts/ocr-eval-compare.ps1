# Noise-aware compare of two OCR eval reports for the tuning loop.
#
# Why this exists: Windows.Media.Ocr is non-deterministic -- the native
# owned-count read can jitter ~+/-2 cells run-to-run (see the
# `owned_count_accuracy_floor_on_native_pngs` doc-comment). So a single
# before/after diff can't tell a real improvement from engine noise. The
# `eval_report_json` diagnostic captures a min/median/max band over
# `OCR_EVAL_RUNS` runs; this script diffs two of those reports and emits
# a KEEP / REVERT / NOISE verdict the `ocr-tune` loop acts on.
#
# Decision rule:
#   REGRESSION (revert) if ANY of:
#     - a gate dropped: fewer fixtures identified, or wrong_writes_max rose
#       (a wrong, non-UNREAD read = the pipeline would overwrite real
#       progress with garbage), or
#     - any fixture's candidate median fell below its baseline MIN
#       (regressed beyond that fixture's own noise band).
#   IMPROVEMENT (keep) if no regression AND the total median rose AND even
#     the candidate's WORST run >= the baseline's typical (median) -- i.e.
#     the gain clears the band, not just the mean.
#   WITHIN NOISE otherwise.
#
# Exit codes: 0 = improvement, 1 = regression/gate-fail, 2 = within noise.
#
# Usage:
#   ./scripts/ocr-eval-compare.ps1 -Baseline base.json -Candidate cand.json
#
# Produce the inputs with (Windows dev-shell, see memory/toolchain.md):
#   $env:OCR_EVAL_RUNS=5; $env:OCR_EVAL_OUT="C:\zt\ocr-eval\base.json"
#   cargo test -p ez-wishlist-overlay --target x86_64-pc-windows-msvc `
#     ocr::pipeline::fixture_tests::eval_report_json -- --ignored --nocapture
#
# Always capture the baseline in the SAME session/build as the candidate:
# the jitter (and even the median) can shift across machines/builds, so a
# stale committed baseline is not a safe reference.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Baseline,
    [Parameter(Mandatory = $true)][string]$Candidate
)

$ErrorActionPreference = 'Stop'

function Read-Report($path) {
    if (-not (Test-Path $path)) { throw "eval report not found: $path" }
    return Get-Content -Raw -Path $path | ConvertFrom-Json
}

$base = Read-Report $Baseline
$cand = Read-Report $Candidate

# Index fixtures by stem (ConvertFrom-Json gives PSCustomObjects, not maps).
$baseFx = @{}
foreach ($f in $base.fixtures) { $baseFx[$f.stem] = $f }
$candFx = @{}
foreach ($f in $cand.fixtures) { $candFx[$f.stem] = $f }

Write-Output "OCR eval compare"
Write-Output ("  baseline : {0}  (runs={1})" -f $Baseline, $base.runs)
Write-Output ("  candidate: {0}  (runs={1})" -f $Candidate, $cand.runs)
Write-Output ""

$bt = $base.totals
$ct = $cand.totals

# --- Hard gates -----------------------------------------------------------
$gateFail = @()
if ($ct.identified -lt $bt.identified) {
    $gateFail += ("identification dropped: {0} -> {1} of {2} fixtures" -f $bt.identified, $ct.identified, $ct.fixtures)
}
if ($ct.wrong_writes_max -gt $bt.wrong_writes_max) {
    $gateFail += ("wrong_writes_max rose: {0} -> {1} (pipeline would overwrite real progress)" -f $bt.wrong_writes_max, $ct.wrong_writes_max)
}

# --- Per-fixture deltas (median[min-max]) ---------------------------------
$regressed = @()
$won = @()
Write-Output "Per-fixture owned-count  baseline med[min-max] -> candidate med[min-max]:"
foreach ($s in ($candFx.Keys | Sort-Object)) {
    $c = $candFx[$s]
    $b = $baseFx[$s]
    if ($null -eq $b) {
        Write-Output ("  {0,-24} (new) -> {1}[{2}-{3}]" -f $s, $c.correct_median, $c.correct_min, $c.correct_max)
        continue
    }
    $mark = '  ~'
    if ($c.correct_median -lt $b.correct_min) { $mark = '  REGRESS'; $regressed += $s }
    elseif ($c.correct_median -gt $b.correct_max) { $mark = '  WIN'; $won += $s }
    Write-Output ("  {0,-24} {1}[{2}-{3}] -> {4}[{5}-{6}]{7}" -f `
            $s, $b.correct_median, $b.correct_min, $b.correct_max, `
            $c.correct_median, $c.correct_min, $c.correct_max, $mark)
}
Write-Output ""

Write-Output ("TOTAL owned-count: {0}[{1}-{2}] -> {3}[{4}-{5}] of {6} labelled" -f `
        $bt.correct_median, $bt.correct_min, $bt.correct_max, `
        $ct.correct_median, $ct.correct_min, $ct.correct_max, $ct.labelled)
Write-Output ("identified: {0} -> {1} of {2};  wrong_writes_max: {3} -> {4}" -f `
        $bt.identified, $ct.identified, $ct.fixtures, $bt.wrong_writes_max, $ct.wrong_writes_max)
Write-Output ""

foreach ($g in $gateFail) { Write-Output ("  GATE FAIL: {0}" -f $g) }
if ($regressed.Count -gt 0) { Write-Output ("  REGRESSED (median below baseline min): {0}" -f ($regressed -join ', ')) }
if ($won.Count -gt 0) { Write-Output ("  WON (median above baseline max): {0}" -f ($won -join ', ')) }
if ($gateFail.Count -gt 0 -or $regressed.Count -gt 0 -or $won.Count -gt 0) { Write-Output "" }

# --- Verdict --------------------------------------------------------------
if ($gateFail.Count -gt 0 -or $regressed.Count -gt 0) {
    Write-Output 'VERDICT: REGRESSION -- revert the change'
    exit 1
}
elseif (($ct.correct_median -gt $bt.correct_median) -and ($ct.correct_min -ge $bt.correct_median)) {
    Write-Output 'VERDICT: IMPROVEMENT -- keep (gain clears the noise band, no regressions)'
    exit 0
}
else {
    Write-Output 'VERDICT: WITHIN NOISE -- no clear change (median did not rise beyond the band)'
    exit 2
}
