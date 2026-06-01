#Requires -Version 5.1
<#
.SYNOPSIS
    AIS Runner - Windows dependency installer.

.DESCRIPTION
    Installs the tools that AIS Runner needs: Node.js, Azure CLI, Azure Functions
    Core Tools, and Azurite.  Already-installed tools are skipped.

    Strategy (per tool):
      1. winget  — fast, built into Windows 10 21H2+ and all of Windows 11
      2. MSI fallback — direct download if winget is unavailable or fails

    Docker Desktop is detected but not installed automatically (too large,
    requires a reboot, and needs interactive licence acceptance).

    Requires administrator privileges and an internet connection.
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

# MSI fallback URLs (used only when winget is unavailable or fails)
$NODE_MSI  = "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-x64.msi"
$AZ_MSI    = "https://azcliprod.blob.core.windows.net/msi/azure-cli-${AZ_CLI_VERSION}-x64.msi"
$FUNC_MSI  = "https://github.com/Azure/azure-functions-core-tools/releases/download/${FUNC_VERSION}/func-cli-${FUNC_VERSION}-x64.msi"

$TMP = $env:TEMP

# ── Helpers ───────────────────────────────────────────────────────────────────
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

# Returns $true if winget is available and functional
function Test-Winget {
    try {
        $null = winget --version 2>&1
        return $LASTEXITCODE -eq 0
    } catch { }
    return $false
}

# Install via winget.  Returns $true on success.
function Install-Winget {
    param([string]$PackageId, [string]$Label)
    Write-Host "  winget install $PackageId ..."
    try {
        winget install --id $PackageId `
            --silent `
            --accept-package-agreements `
            --accept-source-agreements `
            --disable-interactivity 2>&1 | Tee-Object -Variable out | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -eq 0 -or $LASTEXITCODE -eq -1978335189) {
            # 0 = installed, -1978335189 (0x8A150011) = already installed
            Refresh-Path
            return $true
        }
        Write-Warn "winget exited $LASTEXITCODE — trying MSI fallback"
        return $false
    } catch {
        Write-Warn "winget failed: $_ — trying MSI fallback"
        return $false
    }
}

# Install via direct MSI download.
function Install-Msi {
    param([string]$Url, [string]$Label)
    $file = Join-Path $TMP ([System.IO.Path]::GetFileName($Url))
    Write-Host "  Downloading $Label ..."
    try {
        $wc = New-Object System.Net.WebClient
        $wc.DownloadFile($Url, $file)
        $sizeMB = [math]::Round((Get-Item $file).Length / 1MB, 1)
        Write-Host "  Downloaded ${sizeMB} MB — installing silently ..."
        $proc = Start-Process msiexec -ArgumentList "/i `"$file`" /qn /norestart" -Wait -PassThru -NoNewWindow
        if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
            throw "msiexec exited $($proc.ExitCode)"
        }
        Refresh-Path
    } finally {
        if (Test-Path $file) { Remove-Item $file -Force -ErrorAction SilentlyContinue }
    }
}

function Install-NpmPackage {
    param([string]$Package, [string]$Label)
    Write-Host "  npm install -g $Package ..."
    try {
        $result = cmd /c "npm install -g $Package --no-fund --no-audit 2>&1"
        $result | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "npm exited $LASTEXITCODE" }
        Refresh-Path
    } catch {
        Write-Fail "$Label install failed: $_"
        Write-Warn "Retry manually: npm install -g $Package"
    }
}

# ── Elevation check ───────────────────────────────────────────────────────────
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting administrator privileges..." -ForegroundColor Yellow
    $elevArgs = "-ExecutionPolicy Bypass -NoProfile -File `"$PSCommandPath`" -NoPrompt"
    Start-Process powershell $elevArgs -Verb RunAs -Wait
    exit
}

# ── Banner ────────────────────────────────────────────────────────────────────
$hasWinget = Test-Winget
Write-Host ""
Write-Host "  AIS Runner - Windows Setup" -ForegroundColor White
Write-Host "  ==========================" -ForegroundColor DarkGray
Write-Host ""
Write-Host "  Node.js    v$NODE_VERSION"
Write-Host "  Azure CLI  $AZ_CLI_VERSION"
Write-Host "  Azurite    $AZURITE_VERSION  (npm)"
Write-Host "  func       $FUNC_VERSION"
Write-Host "  Docker     Desktop (Service Bus emulator)"
Write-Host ""
if ($hasWinget) {
    Write-Host "  Install method: winget (with MSI fallback)" -ForegroundColor DarkGray
} else {
    Write-Host "  Install method: MSI download (winget not available)" -ForegroundColor DarkGray
}
Write-Host ""

# ── 1. Node.js ────────────────────────────────────────────────────────────────
Write-Step "Node.js v$NODE_VERSION"
Refresh-Path
$nodeVer   = Get-InstalledVersion 'node'
$nodeMajor = if ($nodeVer) { [int]($nodeVer.Split('.')[0]) } else { 0 }
if ($nodeVer -and $nodeMajor -ge 20) {
    Write-Skip "node $nodeVer"
} else {
    if ($nodeVer) { Write-Warn "Found node $nodeVer (< v20) — replacing" }
    $ok = $false
    if ($hasWinget) { $ok = Install-Winget 'OpenJS.NodeJS.LTS' "Node.js LTS" }
    if (-not $ok)   { Install-Msi $NODE_MSI "Node.js v$NODE_VERSION" }
    Refresh-Path
    $nodeVer = Get-InstalledVersion 'node'
    if ($nodeVer) { Write-Ok "node $nodeVer" } else { Write-Fail "node not found after install" }
}

# ── 2. Azure CLI ──────────────────────────────────────────────────────────────
Write-Step "Azure CLI"
Refresh-Path
$azVer = Get-AzCliVersion
if ($azVer) {
    Write-Skip "az $azVer"
} else {
    $ok = $false
    if ($hasWinget) { $ok = Install-Winget 'Microsoft.AzureCLI' "Azure CLI" }
    if (-not $ok)   { Install-Msi $AZ_MSI "Azure CLI $AZ_CLI_VERSION" }
    Refresh-Path
    $azVer = Get-AzCliVersion
    if ($azVer) { Write-Ok "az $azVer" } else { Write-Fail "az not found after install" }
}

# ── 3. Azurite ────────────────────────────────────────────────────────────────
Write-Step "Azurite $AZURITE_VERSION"
Refresh-Path
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    Write-Fail "npm not found — Node.js install may have failed"
} else {
    $azuriteVer = Get-InstalledVersion 'azurite'
    if ($azuriteVer) {
        Write-Skip "azurite $azuriteVer"
    } else {
        Install-NpmPackage "azurite@$AZURITE_VERSION" "Azurite"
        Refresh-Path
        $azuriteVer = Get-InstalledVersion 'azurite'
        if ($azuriteVer) { Write-Ok "azurite $azuriteVer" } else { Write-Fail "azurite not found after install" }
    }
}

