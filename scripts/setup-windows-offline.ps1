#Requires -Version 5.1
<#
.SYNOPSIS
    AIS Runner - Windows dependency installer (offline / air-gapped).

.DESCRIPTION
    Same as setup-windows.ps1 but reads installers from a local folder
    instead of downloading them.

    BEFORE RUNNING:
        Place the following files in the same folder as this script
        (or set $LocalAssets below to the folder where they live):

            node-v20.18.1-x64.msi
            azure-cli-2.65.0-x64.msi
            func-cli-4.0.6610-x64.msi
            azurite-3.30.0.tgz          <- produced by: npm pack azurite@3.30.0

    How to produce those files on a machine that HAS internet:
        curl -LO https://nodejs.org/dist/v20.18.1/node-v20.18.1-x64.msi
        curl -LO https://azcliprod.blob.core.windows.net/msi/azure-cli-2.65.0-x64.msi
        curl -LO https://github.com/Azure/azure-functions-core-tools/releases/download/4.0.6610/func-cli-4.0.6610-x64.msi
        npm pack azurite@3.30.0
    Then copy all four files to the Windows VM (e.g. via UTM shared folder).
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Pinned versions ────────────────────────────────────────────────────────────
$NODE_VERSION    = '20.18.1'
$AZ_CLI_VERSION  = '2.65.0'
$AZURITE_VERSION = '3.30.0'
$FUNC_VERSION    = '4.0.6610'

# ── Local assets folder ───────────────────────────────────────────────────────
# Default: same directory as this script.  Override here if needed.
$LocalAssets = $PSScriptRoot

$NODE_MSI    = Join-Path $LocalAssets "node-v${NODE_VERSION}-x64.msi"
$AZ_MSI      = Join-Path $LocalAssets "azure-cli-${AZ_CLI_VERSION}-x64.msi"
$FUNC_MSI    = Join-Path $LocalAssets "func-cli-${FUNC_VERSION}-x64.msi"
$AZURITE_TGZ = Join-Path $LocalAssets "azurite-${AZURITE_VERSION}.tgz"

# ── Helpers ────────────────────────────────────────────────────────────────────
function Write-Step { param([string]$msg) Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok   { param([string]$msg) Write-Host "  OK  $msg" -ForegroundColor Green }
function Write-Skip { param([string]$msg) Write-Host "  --  $msg (already installed)" -ForegroundColor DarkGray }
function Write-Warn { param([string]$msg) Write-Host "  !!  $msg" -ForegroundColor Yellow }
function Write-Fail { param([string]$msg) Write-Host "  XX  $msg" -ForegroundColor Red }

function Refresh-Path {
    $machine = [System.Environment]::GetEnvironmentVariable('PATH', 'Machine')
    $user    = [System.Environment]::GetEnvironmentVariable('PATH', 'User')
    $env:PATH = "$machine;$user"
}

function Get-InstalledVersion {
    param([string]$Cmd, [string[]]$Arguments = @('--version'))
    try {
        $out = & $Cmd @Arguments 2>&1 | Out-String
        if ($out -match '(\d+\.\d+[\.\d]*)') { return $Matches[1] }
    } catch { }
    return $null
}

function Get-AzCliVersion {
    try {
        $out = az --version 2>&1 | Out-String
        if ($out -match 'azure-cli\s+(\d+\.\d+\.\d+)') { return $Matches[1] }
    } catch { }
    return $null
}

function Install-LocalMsi {
    param([string]$Path, [string]$Label)
    if (-not (Test-Path $Path)) {
        throw "Asset not found: $Path`n  Copy the file there and re-run this script."
    }
    $sizeMB = [math]::Round((Get-Item $Path).Length / 1MB, 1)
    Write-Host "  Installing $Label ($sizeMB MB) ..."
    $proc = Start-Process msiexec -ArgumentList "/i `"$Path`" /qn /norestart" -Wait -PassThru -NoNewWindow
    if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
        throw "msiexec exited with code $($proc.ExitCode)"
    }
    Refresh-Path
}

function Install-LocalNpmTgz {
    param([string]$TgzPath, [string]$Label)
    if (-not (Test-Path $TgzPath)) {
        throw "Asset not found: $TgzPath`n  Run 'npm pack azurite@$AZURITE_VERSION' on a machine with internet and copy the .tgz here."
    }
    Write-Host "  npm install -g $TgzPath ..."
    $result = cmd /c "npm install -g `"$TgzPath`" --no-fund --no-audit 2>&1"
    $result | Select-Object -Last 5 | ForEach-Object { Write-Host "    $_" }
    if ($LASTEXITCODE -ne 0) { throw "npm exited with code $LASTEXITCODE" }
    Refresh-Path
}

# ── Elevation check ───────────────────────────────────────────────────────────
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting administrator privileges..." -ForegroundColor Yellow
    Start-Process powershell "-ExecutionPolicy Bypass -File `"$PSCommandPath`"" -Verb RunAs
    exit
}

Write-Host ""
Write-Host "  AIS Runner - Windows Setup (offline)" -ForegroundColor White
Write-Host "  =====================================" -ForegroundColor DarkGray
Write-Host ""
Write-Host "  Reading assets from: $LocalAssets"
Write-Host ""
Write-Host "  Node.js    v$NODE_VERSION"
Write-Host "  Azure CLI  $AZ_CLI_VERSION"
Write-Host "  Azurite    $AZURITE_VERSION"
Write-Host "  func       $FUNC_VERSION"
Write-Host "  Docker     Desktop (manual - not handled here)"
Write-Host ""

