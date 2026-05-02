# Packaging script for Azurite (Windows PowerShell)
# Builds a standalone azurite.exe for the AIS Runner

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
$BinDir = Join-Path $ProjectRoot "bin"
$TempDir = Join-Path $ProjectRoot "tmp_azurite_build"

if (!(Test-Path $BinDir)) { New-Item -ItemType Directory -Path $BinDir }
if (!(Test-Path $TempDir)) { New-Item -ItemType Directory -Path $TempDir }

Write-Host "📦 Packaging Azurite into standalone binary (Windows)..." -ForegroundColor Cyan

# 1. Ensure pkg is available
if (!(Get-Command pkg -ErrorAction SilentlyContinue)) {
    Write-Host "⚠️ 'pkg' not found. Installing globally..." -ForegroundColor Yellow
    npm install -g pkg
}

# 2. Install Azurite locally
Set-Location $TempDir
if (!(Test-Path "node_modules/azurite")) {
    Write-Host "⬇️ Downloading Azurite..." -ForegroundColor Gray
    npm init -y | Out-Null
    npm install azurite@3.30.0 --no-save
}

# 3. Build Standalone
Write-Host "🚀 Building for node18-win-x64..." -ForegroundColor Green
$OutputPath = Join-Path $BinDir "azurite.exe"

pkg node_modules/azurite/dist/src/azurite.js `
    --targets node18-win-x64 `
    --output $OutputPath `
    --public

Write-Host "✅ Success! Standalone Azurite created at: $OutputPath" -ForegroundColor Green
Write-Host "💡 You can now build the main app with 'cargo build --release --features bundle-azurite'" -ForegroundColor Cyan
