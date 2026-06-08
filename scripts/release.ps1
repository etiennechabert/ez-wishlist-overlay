# Cut a release: derive the version from crates/app/Cargo.toml, run the same
# invariants CI enforces, then create and push an annotated `v<version>` tag
# (which triggers .github/workflows/release.yml to build + publish the MSI).
#
# Why this exists: the version lives in exactly ONE place,
# crates/app/Cargo.toml, and every PR bumps it ahead of the last release
# (ci.yml's version-bump gate). The release tag must equal that version.
# Hand-typing the tag (`git tag v0.4.4`) drifts behind Cargo.toml: it bit
# v0.4.2 and v0.4.4, which tagged a number below Cargo.toml and tripped
# release.yml's match guard with a dangling tag + a red Actions run. Deriving
# the tag here makes that mistake impossible: you never type the number.
#
# Pairs with .githooks/pre-push, which rejects a mismatched tag even if you
# bypass this script. Enable that once with:
#   git config core.hooksPath .githooks
#
# Usage:
#   ./scripts/release.ps1            # tag v<Cargo.toml version> and push it
#   ./scripts/release.ps1 -DryRun    # run every check + report; touch nothing
#
# NOTE: keep this file ASCII-only. Windows PowerShell 5.1 reads a BOM-less
# .ps1 as the ANSI code page, so non-ASCII punctuation (em dashes, smart
# quotes) is mis-decoded and breaks parsing.

[CmdletBinding()]
param(
    [switch]$DryRun
)

# We rely on $LASTEXITCODE after each git call (not throw-on-error), so keep
# the policy permissive: native git writes progress to stderr, which PS 5.1
# would otherwise wrap into terminating errors. Every failure path below is
# an explicit Write-Error + exit.
$ErrorActionPreference = 'Continue'

# Clean one-line error to stderr (Write-Error adds positional + CategoryInfo
# noise that buries the actual message).
function Fail($msg) { $host.UI.WriteErrorLine("release.ps1: $msg"); exit 1 }

# --- single source of truth: the version in crates/app/Cargo.toml ---
$repoRoot = & git rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0 -or -not $repoRoot) { Fail 'Not inside a git repository.' }

$cargoToml = Join-Path $repoRoot 'crates/app/Cargo.toml'
if (-not (Test-Path $cargoToml)) { Fail "crates/app/Cargo.toml not found at $cargoToml" }

$m = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $m) { Fail 'Could not find a version line in crates/app/Cargo.toml' }
$version = $m.Matches[0].Groups[1].Value
$tag = "v$version"

# --- latest released tag (same `sort -V` semantics ci.yml uses) ---
$latest = & git tag --list 'v*' |
    ForEach-Object { $_ -replace '^v', '' } |
    Where-Object { $_ -match '^\d+(\.\d+){0,2}$' } |
    ForEach-Object { [version]$_ } |
    Sort-Object |
    Select-Object -Last 1

Write-Host "Cargo.toml version : $version"
Write-Host "Release tag        : $tag"
Write-Host ("Latest released tag: " + $(if ($latest) { "v$latest" } else { '<none>' }))

# --- pre-flight checks ---

# 1. Clean working tree: the tag must capture committed state only.
$dirty = & git status --porcelain
if ($dirty) { Fail "Working tree is dirty. Commit or stash before releasing:`n$dirty" }

# 2. Cut from main (matches release.yml's source of truth).
$branch = & git rev-parse --abbrev-ref HEAD
if ($branch -ne 'main') { Fail "On branch '$branch', not 'main'. Release tags are cut from main." }

# 3. Tag must not already exist, locally or on origin.
$null = & git rev-parse -q --verify "refs/tags/$tag"
if ($LASTEXITCODE -eq 0) { Fail "Tag $tag already exists locally. Bump crates/app/Cargo.toml first." }
$remoteTag = & git ls-remote --tags origin "refs/tags/$tag"
if ($LASTEXITCODE -ne 0) {
    Write-Warning "Could not reach origin to check for an existing $tag tag. Continuing (the push will still fail if it exists)."
} elseif ($remoteTag) {
    Fail "Tag $tag already exists on origin. Bump crates/app/Cargo.toml first."
}

# 4. Version must be strictly ahead of the latest released tag: the same
#    invariant ci.yml's version-bump job enforces, so tags never go backwards.
if ($latest -and [version]$version -le $latest) {
    Fail "Cargo.toml version $version is not ahead of latest tag v$latest. Bump it before releasing."
}

if ($DryRun) {
    Write-Host "`n[dry run] All checks passed. Would create annotated tag $tag and push it to origin." -ForegroundColor Yellow
    exit 0
}

# --- create + push the tag (this fires release.yml) ---
& git tag -a $tag -m "Release $tag"
if ($LASTEXITCODE -ne 0) { Fail "git tag $tag failed." }

& git push origin $tag
if ($LASTEXITCODE -ne 0) {
    & git tag -d $tag | Out-Null
    Fail "git push failed. Removed the local $tag tag so you can retry."
}

Write-Host "`nPushed $tag. release.yml is building the MSI now:" -ForegroundColor Green
Write-Host "  https://github.com/etiennechabert/ez-wishlist-overlay/actions/workflows/release.yml"