# ── 1. Node.js ────────────────────────────────────────────────────────────────
Write-Step "Node.js v$NODE_VERSION"
$nodeVer = Get-InstalledVersion 'node'
if ($nodeVer -and $nodeVer.StartsWith('20.')) {
    Write-Skip "node $nodeVer"
} else {
    if ($nodeVer) { Write-Warn "Found node $nodeVer - will replace with v$NODE_VERSION" }
    Install-LocalMsi $NODE_MSI "Node.js v$NODE_VERSION"
    Refresh-Path
    $nodeVer = Get-InstalledVersion 'node'
    if ($nodeVer) { Write-Ok "node $nodeVer" } else { Write-Fail "node not found after install - open a new terminal" }
}

# ── 2. Azure CLI ──────────────────────────────────────────────────────────────
Write-Step "Azure CLI $AZ_CLI_VERSION"
$azVer = Get-AzCliVersion
if ($azVer -eq $AZ_CLI_VERSION) {
    Write-Skip "az $azVer"
} else {
    if ($azVer) { Write-Warn "Found az $azVer - will replace with $AZ_CLI_VERSION" }
    Install-LocalMsi $AZ_MSI "Azure CLI $AZ_CLI_VERSION"
    Refresh-Path
    $azVer = Get-AzCliVersion
    if ($azVer) { Write-Ok "az $azVer" } else { Write-Fail "az not found after install" }
}

# ── 3. Azurite ────────────────────────────────────────────────────────────────
Write-Step "Azurite $AZURITE_VERSION"
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    Write-Fail "npm not found - Node.js install may have failed or requires a new terminal"
} else {
    $azuriteVer = Get-InstalledVersion 'azurite'
    if ($azuriteVer -eq $AZURITE_VERSION) {
        Write-Skip "azurite $azuriteVer"
    } else {
        if ($azuriteVer) { Write-Warn "Found azurite $azuriteVer - will replace" }
        Install-LocalNpmTgz $AZURITE_TGZ "Azurite"
        $azuriteVer = Get-InstalledVersion 'azurite'
        if ($azuriteVer) { Write-Ok "azurite $azuriteVer" } else { Write-Fail "azurite not found after install" }
    }
}

# ── 4. Azure Functions Core Tools ─────────────────────────────────────────────
Write-Step "Azure Functions Core Tools $FUNC_VERSION"
$funcVer = Get-InstalledVersion 'func'
if ($funcVer -and $funcVer.StartsWith('4.')) {
    Write-Skip "func $funcVer"
} else {
    if ($funcVer) { Write-Warn "Found func $funcVer - will replace with $FUNC_VERSION" }
    Install-LocalMsi $FUNC_MSI "func $FUNC_VERSION"
    Refresh-Path
    $funcVer = Get-InstalledVersion 'func'
    if ($funcVer) { Write-Ok "func $funcVer" } else { Write-Fail "func not found after install - try opening a new terminal" }
}

# ── 5. Docker Desktop ─────────────────────────────────────────────────────────
Write-Step "Docker Desktop"
$dockerRunning   = $false
$dockerInstalled = $false

if (Get-Command docker -ErrorAction SilentlyContinue) {
    $dockerInstalled = $true
    try {
        docker info 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0) { $dockerRunning = $true }
    } catch { }
}
if (-not $dockerInstalled) {
    $dockerPaths = @(
        "$env:ProgramFiles\Docker\Docker\Docker Desktop.exe",
        "$env:LocalAppData\Docker\Docker Desktop.exe"
    )
    foreach ($p in $dockerPaths) { if (Test-Path $p) { $dockerInstalled = $true; break } }
}

if ($dockerRunning) {
    Write-Skip "Docker $(Get-InstalledVersion 'docker' @('--version')) (running)"
} elseif ($dockerInstalled) {
    Write-Warn "Docker Desktop installed but not running - start it from the Start Menu."
} else {
    Write-Warn "Docker Desktop not found."
    Write-Host "  Download the installer on a machine with internet, transfer here, and run it manually." -ForegroundColor Yellow
    Write-Warn "Continuing without Docker — Service Bus emulator will not be available."
}

# ── Summary ───────────────────────────────────────────────────────────────────
Refresh-Path
Write-Host ""
Write-Host "  =====================================" -ForegroundColor DarkGray
Write-Host ""

$dockerSummaryVer = if ($dockerRunning) { Get-InstalledVersion 'docker' @('--version') }
                   elseif ($dockerInstalled) { 'installed (not running)' }
                   else { $null }

$results = @(
    @{ Name = 'node';    Ver = (Get-InstalledVersion 'node') },
    @{ Name = 'az';      Ver = (Get-AzCliVersion) },
    @{ Name = 'azurite'; Ver = (Get-InstalledVersion 'azurite') },
    @{ Name = 'func';    Ver = (Get-InstalledVersion 'func') },
    @{ Name = 'docker';  Ver = $dockerSummaryVer }
)

foreach ($r in $results) {
    if ($r.Ver) {
        Write-Host "  OK  $($r.Name.PadRight(10)) $($r.Ver)" -ForegroundColor Green
    } else {
        Write-Host "  XX  $($r.Name.PadRight(10)) not found" -ForegroundColor Red
    }
}

Write-Host ""
$coreOk = -not (($results | Where-Object { $_.Name -ne 'docker' }) | Where-Object { -not $_.Ver })
if ($coreOk) {
    Write-Host "  Core dependencies ready. You can launch ais-runner.exe." -ForegroundColor Green
    if (-not $dockerRunning) {
        Write-Host "  Docker Desktop missing or not running — Service Bus emulator unavailable." -ForegroundColor Yellow
    }
} else {
    Write-Host "  Some tools were not found. Open a new terminal and verify manually." -ForegroundColor Yellow
}
Write-Host ""
Read-Host "Press Enter to close"
