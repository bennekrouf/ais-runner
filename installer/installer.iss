; AIS Runner — Windows Installer
; Build: iscc /DMyAppVersion=X.Y.Z installer\installer.iss
; Output: dist\ais-runner-setup.exe

#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

#define MyAppName      "AIS Runner"
#define MyAppPublisher "Bennekrouf"
#define MyAppURL       "https://github.com/Bennekrouf/ais-runner"
#define MyAppExeName   "ais-runner.exe"

[Setup]
AppId={{8B4F5C2A-3D1E-4F7B-9A6C-E2D8F3B1C4A5}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases/latest
; Admin install — UAC appears once at startup so the dependency script
; (Node.js, Azure CLI, func, azurite) can install without self-elevation tricks.
; ais-runner.exe itself is installed to Program Files but stores all runtime
; data in %LOCALAPPDATA%\AIS Runner\ (created at launch by Rust code).
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\dist
OutputBaseFilename=ais-runner-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
UninstallDisplayName={#MyAppName} {#MyAppVersion}
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; \
  Description: "Create a &desktop shortcut"; \
  GroupDescription: "Additional shortcuts:"

[Files]
Source: "..\target\release\ais-runner.exe";     DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\WebView2Loader.dll";  DestDir: "{app}"; Flags: ignoreversion
Source: "..\scripts\setup-windows.ps1";          DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}";           Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{commondesktop}\{#MyAppName}";   Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; Installer already runs elevated — the script runs as admin directly,
; no self-elevation needed.  Skips already-installed tools in seconds.
Filename: "powershell.exe"; \
  Parameters: "-ExecutionPolicy Bypass -NoProfile -File ""{app}\setup-windows.ps1"" -NoPrompt"; \
  StatusMsg: "Installing runtime dependencies (Node, Azure CLI, func, azurite)..."; \
  Flags: waituntilterminated runhidden

; Offer to launch the app on the Finish page
Filename: "{app}\{#MyAppExeName}"; \
  Description: "Launch {#MyAppName}"; \
  Flags: nowait postinstall skipifsilent

; ── Upgrade detection ─────────────────────────────────────────────────────────
; Reads the previously installed version from the registry, shows a confirmation
; dialog with both version numbers, and silently removes the old install before
; copying new files. Settings/data are preserved (not managed by Inno).
[Code]

function GetInstalledVersion(): String;
var
  RegKey: String;
  Ver:    String;
begin
  RegKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8B4F5C2A-3D1E-4F7B-9A6C-E2D8F3B1C4A5}_is1';
  if not RegQueryStringValue(HKLM, RegKey, 'DisplayVersion', Ver) then
    if not RegQueryStringValue(HKCU, RegKey, 'DisplayVersion', Ver) then
      Ver := '';
  Result := Ver;
end;

function GetUninstallString(): String;
var
  RegKey:    String;
  UninstStr: String;
begin
  RegKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8B4F5C2A-3D1E-4F7B-9A6C-E2D8F3B1C4A5}_is1';
  if not RegQueryStringValue(HKLM, RegKey, 'QuietUninstallString', UninstStr) then
    if not RegQueryStringValue(HKCU, RegKey, 'QuietUninstallString', UninstStr) then
      UninstStr := '';
  Result := UninstStr;
end;

function InitializeSetup(): Boolean;
var
  InstalledVer: String;
  NewVer:       String;
  Msg:          String;
  UninstStr:    String;
  ResultCode:   Integer;
  NL:           String;
begin
  Result := True;
  NL := #13#10;

  InstalledVer := GetInstalledVersion();
  if InstalledVer = '' then
    Exit;   // fresh install

  NewVer := '{#MyAppVersion}';

  if InstalledVer = NewVer then
    Msg := 'Version ' + InstalledVer + ' of {#MyAppName} is already installed.' + NL + NL +
           'Do you want to reinstall it?'
  else
    Msg := '{#MyAppName} is already installed.' + NL + NL +
           '  Installed version:  ' + InstalledVer + NL +
           '  New version:        ' + NewVer + NL + NL +
           'The old version will be removed before installing the new one.' + NL +
           'Your settings and data will be preserved.' + NL + NL +
           'Continue?';

  if MsgBox(Msg, mbConfirmation, MB_YESNO) = IDNO then
  begin
    Result := False;
    Exit;
  end;

  // Silent uninstall of the previous version
  UninstStr := GetUninstallString();
  if UninstStr <> '' then
  begin
    Exec('>', UninstStr, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Sleep(500);
  end;
end;
