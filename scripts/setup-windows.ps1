#Requires -Version 5.1
<#
.SYNOPSIS
    AIS Runner — Windows dependency installer.

.DESCRIPTION
    Downloads and installs the exact tool versions that AIS Runner was tested
    against.  Run once before launching ais-runner.exe.

    Tools installed:
        Node.js          v20.18.1  (LTS — required by func and azurite)
        Azure CLI        2.65.0
        Azurite          3.30.0    (via npm)
        func core tools  4.x       (via npm)

    Already-installed tools at the correct version are skipped.
    Requires an internet connection.  Elevation (admin) is requested if needed.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Pinned versions ────────────────────────────────────────────────────────────
$NODE_VERSION   = '20.18.1'
$AZ_CLI_VERSION = '2.65.0'
$AZURITE_VERSION = '3.30.0'
$FUNC_MAJOR      = '4'   # latest stable 4.x from npm

$NODE_URL  = "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-x64.msi"
$AZ_URL    = "https://azcliprod.blob.core.windows.net/msi/azure-cli-${AZ_CLI_VERSION}-x64.msi"

$TMP = $env:TEMP

# ── Helpers ────────────────────────────────────────────────────────────────────
function Write-Step  { param([string]$msg) Write-Host "`n▸ $msg" -ForegroundColor Cyan }
function Write-Ok    { param([string]$msg) Write-Host "  ✓ $msg" -ForegroundColor Green }
function Write-Skip  { param([string]$msg) Write-Host "  — $msg (already installed)" -ForegroundColor DarkGray }
function Write-Warn  { param([string]$msg) Write-Host "  ⚠ $msg" -ForegroundColor Yellow }

function Invoke-Download {
    param([string]$Url, [string]$Dest)
    Write-Host "  Downloading $(Split-Path $Url -Leaf) …" -NoNewline
    $wc = New-Object System.Net.WebClient
    $wc.DownloadFile($Url, $Dest)
    Write-Host " done ($([math]::Round((Get-Item $Dest).Length/1MB,1)) MB)"
}

function Get-CommandVersion {
    param([string]$Cmd, [string]$Args = '--version')
    try {
        $out = & $Cmd $Args 2>&1 | Select-Object -First 1
        return ($out -replace '[^0-9\.]','').Trim()
    } catch { return $null }
}

function Refresh-Path {
    # Reload PATH from registry so newly installed tools are visible in this session
    $machine = [System.Environment]::GetEnvironmentVariable('PATH','Machine')
    $user    = [System.Environment]::GetEnvironmentVariable('PATH','User')
    $env:PATH = "$machine;$user"
}

# ── Elevation check ───────────────────────────────────────────────────────────
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
             [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting administrator privileges…" -ForegroundColor Yellow
    Start-Process powershell "-ExecutionPolicy Bypass -File `"$PSCommandPath`"" -Verb RunAs
    exit
}

Write-Host @"

  ╔══════════════════════════════════════════╗
  ║   AIS Runner — Windows Setup             ║
  ╚══════════════════════════════════════════╝

  Node.js       v$NODE_VERSION
  Azure CLI     $AZ_CLI_VERSION
  Azurite       $AZURITE_VERSION
  func (npm)    latest $FUNC_MAJOR.x

"@ -ForegroundColor White

# ── 1. Node.js ────────────────────────────────────────────────────────────────
Write-Step "Node.js v$NODE_VERSION"
$nodeVer = Get-CommandVersion 'node'
if ($nodeVer -and $nodeVer.StartsWith('20.')) {
    Write-Skip "node $nodeVer"
} else {
    if ($nodeVer) { Write-Warn "Found node $nodeVer — replacing with v$NODE_VERSION" }
    $msi = Join-Path $TMP "node-v${NODE_VERSION}-x64.msi"
    Invoke-Download $NODE_URL $msi
    Write-Host "  Installing Node.js (silent) …"
    Start-Process msiexec -ArgumentList "/i `"$msi`" /qn /norestart ADDLOCAL=ALL" -Wait -NoNewWindow
    Remove-Item $msi -Force
    Refresh-Path
    $nodeVer = Get-CommandVersion 'node'
    Write-Ok "node $nodeVer"
}