# ── 4. Azure Functions Core Tools ─────────────────────────────────────────────
Write-Step "Azure Functions Core Tools"
Refresh-Path
$funcVer = Get-InstalledVersion 'func'
if ($funcVer -and $funcVer.StartsWith('4.')) {
    Write-Skip "func $funcVer"
} else {
    if ($funcVer) { Write-Warn "Found func $funcVer — replacing with v4" }
    $ok = $false
    if ($hasWinget) { $ok = Install-Winget 'Microsoft.Azure.FunctionsCoreTools' "func v4" }
    if (-not $ok)   { Install-Msi $FUNC_MSI "func $FUNC_VERSION" }
    Refresh-Path
    $funcVer = Get-InstalledVersion 'func'
    if ($funcVer) { Write-Ok "func $funcVer" } else { Write-Fail "func not found after install" }
}

# ── 5. Docker Desktop ─────────────────────────────────────────────────────────
Write-Step "Docker Desktop"
$dockerRunning   = $false
$dockerInstalled = $false

if (Get-Command docker -ErrorAction SilentlyContinue) {
    $dockerInstalled = $true
    try {
        $null = docker info 2>&1
        if ($LASTEXITCODE -eq 0) { $dockerRunning = $true }
    } catch { }
}
if (-not $dockerInstalled) {
    @("$env:ProgramFiles\Docker\Docker\Docker Desktop.exe",
      "$env:LocalAppData\Docker\Docker Desktop.exe") | ForEach-Object {
        if (Test-Path $_) { $dockerInstalled = $true }
    }
}

if ($dockerRunning) {
    Write-Skip "Docker $(Get-InstalledVersion 'docker' @('--version')) (running)"
} elseif ($dockerInstalled) {
    Write-Warn "Docker Desktop installed but not running — start it from the Start Menu before using the Service Bus emulator."
} else {
    Write-Warn "Docker Desktop not installed."
    Write-Host "  Required for the Service Bus emulator.  Install manually:" -ForegroundColor Yellow
    Write-Host "  https://www.docker.com/products/docker-desktop/" -ForegroundColor Cyan
    if (-not $NoPrompt) {
        $ans = Read-Host "  Open download page now? (Y/N)"
        if ($ans -match '^[Yy]') { Start-Process "https://www.docker.com/products/docker-desktop/" }
    }
}

# ── Summary ───────────────────────────────────────────────────────────────────
Refresh-Path
Write-Host ""
Write-Host "  ==========================" -ForegroundColor DarkGray
Write-Host ""

$dockerVer = if ($dockerRunning)   { Get-InstalledVersion 'docker' @('--version') }
             elseif ($dockerInstalled) { 'installed (not running)' }
             else { $null }

$results = @(
    @{ Name = 'node';    Ver = (Get-InstalledVersion 'node') },
    @{ Name = 'az';      Ver = (Get-AzCliVersion) },
    @{ Name = 'azurite'; Ver = (Get-InstalledVersion 'azurite') },
    @{ Name = 'func';    Ver = (Get-InstalledVersion 'func') },
    @{ Name = 'docker';  Ver = $dockerVer }
)

foreach ($r in $results) {
    if ($r.Ver) {
        Write-Host "  OK  $($r.Name.PadRight(10)) $($r.Ver)" -ForegroundColor Green
    } else {
        Write-Host "  XX  $($r.Name.PadRight(10)) not found" -ForegroundColor Red
    }
}

Write-Host ""
$coreOk = -not (($results | Where-Object { $_.Name -ne 'docker' } | Where-Object { -not $_.Ver }) )
if ($coreOk) {
    Write-Host "  Core dependencies ready." -ForegroundColor Green
    if (-not $dockerRunning) {
        Write-Host "  Docker missing or not running — Service Bus emulator unavailable." -ForegroundColor Yellow
    }
} else {
    Write-Host "  Some tools missing. If they were just installed, open a new terminal" -ForegroundColor Yellow
    Write-Host "  and run: node --version / az --version / azurite --version / func --version" -ForegroundColor Yellow
}
Write-Host ""

if (-not $NoPrompt) { Read-Host "Press Enter to close" }
