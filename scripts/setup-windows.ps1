#Requires -Version 5.1
<#
.SYNOPSIS
    AIS Runner - Windows dependency installer.

.DESCRIPTION
    Downloads and installs the exact tool versions that AIS Runner was tested
    against.  Run once before launching ais-runner.exe.

    Tools installed:
        Node.js          v20.18.1  (LTS - required by azurite)
        Azure CLI        2.65.0
        Azurite          3.30.0    (via npm)
        func core tools  4.0.6610  (official MSI - more reliable than npm on Windows)
        Docker Desktop   latest    (required for the Service Bus emulator)

    Already-installed tools at the correct version are skipped.
    Requires an internet connection and administrator privileges.

    NOTE - Docker Desktop:
        The installer is large (~600 MB) and requires a reboot + manual launch.
        If Docker is already installed and running, it is skipped automatically.
        The Service Bus emulator (AzureCosmosDB) will not work without it.
#>
param(
    [switch]$NoPrompt   # Set by the graphical installer — skips all Read-Host pauses
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Pinned versions ────────────────────────────────────────────────────────────
$NODE_VERSION    = '20.18.1'
$AZ_CLI_VERSION  = '2.65.0'
$AZURITE_VERSION = '3.30.0'
$FUNC_VERSION    = '4.0.6610'

$NODE_URL    = "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-x64.msi"
$AZ_URL      = "https://azcliprod.blob.core.windows.net/msi/azure-cli-${AZ_CLI_VERSION}-x64.msi"
$FUNC_URL    = "https://github.com/Azure/azure-functions-core-tools/releases/download/${FUNC_VERSION}/func-cli-${FUNC_VERSION}-x64.msi"

$TMP = $env:TEMP

# ── Helpers ────────────────────────────────────────────────────────────────────
function Write-Step { param([string]$msg) Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok   { param([string]$msg) Write-Host "  OK  $msg" -ForegroundColor Green }
function Write-Skip { param([string]$msg) Write-Host "  --  $msg (already installed)" -ForegroundColor DarkGray }
function Write-Warn { param([string]$msg) Write-Host "  !!  $msg" -ForegroundColor Yellow }
function Write-Fail { param([string]$msg) Write-Host "  XX  $msg" -ForegroundColor Red }

function Refresh-Path {
    # Reload PATH from registry so newly installed tools are visible in this session.
    # Called after every MSI/npm install - critical on Windows where PATH changes
    # from installers are not inherited by the current process automatically.
    $machine = [System.Environment]::GetEnvironmentVariable('PATH', 'Machine')
    $user    = [System.Environment]::GetEnvironmentVariable('PATH', 'User')
    $env:PATH = "$machine;$user"
}

function Get-InstalledVersion {
    # Returns the version string for a command, or $null if not found/runnable.
    param([string]$Cmd, [string[]]$Arguments = @('--version'))
    try {
        $out = & $Cmd @Arguments 2>&1 | Out-String
        # Extract first x.y.z pattern found
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

function Install-Msi {
    param([string]$Url, [string]$Label)
    $file = Join-Path $TMP ([System.IO.Path]::GetFileName($Url))
    Write-Host "  Downloading $Label ..."
    try {
        $wc = New-Object System.Net.WebClient
        $wc.DownloadFile($Url, $file)
        $sizeMB = [math]::Round((Get-Item $file).Length / 1MB, 1)
        Write-Host "  Downloaded $sizeMB MB - installing (silent) ..."
        $proc = Start-Process msiexec -ArgumentList "/i `"$file`" /qn /norestart" -Wait -PassThru -NoNewWindow
        if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
            throw "msiexec exited with code $($proc.ExitCode)"
        }
        Refresh-Path
    } finally {
        if (Test-Path $file) { Remove-Item $file -Force }
    }
}

function Install-NpmPackage {
    param([string]$Package, [string]$Label)
    Write-Host "  npm install -g $Package ..."
    try {
        # Run npm via cmd to avoid PowerShell execution policy issues with npm.cmd
        $result = cmd /c "npm install -g $Package --no-fund --no-audit 2>&1"
        $result | Select-Object -Last 5 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "npm exited with code $LASTEXITCODE" }
        Refresh-Path
    } catch {
        Write-Fail "$Label install failed: $_"
        Write-Warn "You can retry manually: npm install -g $Package"
    }
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
Write-Host "  AIS Runner - Windows Setup" -ForegroundColor White
Write-Host "  ==========================" -ForegroundColor DarkGray
Write-Host ""
Write-Host "  Node.js    v$NODE_VERSION"
Write-Host "  Azure CLI  $AZ_CLI_VERSION"
Write-Host "  Azurite    $AZURITE_VERSION"
Write-Host "  func       $FUNC_VERSION"
Write-Host "  Docker     Desktop (for Service Bus emulator)"
Write-Host ""

# ── 1. Node.js ────────────────────────────────────────────────────────────────
Write-Step "Node.js v$NODE_VERSION"
$nodeVer   = Get-InstalledVersion 'node'
$nodeMajor = if ($nodeVer) { [int]($nodeVer.Split('.')[0]) } else { 0 }
if ($nodeVer -and $nodeMajor -ge 20) {
    Write-Skip "node $nodeVer"
} else {
    if ($nodeVer) { Write-Warn "Found node $nodeVer (< v20) - will replace with v$NODE_VERSION" }
    Install-Msi $NODE_URL "Node.js v$NODE_VERSION"
    # Verify
    Refresh-Path
    $nodeVer = Get-InstalledVersion 'node'
    if ($nodeVer) { Write-Ok "node $nodeVer" } else { Write-Fail "node not found after install - PATH may need a new terminal" }
}

# ── 2. Azure CLI ──────────────────────────────────────────────────────────────
Write-Step "Azure CLI $AZ_CLI_VERSION"
$azVer = Get-AzCliVersion
if ($azVer -eq $AZ_CLI_VERSION) {
    Write-Skip "az $azVer"
} else {
    if ($azVer) { Write-Warn "Found az $azVer - will replace with $AZ_CLI_VERSION" }
    Install-Msi $AZ_URL "Azure CLI $AZ_CLI_VERSION"
    Refresh-Path
    $azVer = Get-AzCliVersion
    if ($azVer) { Write-Ok "az $azVer" } else { Write-Fail "az not found after install" }
}

# ── 3. Azurite ────────────────────────────────────────────────────────────────
Write-Step "Azurite $AZURITE_VERSION"
# npm must be available - check explicitly after Node install
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    Write-Fail "npm not found - Node.js install may have failed or requires a new terminal"
} else {
    $azuriteVer = Get-InstalledVersion 'azurite'
    if ($azuriteVer -eq $AZURITE_VERSION) {
        Write-Skip "azurite $azuriteVer"
    } else {
        if ($azuriteVer) { Write-Warn "Found azurite $azuriteVer - will replace with $AZURITE_VERSION" }
        Install-NpmPackage "azurite@$AZURITE_VERSION" "Azurite"
        $azuriteVer = Get-InstalledVersion 'azurite'
        if ($azuriteVer) { Write-Ok "azurite $azuriteVer" } else { Write-Fail "azurite not found after install" }
    }
}

# ── 4. Azure Functions Core Tools (official MSI - more reliable than npm) ─────
Write-Step "Azure Functions Core Tools $FUNC_VERSION"
$funcVer = Get-InstalledVersion 'func'
if ($funcVer -and $funcVer.StartsWith('4.')) {
    Write-Skip "func $funcVer"
} else {
    if ($funcVer) { Write-Warn "Found func $funcVer - will replace with $FUNC_VERSION" }
    Install-Msi $FUNC_URL "func $FUNC_VERSION"
    Refresh-Path
    $funcVer = Get-InstalledVersion 'func'
    if ($funcVer) { Write-Ok "func $funcVer" } else { Write-Fail "func not found after install - try opening a new terminal" }
}

# ── 5. Docker Desktop ────────────────────────────────────────────────────────
# Required for the Service Bus emulator (docker compose up).
# We cannot silently install Docker Desktop - it requires a reboot and interactive
# acceptance of the license.  We detect it and guide the user if missing.
Write-Step "Docker Desktop"
$dockerRunning = $false
$dockerInstalled = $false

# Check if docker CLI is reachable
if (Get-Command docker -ErrorAction SilentlyContinue) {
    $dockerInstalled = $true
    # Check if the Docker daemon is actually running
    try {
        $info = docker info 2>&1 | Out-String
        if ($LASTEXITCODE -eq 0) { $dockerRunning = $true }
    } catch { }
}

# Also check common install locations even if not on PATH yet
if (-not $dockerInstalled) {
    $dockerPaths = @(
        "$env:ProgramFiles\Docker\Docker\Docker Desktop.exe",
        "$env:LocalAppData\Docker\Docker Desktop.exe"
    )
    foreach ($p in $dockerPaths) {
        if (Test-Path $p) { $dockerInstalled = $true; break }
    }
}

if ($dockerRunning) {
    $dockerVer = Get-InstalledVersion 'docker' @('--version')
    Write-Skip "Docker $dockerVer (running)"
} elseif ($dockerInstalled) {
    Write-Warn "Docker Desktop is installed but not running."
    Write-Warn "Launch Docker Desktop from the Start Menu and wait for it to start before using the Service Bus emulator."
} else {
    Write-Warn "Docker Desktop is NOT installed."
    Write-Host ""
    Write-Host "  Docker Desktop is required to run the Service Bus emulator." -ForegroundColor Yellow
    Write-Host "  It cannot be installed silently — please install it manually:" -ForegroundColor Yellow
    Write-Host "  https://www.docker.com/products/docker-desktop/" -ForegroundColor Cyan
    Write-Host ""
    if (-not $NoPrompt) {
        $openBrowser = Read-Host "  Open the Docker Desktop download page now? (Y/N)"
        if ($openBrowser -eq 'Y' -or $openBrowser -eq 'y') {
            Start-Process "https://www.docker.com/products/docker-desktop/"
            Write-Host "  After installing Docker Desktop, re-run this script to verify." -ForegroundColor DarkGray
        }
    }
    Write-Warn "Continuing without Docker — Service Bus emulator will not be available."
}

# ── Summary ───────────────────────────────────────────────────────────────────
Refresh-Path
Write-Host ""
Write-Host "  ==========================" -ForegroundColor DarkGray
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

$allOk = $true
foreach ($r in $results) {
    if ($r.Ver) {
        Write-Host "  OK  $($r.Name.PadRight(10)) $($r.Ver)" -ForegroundColor Green
    } else {
        Write-Host "  XX  $($r.Name.PadRight(10)) not found" -ForegroundColor Red
        $allOk = $false
    }
}

Write-Host ""
# Docker missing is a warning, not a hard failure — all other features still work.
$coreOk = $results | Where-Object { $_.Name -ne 'docker' } | ForEach-Object { $_.Ver -ne $null }
$allOk   = -not ($coreOk -contains $false)

if ($allOk) {
    Write-Host "  Core dependencies ready. You can launch ais-runner.exe." -ForegroundColor Green
    if (-not $dockerRunning) {
        Write-Host "  Docker Desktop missing or not running — Service Bus emulator unavailable." -ForegroundColor Yellow
    }
} else {
    Write-Host "  Some tools were not found. Open a new terminal and run:" -ForegroundColor Yellow
    Write-Host "  node --version / az --version / azurite --version / func --version" -ForegroundColor Yellow
    Write-Host "  If they work there, PATH was updated but requires a new session." -ForegroundColor DarkGray
}
Write-Host ""

if (-not $NoPrompt) { Read-Host "Press Enter to close" }