# ── 2. Azure CLI ──────────────────────────────────────────────────────────────
Write-Step "Azure CLI $AZ_CLI_VERSION"
$azVer = Get-CommandVersion 'az' '--version'
# az --version returns multi-line; first line contains "azure-cli  X.Y.Z"
$azVer = try { (az --version 2>&1 | Select-String 'azure-cli\s+([\d\.]+)').Matches[0].Groups[1].Value } catch { $null }
if ($azVer -eq $AZ_CLI_VERSION) {
    Write-Skip "az $azVer"
} else {
    if ($azVer) { Write-Warn "Found az $azVer — replacing with $AZ_CLI_VERSION" }
    $msi = Join-Path $TMP "azure-cli-${AZ_CLI_VERSION}-x64.msi"
    Invoke-Download $AZ_URL $msi
    Write-Host "  Installing Azure CLI (silent) …"
    Start-Process msiexec -ArgumentList "/i `"$msi`" /qn /norestart" -Wait -NoNewWindow
    Remove-Item $msi -Force
    Refresh-Path
    $azVer = try { (az --version 2>&1 | Select-String 'azure-cli\s+([\d\.]+)').Matches[0].Groups[1].Value } catch { '?' }
    Write-Ok "az $azVer"
}

# ── 3. Azurite (npm) ──────────────────────────────────────────────────────────
Write-Step "Azurite $AZURITE_VERSION"
$azuriteVer = Get-CommandVersion 'azurite' '--version'
if ($azuriteVer -eq $AZURITE_VERSION) {
    Write-Skip "azurite $azuriteVer"
} else {
    if ($azuriteVer) { Write-Warn "Found azurite $azuriteVer — replacing with $AZURITE_VERSION" }
    Write-Host "  npm install -g azurite@$AZURITE_VERSION …"
    npm install -g "azurite@$AZURITE_VERSION" --no-fund --no-audit 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" }
    Refresh-Path
    $azuriteVer = Get-CommandVersion 'azurite' '--version'
    Write-Ok "azurite $azuriteVer"
}

# ── 4. Azure Functions Core Tools ─────────────────────────────────────────────
Write-Step "Azure Functions Core Tools $FUNC_MAJOR.x"
$funcVer = Get-CommandVersion 'func'
if ($funcVer -and $funcVer.StartsWith("$FUNC_MAJOR.")) {
    Write-Skip "func $funcVer"
} else {
    if ($funcVer) { Write-Warn "Found func $funcVer — replacing with latest $FUNC_MAJOR.x" }
    Write-Host "  npm install -g azure-functions-core-tools@$FUNC_MAJOR …"
    npm install -g "azure-functions-core-tools@$FUNC_MAJOR" --unsafe-perm true --no-fund --no-audit 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" }
    Refresh-Path
    $funcVer = Get-CommandVersion 'func'
    Write-Ok "func $funcVer"
}

# ── Summary ───────────────────────────────────────────────────────────────────
Write-Host "`n" -NoNewline
Write-Host "  ══════════════════════════════════════════" -ForegroundColor DarkGray
Write-Host "  All dependencies installed." -ForegroundColor Green
Write-Host ""
Write-Host "  node    $(node --version 2>$null)"
Write-Host "  az      $(az version --query '\"azure-cli\"' -o tsv 2>$null)"
Write-Host "  azurite $(azurite --version 2>$null)"
Write-Host "  func    $(func --version 2>$null)"
Write-Host ""
Write-Host "  You can now launch ais-runner.exe" -ForegroundColor White
Write-Host "  ══════════════════════════════════════════" -ForegroundColor DarkGray
Write-Host ""

Read-Host "Press Enter to close"
