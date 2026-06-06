# Write committed, human-readable OCR result sidecars from an eval report.
#
# `eval_report_json` (via `scripts/ocr-eval.ps1`) scores the fixtures and emits
# one combined JSON. This turns that JSON into a small `*.ocr-result.txt` file
# next to EACH fixture so the repo records, at a glance, which capture the OCR
# reads correctly and which it doesn't — committed alongside the screenshot it
# describes (unlike the timestamped `*.ocr-debug.*.txt` dumps, which are
# gitignored in-flight diagnostics).
#
#   - hideout/<UpgradeId>.ocr-result.txt : per-cell PASS/FAIL (owned-count read
#     vs the hand label) + identification status.
#   - box/box.ocr-result.txt, stash/stash.ocr-result.txt : scan-level tile tally
#     (these are merged scans, so the result is per-scan, not per-shot-frame).
#
# The Windows OCR engine is non-deterministic, so the eval runs each hideout
# fixture `-Runs` times; a read that varied across runs is shown as e.g.
# "2x4,UNREADx1" with a FLAKY status, so a committed PASS means "read correctly
# on every run", not "happened to read once".
#
# Usage:
#   ./scripts/ocr-eval.ps1 -Out C:\zt\ocr-eval\score.json   # produce the JSON
#   ./scripts/ocr-write-results.ps1 -Json C:\zt\ocr-eval\score.json
#
# Defaults to the same path ocr-eval.ps1 writes, so the two compose with no args.

[CmdletBinding()]
param(
    [string]$Json = 'C:\zt\ocr-eval\score.json'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Json)) {
    Write-Error "eval report not found: $Json  (run scripts/ocr-eval.ps1 first)"
    exit 1
}
$r = Get-Content -Raw -Path $Json | ConvertFrom-Json
$runs = $r.runs
$repo = Split-Path -Parent $PSScriptRoot
$shots = "$($r.hideout.fixtures.Count) fixtures, $runs run(s) each"

# Write lines as BOM-less UTF-8 (PS 5.1's `Set-Content -Encoding utf8` prepends a
# BOM, which clutters committed text files); LF newlines for a clean git diff.
$utf8NoBom = New-Object System.Text.UTF8Encoding $false
function Write-Lines($path, $lines) {
    [System.IO.File]::WriteAllText($path, (($lines -join "`n") + "`n"), $utf8NoBom)
}

# Collapse a cell's per-run reads into one column. Unanimous -> the value;
# otherwise "<value>x<count>,..." most-frequent first, which flags jitter.
function Format-Reads($reads) {
    $distinct = @($reads | Select-Object -Unique)
    if ($distinct.Count -le 1) { return [string]$distinct[0] }
    $g = $reads | Group-Object | Sort-Object Count -Descending
    return (($g | ForEach-Object { '{0}x{1}' -f $_.Name, $_.Count }) -join ',')
}

$written = 0
foreach ($f in $r.hideout.fixtures) {
    $lines = New-Object System.Collections.Generic.List[string]
    $idStatus = if ($f.identified) { 'PASS (resolved on every run)' }
    else { "FAIL (run 1 resolved as '$($f.identified_as)')" }

    # Per-fixture wrong-read count: a labelled cell that read a number != label
    # on any run (UNREAD never counts — it preserves the existing value).
    $wrong = 0
    foreach ($c in $f.cells) {
        $labelled = ($null -ne $c.label_owned) -and ($c.label_needed -eq $c.needed)
        if (-not $labelled) { continue }
        foreach ($rd in $c.reads) {
            if ($rd -ne 'UNREAD' -and [string]$rd -ne [string]$c.label_owned) { $wrong++; break }
        }
    }

    $lines.Add("# OCR result - $($f.stem)   (auto-generated; do not edit by hand)")
    $lines.Add('# Source: full OCR pipeline over the sibling .webp. Regenerate with')
    $lines.Add('#   scripts/ocr-eval.ps1  then  scripts/ocr-write-results.ps1  (see screenshots/CLAUDE.md).')
    $lines.Add('# The Windows OCR engine is non-deterministic: a read like "2x4,UNREADx1" varied')
    $lines.Add('# across runs (FLAKY); a PASS means the cell read correctly on every run.')
    $lines.Add('#')
    $lines.Add("identification: $idStatus")
    $lines.Add("owned-count:    $($f.correct_median)/$($f.labelled) cells correct  (band $($f.correct_min)-$($f.correct_max) over $runs runs; wrong reads: $wrong)")
    $lines.Add('#')
    $lines.Add('# status  cell  item_id                         read              label (owned/needed)')
    foreach ($c in ($f.cells | Sort-Object index)) {
        $labelled = ($null -ne $c.label_owned) -and ($c.label_needed -eq $c.needed)
        $status =
        if (-not $labelled) { 'n/a  ' }
        elseif ($c.correct_runs -eq $runs) { 'PASS ' }
        elseif ($c.correct_runs -eq 0) { 'FAIL ' }
        else { 'FLAKY' }
        $readStr = Format-Reads $c.reads
        $label = if ($null -ne $c.label_owned) { "$($c.label_owned)/$($c.label_needed)" } else { '-' }
        $lines.Add(('  {0}  [{1}]  {2,-30}  {3,-16}  {4}' -f $status, $c.index, $c.item_id, $readStr, $label))
    }

    $out = Join-Path $repo "screenshots/hideout/$($f.stem).ocr-result.txt"
    Write-Lines $out $lines
    $written++
}

# Scan assets: one result per merged scan (not per shot frame).
function Write-ScanResult($asset, $scanObj) {
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("# OCR result - $asset scan   (auto-generated; do not edit by hand)")
    $lines.Add('# Source: read_tiles + merge_capture over the frozen .boxes.json shots.')
    $lines.Add('#   Deterministic (no live engine) -> no run-to-run jitter.')
    $lines.Add('# Regenerate with scripts/ocr-eval.ps1 then scripts/ocr-write-results.ps1.')
    $lines.Add('#')
    $exact = if ($scanObj.all_exact) { 'PASS (matches label exactly)' } else { 'FAIL (does not match label exactly)' }
    $lines.Add("tiles:       $($scanObj.tiles_correct)/$($scanObj.tiles_total) correct")
    $lines.Add("exact-match: $exact")
    if ($scanObj.note) { $lines.Add("note:        $($scanObj.note)") }
    $out = Join-Path $repo "screenshots/$asset/$asset.ocr-result.txt"
    Write-Lines $out $lines
}
Write-ScanResult 'box' $r.'box'
Write-ScanResult 'stash' $r.stash

Write-Output "wrote $written hideout result sidecars + box/stash ($shots) under screenshots/"
