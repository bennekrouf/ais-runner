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
; Install per-user into %LOCALAPPDATA%\AIS Runner — no UAC prompt needed.
DefaultDirName={localappdata}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\dist
OutputBaseFilename=ais-runner-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1809+ required for WebView2
MinVersion=10.0.17763
UninstallDisplayName={#MyAppName} {#MyAppVersion}
; Kill running instance before upgrade, restart after if it was running
CloseApplications=yes
; Clean install: remove files that are no longer part of the new version
; (Inno writes the file list into the uninstall log; this flag uses that list)

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
; Always install runtime dependencies — the script skips anything already present
; (completes in seconds if tools are installed, a few minutes on a fresh machine).
Filename: "powershell.exe"; \
  Parameters: "-ExecutionPolicy Bypass -NoProfile -File ""{app}\setup-windows.ps1"" -NoPrompt"; \
  StatusMsg: "Installing runtime dependencies — skipping already-installed tools..."; \
  Flags: waituntilterminated

; Offer to launch the app on the Finish page
Filename: "{app}\{#MyAppExeName}"; \
  Description: "Launch {#MyAppName}"; \
  Flags: nowait postinstall skipifsilent

; ── Pascal script ─────────────────────────────────────────────────────────────
; Detects an existing install, asks the user to confirm the upgrade, and
; performs a clean uninstall of the previous version before copying new files.
[Code]

// Registry key written by Inno for per-user installs
function GetInstalledVersion(): String;
var
  RegKey: String;
  Ver: String;
begin
  RegKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' +
            '{8B4F5C2A-3D1E-4F7B-9A6C-E2D8F3B1C4A5}_is1';
  if not RegQueryStringValue(HKCU, RegKey, 'DisplayVersion', Ver) then
    Ver := '';
  Result := Ver;
end;

function GetUninstallString(): String;
var
  RegKey: String;
  UninstStr: String;
begin
  RegKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' +
            '{8B4F5C2A-3D1E-4F7B-9A6C-E2D8F3B1C4A5}_is1';
  if not RegQueryStringValue(HKCU, RegKey, 'QuietUninstallString', UninstStr) then
    UninstStr := '';
  Result := UninstStr;
end;

// Called before the wizard pages are shown.
// If a previous version is found: show a confirmation dialog listing both
// versions, then silently uninstall the old one before proceeding.
function InitializeSetup(): Boolean;
var
  InstalledVer: String;
  NewVer:       String;
  Msg:          String;
  UninstStr:    String;
  ResultCode:   Integer;
begin
  Result := True;   // default: proceed with install

  InstalledVer := GetInstalledVersion();
  if InstalledVer = '' then
    Exit;   // fresh install — nothing to do

  NewVer := '{#MyAppVersion}';

  if InstalledVer = NewVer then
    Msg := 'Version ' + InstalledVer + ' of {#MyAppName} is already installed.' + #13#10 +
           #13#10 +
           'Do you want to reinstall it?'
  else
    Msg := '{#MyAppName} is already installed.' + #13#10 +
           #13#10 +
           '  Installed version:  ' + InstalledVer + #13#10 +
           '  New version:        ' + NewVer + #13#10 +
           #13#10 +
           'The old version will be removed before installing the new one.' + #13#10 +
           'Your settings and data will be preserved.' + #13#10 +
           #13#10 +
           'Continue?';

  if MsgBox(Msg, mbConfirmation, MB_YESNO) = IDNO then
  begin
    Result := False;   // user cancelled
    Exit;
  end;

  // Silent uninstall of the previous version
  UninstStr := GetUninstallString();
  if UninstStr <> '' then
  begin
    // QuietUninstallString already contains /SILENT — just run it
    Exec('>', UninstStr, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    // Give the OS a moment to finish cleaning up file handles
    Sleep(500);
  end;
end;
