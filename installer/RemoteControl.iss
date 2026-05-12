; ------------------------------------------------------------------
;  RemoteControl Windows installer (Inno Setup 6+)
; ------------------------------------------------------------------
;
;  This script is invoked by build-installer.ps1 — DON'T run it
;  directly unless you've already prepared the staging files:
;
;    * ..\target\release\remotecontrol-server.exe   (cargo build --release)
;    * vendor\MicrosoftEdgeWebview2Setup.exe        (downloaded by the ps1)
;    * icon.ico                                     (extracted from cargo OUT_DIR)
;
;  The ps1 stages those, then runs:  ISCC.exe RemoteControl.iss
;
;  Output:  dist\RemoteControl-Setup-<version>.exe
; ------------------------------------------------------------------

#define AppName        "RemoteControl"
#define AppPublisher   "RemoteControl"
#define AppVersion     "0.2.0"
#define AppExeName     "remotecontrol-server.exe"
; Stable AppId — Inno's uninstaller key. Do NOT change between
; versions or every install will live alongside the old one.
#define AppId          "{{6B2D8E1C-A93F-4F4B-9F8A-1F9B5C8E2A41}"

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL=https://github.com/Minato-mzc
; Per-user install — no UAC prompt, lives under %LOCALAPPDATA%\Programs\,
; and the autostart entry we write to HKCU\…\Run actually targets the
; current user instead of the administrator's hive. {userpf} resolves
; to that path. The user can still override to a machine-wide install
; via the wizard's "Install for all users" toggle (controlled by the
; `PrivilegesRequiredOverridesAllowed=dialog` flag below).
DefaultDirName={userpf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=dist
OutputBaseFilename=RemoteControl-Setup-{#AppVersion}
SetupIconFile=icon.ico
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
; 64-bit only — DXGI Desktop Duplication and our NVENC FFI both
; assume an x64 process.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

[Languages]
; Inno Setup 6.7+ no longer ships the simplified-Chinese translation
; (licensing reasons). Wizard chrome is English; our custom button/task
; labels below stay Chinese because the target users are.
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: checkedonce
Name: "autostart";   Description: "开机自动启动 {#AppName}";   GroupDescription: "启动选项:"; Flags: checkedonce

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
; WebView2 evergreen bootstrapper — Microsoft's ~1 MB stub that
; downloads + installs the runtime on demand. We only run it when
; WebView2 is missing (see [Run] / NeedsWebView2 below).
Source: "vendor\MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Icons]
Name: "{group}\{#AppName}";       Filename: "{app}\{#AppExeName}"
Name: "{group}\卸载 {#AppName}";  Filename: "{uninstallexe}"
; {autodesktop} = per-user desktop in lowest mode, all-users desktop
; if the wizard's "Install for all users" override was taken.
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
; HKCU Run entry — survives Windows reboots, doesn't need admin to
; remove (uninstaller drops it via uninsdeletevalue). Quoted path so
; "Program Files" doesn't confuse the parser at logon.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExeName}"""; \
    Tasks: autostart; Flags: uninsdeletevalue

[Run]
; Install WebView2 silently if absent. /silent /install is the
; documented evergreen bootstrapper flag pair — installs the latest
; runtime to a per-machine location, no UI, no reboot.
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; \
    Parameters: "/silent /install"; \
    StatusMsg: "正在安装 WebView2 运行时…"; \
    Check: NeedsWebView2

; Launch right after install. skipifsilent so /VERYSILENT installs
; (deploy scripts) don't pop a window.
Filename: "{app}\{#AppExeName}"; \
    Description: "立即启动 {#AppName}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; Best-effort: kill any running instance so Inno can delete the exe.
; Errors silenced — if it's not running, taskkill returns non-zero.
Filename: "{sys}\taskkill.exe"; \
    Parameters: "/F /IM {#AppExeName}"; \
    Flags: runhidden; \
    RunOnceId: "KillRunning"

[Code]
// True if WebView2 evergreen runtime is NOT installed.
// Inno checks the 32-bit registry view by default; WebView2 logs
// its machine-wide install under WOW6432Node on 64-bit Windows.
// We also check the HKCU per-user variant for completeness.
function NeedsWebView2(): Boolean;
var
  Version: String;
begin
  Result := True;
  if RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) and (Version <> '') then
    Result := False
  else if RegQueryStringValue(HKLM, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) and (Version <> '') then
    Result := False
  else if RegQueryStringValue(HKCU, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) and (Version <> '') then
    Result := False;
end;
