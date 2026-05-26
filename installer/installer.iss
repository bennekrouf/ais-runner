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
; Install per-user into %LOCALAPPDATA%\Programs — no UAC prompt needed.
; The app + WebView2 data dir are both in user-writable space, so
; "cannot create data directory" errors never occur.
; Use /ALLUSERS on the command line to override to a system-wide install.
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
; WebView2 Evergreen Bootstrapper — installs silently if runtime is missing.
; Downloaded from https://go.microsoft.com/fwlink/p/?LinkId=2124703
Source: "..\installer\MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Icons]
Name: "{group}\{#MyAppName}";           Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{commondesktop}\{#MyAppName}";   Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; Install WebView2 runtime silently if not already present.
; /silent = no UI, /install = install if needed, exit 0 if already installed.
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; \
  Parameters: "/silent /install"; \
  StatusMsg: "Installing WebView2 runtime..."; \
  Flags: waituntilterminated

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
