# Noise-aware compare of two OCR eval reports for the tuning loop.
#
# Both reports come from `eval_report_json` (via `scripts/ocr-eval.ps1`), which
# scores the three assets independently. This diffs a baseline vs a candidate
# and emits a KEEP / REVERT / NOISE verdict the `ocr-tune` loop acts on.
#
# The assets have different noise profiles, so they're compared differently:
#   - hideout: the live Windows.Media.Ocr read jitters ~+/-2 cells run-to-run
#     (see `owned_count_accuracy_floor_on_native_pngs`), so `eval_report_json`
#     captures a min/median/max band over `OCR_EVAL_RUNS` runs and we only count
#     a move that clears the band.
#   - box / stash: deterministic (scored from the frozen `.boxes.json`), so ANY
#     change in tile accuracy is real, not noise.
#
# Decision rule:
#   REGRESSION (revert) if ANY of:
#     - hideout: fewer fixtures identified, or wrong_writes_max rose (a wrong,
#       non-UNREAD read = the pipeline would overwrite real progress), or any
#       fixture's candidate median fell below its baseline MIN (beyond its band).
#     - box: exact-match was lost (the gate), or its tiles_correct dropped.
#   IMPROVEMENT (keep) if no regression AND at least one asset clearly improved:
#     hideout's worst run >= the baseline median (gain clears the band), or
#     box/stash tiles_correct rose (deterministic, so any rise is real).
#   WITHIN NOISE otherwise.
#   (stash is informational: its delta is reported and a drop is flagged, but it
#   does not by itself force a revert -- name divergences keep its tally inexact.)
#
# Exit codes: 0 = improvement, 1 = regression/gate-fail, 2 = within noise.
#
# Usage:
#   ./scripts/ocr-eval-compare.ps1 -Baseline base.json -Candidate cand.json
#
# Produce the inputs with `./scripts/ocr-eval.ps1 -Out base.json` (capture the
# baseline in the SAME session/build as the candidate -- the hideout jitter and
# even its median can shift across machines/builds, so a stale committed
# baseline is not a safe reference).

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

Write-Output 'OCR eval compare'
Write-Output ('  baseline : {0}  (runs={1})' -f $Baseline, $base.runs)
Write-Output ('  candidate: {0}  (runs={1})' -f $Candidate, $cand.runs)
Write-Output ''

$gateFail = @()
$regressed = @()
$won = @()

# ============================ HIDEOUT (noise band) ============================
$bh = $base.hideout
$ch = $cand.hideout
$bt = $bh.totals
$ct = $ch.totals

# Index fixtures by stem (ConvertFrom-Json gives PSCustomObjects, not maps).
$baseFx = @{}
foreach ($f in $bh.fixtures) { $baseFx[$f.stem] = $f }
$candFx = @{}
foreach ($f in $ch.fixtures) { $candFx[$f.stem] = $f }

if ($ct.identified -lt $bt.identified) {
    $gateFail += ('hideout identification dropped: {0} -> {1} of {2} fixtures' -f $bt.identified, $ct.identified, $ct.fixtures)
}
if ($ct.wrong_writes_max -gt $bt.wrong_writes_max) {
    $gateFail += ('hideout wrong_writes_max rose: {0} -> {1} (would overwrite real progress)' -f $bt.wrong_writes_max, $ct.wrong_writes_max)
}

Write-Output 'hideout per-fixture owned-count  baseline med[min-max] -> candidate med[min-max]:'
foreach ($s in ($candFx.Keys | Sort-Object)) {
    $c = $candFx[$s]
    $b = $baseFx[$s]
    if ($null -eq $b) {
        Write-Output ('  {0,-24} (new) -> {1}[{2}-{3}]' -f $s, $c.correct_median, $c.correct_min, $c.correct_max)
        continue
    }
    $mark = '  ~'
    if ($c.correct_median -lt $b.correct_min) { $mark = '  REGRESS'; $regressed += $s }
    elseif ($c.correct_median -gt $b.correct_max) { $mark = '  WIN'; $won += $s }
    Write-Output ('  {0,-24} {1}[{2}-{3}] -> {4}[{5}-{6}]{7}' -f `
            $s, $b.correct_median, $b.correct_min, $b.correct_max, `
            $c.correct_median, $c.correct_min, $c.correct_max, $mark)
}
Write-Output ''
Write-Output ('hideout TOTAL owned-count: {0}[{1}-{2}] -> {3}[{4}-{5}] of {6} labelled' -f `
        $bt.correct_median, $bt.correct_min, $bt.correct_max, `
        $ct.correct_median, $ct.correct_min, $ct.correct_max, $ct.labelled)
