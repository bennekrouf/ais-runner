# Fails if the given .exe imports the Visual C++ runtime DLLs, i.e. would need the
# VC++ Redistributable installed. .cargo/config.toml links the CRT statically; this
# catches that config being lost or a dependency pulling the dynamic CRT back in.
#
# Usage: pwsh scripts/check-windows-crt.ps1 target\release\ais-runner.exe
param([Parameter(Mandatory)][string]$Exe)

$ErrorActionPreference = "Stop"

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$dumpbin = & $vswhere -latest -products * `
  -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  -find "VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" | Select-Object -First 1
if (-not $dumpbin) { Write-Error "dumpbin.exe not found" }

$deps = & $dumpbin /nologo /dependents $Exe
if ($LASTEXITCODE -ne 0) { Write-Error "dumpbin failed on $Exe" }
$deps | Write-Host

# Only the VC++ runtime matters: the Universal CRT (ucrtbase, api-ms-win-crt-*) ships with
# Windows 10, which the installer already requires.
$crt = $deps | Select-String -Pattern '^\s*(vcruntime\d+(_\d+)?|msvcp\d+(_\w+)?)\.dll\s*$'
if ($crt) {
  Write-Host "::error::$Exe depends on the dynamic C runtime: $(($crt | ForEach-Object { $_.Line.Trim() }) -join ', ')"
  exit 1
}
Write-Host "$Exe links the C runtime statically."
