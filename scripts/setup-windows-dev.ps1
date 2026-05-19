#Requires -Version 5.1
<#
.SYNOPSIS
    AIS Runner - Windows developer build setup.

.DESCRIPTION
    Prepares the build toolchain and compiles ais-runner from source.

    Steps performed:
        1. Verify Rust/Cargo is installed (guides you to rustup.rs if missing)
        2. Install MSYS2 via winget (if not already at C:\msys64)
        3. Install MinGW-w64 GCC + binutils via pacman
        4. Add C:\msys64\mingw64\bin to the system PATH permanently
        5. cargo build --release
        6. Copy WebView2Loader.dll next to ais-runner.exe

    Run from the root of the ais-runner repository.
    Requires an internet connection and administrator privileges.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$MSYS2_BIN  = 'C:\msys64\mingw64\bin'
$SCRIPT_DIR = Split-Path -Parent $MyInvocation.MyCommand.Path
$REPO_ROOT  = Split-Path -Parent $SCRIPT_DIR

function Write-Step { param([string]$msg) Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok   { param([string]$msg) Write-Host "  OK  $msg" -ForegroundColor Green }
function Write-Skip { param([string]$msg) Write-Host "  --  $msg (already done)" -ForegroundColor DarkGray }
function Write-Warn { param([string]$msg) Write-Host "  !!  $msg" -ForegroundColor Yellow }
function Write-Fail { param([string]$msg) Write-Host "  XX  $msg" -ForegroundColor Red }

# ── Elevation check ───────────────────────────────────────────────────────────
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting administrator privileges..." -ForegroundColor Yellow
    Start-Process powershell "-ExecutionPolicy Bypass -File `"$PSCommandPath`"" -Verb RunAs
    exit
}

Write-Host ""
Write-Host "  AIS Runner - Developer Build Setup" -ForegroundColor White
Write-Host "  ===================================" -ForegroundColor DarkGray
Write-Host ""

# ── 1. Rust / Cargo ───────────────────────────────────────────────────────────
Write-Step "Rust toolchain"
if (Get-Command cargo -ErrorAction SilentlyContinue) {
    $rustVer = (rustc --version 2>&1) -replace 'rustc ', ''
    Write-Skip "rustc $rustVer"
} else {
    Write-Fail "cargo not found."
    Write-Host ""
    Write-Host "  Install Rust from https://rustup.rs/ then re-run this script." -ForegroundColor Yellow
    $open = Read-Host "  Open rustup.rs in your browser now? (Y/N)"
    if ($open -eq 'Y' -or $open -eq 'y') { Start-Process "https://rustup.rs/" }
    exit 1
}

# ── 2. MSYS2 ─────────────────────────────────────────────────────────────────
Write-Step "MSYS2"
if (Test-Path 'C:\msys64\usr\bin\bash.exe') {
    Write-Skip "MSYS2 already at C:\msys64"
} else {
    Write-Host "  Installing MSYS2 via winget..."
    winget install -e --id MSYS2.MSYS2 --silent --accept-package-agreements --accept-source-agreements
    if (-not (Test-Path 'C:\msys64\usr\bin\bash.exe')) {
        Write-Fail "MSYS2 install failed - install manually from https://www.msys2.org/ then re-run"
        exit 1
    }
    Write-Ok "MSYS2 installed"
}

# ── 3. MinGW-w64 GCC + binutils ──────────────────────────────────────────────
Write-Step "MinGW-w64 GCC + binutils"
if (Test-Path "$MSYS2_BIN\dlltool.exe") {
    $gccVer = (& "$MSYS2_BIN\gcc.exe" --version 2>&1 | Select-Object -First 1) -replace '.*?(\d+\.\d+\.\d+).*', '$1'
    Write-Skip "gcc $gccVer"
} else {
    Write-Host "  Running pacman to install mingw-w64-x86_64-gcc and binutils..."
    & 'C:\msys64\usr\bin\bash.exe' -lc "pacman -S --noconfirm mingw-w64-x86_64-gcc mingw-w64-x86_64-binutils 2>&1"
    if (Test-Path "$MSYS2_BIN\dlltool.exe") {
        Write-Ok "MinGW-w64 installed"
    } else {
        Write-Fail "MinGW-w64 install failed"
        exit 1
    }
}

# ── 4. Add MinGW to system PATH ───────────────────────────────────────────────
Write-Step "System PATH"
$machinePath = [System.Environment]::GetEnvironmentVariable('PATH', 'Machine')
if ($machinePath -like "*$MSYS2_BIN*") {
    Write-Skip "$MSYS2_BIN already in system PATH"
} else {
    [System.Environment]::SetEnvironmentVariable('PATH', "$MSYS2_BIN;$machinePath", 'Machine')
    Write-Ok "Added $MSYS2_BIN to system PATH"
}
if ($env:PATH -notlike "*$MSYS2_BIN*") {
    $env:PATH = "$MSYS2_BIN;$env:PATH"
}

# ── 5. cargo build --release ──────────────────────────────────────────────────
Write-Step "cargo build --release"
Push-Location $REPO_ROOT
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    Write-Ok "Build complete"
} finally {
    Pop-Location
}

# ── 6. Copy WebView2Loader.dll ────────────────────────────────────────────────
Write-Step "WebView2Loader.dll"
$releaseDir = Join-Path $REPO_ROOT "target\release"
$dllDest    = Join-Path $releaseDir "WebView2Loader.dll"

if (Test-Path $dllDest) {
    Write-Skip "WebView2Loader.dll already in target\release\"
} else {
    $dllSrc = Get-ChildItem "$releaseDir\build\webview2-com-sys-*\out\x64\WebView2Loader.dll" `
                -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($dllSrc) {
        Copy-Item $dllSrc.FullName $dllDest -Force
        Write-Ok "Copied WebView2Loader.dll to target\release\"
    } else {
        Write-Fail "WebView2Loader.dll not found in build output - try rebuilding"
    }
}

# ── Summary ───────────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "  ===================================" -ForegroundColor DarkGray
Write-Host ""
$exePath = Join-Path $REPO_ROOT "target\release\ais-runner.exe"
if (Test-Path $exePath) {
    Write-Host "  Build ready: target\release\ais-runner.exe" -ForegroundColor Green
    Write-Host ""
    $launch = Read-Host "  Launch ais-runner.exe now? (Y/N)"
    if ($launch -eq 'Y' -or $launch -eq 'y') { Start-Process $exePath }
} else {
    Write-Fail "ais-runner.exe not found after build"
}
Write-Host ""
Read-Host "Press Enter to close"