Write-Output ('  identified: {0} -> {1} of {2};  wrong_writes_max: {3} -> {4}' -f `
        $bt.identified, $ct.identified, $ct.fixtures, $bt.wrong_writes_max, $ct.wrong_writes_max)
Write-Output ''

# ========================= BOX / STASH (deterministic) ========================
# Helper: one line + regression/gate bookkeeping for a box-scan asset.
function Compare-Scan($name, $b, $c, [bool]$gated) {
    $line = ('{0,-8} tiles {1}/{2} -> {3}/{4}   exact {5} -> {6}' -f `
            $name, $b.tiles_correct, $b.tiles_total, $c.tiles_correct, $c.tiles_total, $b.all_exact, $c.all_exact)
    $drop = $c.tiles_correct -lt $b.tiles_correct
    $lostExact = $b.all_exact -and -not $c.all_exact
    $rose = $c.tiles_correct -gt $b.tiles_correct
    if ($drop -or $lostExact) { $line += '   REGRESS' }
    elseif ($rose) { $line += '   WIN' }
    Write-Output ('  ' + $line)
    [pscustomobject]@{ Drop = $drop; LostExact = $lostExact; Rose = $rose; Gated = $gated }
}

Write-Output 'box / stash tile accuracy (deterministic):'
$boxCmp = Compare-Scan 'box'   $base.'box' $cand.'box'  $true
$stashCmp = Compare-Scan 'stash' $base.stash $cand.stash $false
Write-Output ''

if ($boxCmp.LostExact) { $gateFail += 'box lost exact-match (the box gate would fail)' }
if ($boxCmp.Drop) { $gateFail += ('box tiles_correct dropped: {0} -> {1}' -f $base.'box'.tiles_correct, $cand.'box'.tiles_correct) }
if ($stashCmp.Drop) {
    # Informational: stash is un-gated (name-divergence misses), but the drop is
    # deterministic, so surface it loudly without forcing a revert.
    Write-Output ('  NOTE: stash tiles_correct dropped {0} -> {1} (informational, not gated)' -f $base.stash.tiles_correct, $cand.stash.tiles_correct)
}

# ================================== VERDICT ===================================
foreach ($g in $gateFail) { Write-Output ('  GATE FAIL: {0}' -f $g) }
if ($regressed.Count -gt 0) { Write-Output ('  HIDEOUT REGRESSED (median below baseline min): {0}' -f ($regressed -join ', ')) }
if ($won.Count -gt 0) { Write-Output ('  HIDEOUT WON (median above baseline max): {0}' -f ($won -join ', ')) }
if ($gateFail.Count -gt 0 -or $regressed.Count -gt 0 -or $won.Count -gt 0 -or $boxCmp.Rose -or $stashCmp.Rose) { Write-Output '' }

$hideoutImproved = ($ct.correct_median -gt $bt.correct_median) -and ($ct.correct_min -ge $bt.correct_median)
$deterministicImproved = $boxCmp.Rose -or $stashCmp.Rose

if ($gateFail.Count -gt 0 -or $regressed.Count -gt 0) {
    Write-Output 'VERDICT: REGRESSION -- revert the change'
    exit 1
}
elseif ($hideoutImproved -or $deterministicImproved) {
    Write-Output 'VERDICT: IMPROVEMENT -- keep (a gain cleared the band / deterministic rise, no regressions)'
    exit 0
}
else {
    Write-Output 'VERDICT: WITHIN NOISE -- no clear change'
    exit 2
}
