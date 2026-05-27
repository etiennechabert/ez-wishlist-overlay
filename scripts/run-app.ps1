# Run the desktop app with the toolchain wiring our workspace needs.
#
# Why this exists: `openvr_sys` (a transitive Windows-only dependency)
# hard-codes the MSVC-only `/DWIN32` cxxflag in its CMake build, so a
# plain `cargo app-dev` against the default gnullvm toolchain fails at
# CMake configure time. The fix is to target `x86_64-pc-windows-msvc`
# instead, which needs:
#
#   - `link.exe` + LIB / INCLUDE paths from a VS Build Tools install
#     (this script enters the VS Dev Shell to populate them)
#   - `libclang` for openvr_sys' bindgen (sets `LIBCLANG_PATH`)
#
# The cargo aliases `app` and `app-dev` already pin the MSVC target;
# this script just makes the env those aliases need available in the
# current shell. See memory/toolchain.md for the full background.
#
# Usage:
#   ./scripts/run-app.ps1            # debug build (runs `cargo app-dev`)
#   ./scripts/run-app.ps1 -Release   # release build (runs `cargo app`)
#
# Any extra args after the flags are forwarded to cargo, e.g.
#   ./scripts/run-app.ps1 -- --quiet

[CmdletBinding()]
param(
    [switch]$Release,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Forward
)

$ErrorActionPreference = 'Stop'

$vsInstall = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools'
$devShellDll = Join-Path $vsInstall 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll'
$libclang = 'C:\Program Files\LLVM\bin'

if (-not (Test-Path $devShellDll)) {
    Write-Error ("VS Dev Shell module not found at $devShellDll. " +
        "Install VS Build Tools 2022 (see memory/toolchain.md) or edit " +
        "this script if your install path differs.")
    exit 1
}
if (-not (Test-Path $libclang)) {
    Write-Error ("LLVM not found at $libclang (openvr_sys bindgen needs " +
        "libclang.dll). Install via `winget install LLVM.LLVM` or edit " +
        "this script if your install path differs.")
    exit 1
}

# Prepend the VS Installer dir so Enter-VsDevShell can find `vswhere.exe`,
# and merge the user + machine PATH so cargo/rustc are reachable even from
# a minimal shell (Claude Code's runner trims PATH).
$env:Path = "C:\Program Files (x86)\Microsoft Visual Studio\Installer;" +
    [System.Environment]::GetEnvironmentVariable('Path', 'User') + ';' +
    [System.Environment]::GetEnvironmentVariable('Path', 'Machine')
$env:LIBCLANG_PATH = $libclang

Import-Module $devShellDll
Enter-VsDevShell `
    -VsInstallPath $vsInstall `
    -SkipAutomaticLocation `
    -DevCmdArguments '-arch=amd64 -host_arch=amd64' | Out-Null

$alias = if ($Release) { 'app' } else { 'app-dev' }
# Cargo writes its progress to stderr. With $ErrorActionPreference = 'Stop'
# above, PS 5.1 wraps each native stderr line as a terminating ErrorRecord
# when this script is piped/redirected from outside — which kills the run
# before cargo finishes. Drop the strict policy just for the cargo call.
$ErrorActionPreference = 'Continue'
& cargo $alias @Forward
exit $LASTEXITCODE
