; tty7 Windows installer (Inno Setup 6 — preinstalled on GitHub's
; windows-latest runners). Compiled by bundle-windows.ps1, which stages the
; payload and passes every path in via /D defines:
;
;   /DAppVersion=<semver>   display version parsed from Cargo.toml
;   /DVersionInfoVersion=<numeric version> PE-compatible file version
;   /DStageDir=<abs path>   staged payload (app, CLI, updater, marker, resources)
;   /DOutputDir=<abs path>  where the setup exe is written
;   /DOutputName=<basename> setup exe filename, without ".exe"
;   /DArchitectures=<id>    Inno architecture: x64compatible or arm64
;
; Defaults to a per-user install ({localappdata}\Programs\tty7 — no UAC
; prompt), with an "install for all users" escape hatch in the dialog. The
; build is unsigned, so SmartScreen warns on first launch either way — same as
; the portable zip.

#ifndef AppVersion
  #error Missing /DAppVersion — this script is meant to be compiled via bundle-windows.ps1
#endif
#ifndef VersionInfoVersion
  #error Missing /DVersionInfoVersion — this script is meant to be compiled via bundle-windows.ps1
#endif
#ifndef Architectures
  #error Missing /DArchitectures — this script is meant to be compiled via bundle-windows.ps1
#endif

[Setup]
; ntty7 does not share tty7's AppId. The same id would make this installer
; an upgrade of an installed tty7, and the uninstaller would remove that app.
AppId={{7F2C1A94-0E6B-4D58-A3C1-9B84E2D70F15}
AppName=ntty7
AppVersion={#AppVersion}
VersionInfoVersion={#VersionInfoVersion}
AppPublisher=tty7 contributors
AppPublisherURL=https://github.com/l0ng-ai/tty7
AppSupportURL=https://github.com/l0ng-ai/tty7/issues
AppUpdatesURL=https://github.com/l0ng-ai/tty7/releases
DefaultDirName={autopf}\ntty7
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed={#Architectures}
ArchitecturesInstallIn64BitMode={#Architectures}
MinVersion=10.0
LicenseFile={#StageDir}\LICENSE.txt
SetupIconFile=..\..\assets\favicon.ico
UninstallDisplayIcon={app}\ntty7.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Restart Manager closes a running ntty7 before files are replaced. It must
; not relaunch the app: an upgrade or uninstall that starts the GUI opens a
; window, and the old daemon flags this binary no longer accepts would do that.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

; Builds before the tty7/tty7-app split installed the GUI as tty7.exe. Upgrading
; only *adds* ntty7.exe, so the old binary would stay on disk — and a taskbar
; pin (which Inno cannot rewrite, unlike the [Icons] shortcuts) still points at
; it. The user would keep launching the previous version from their pinned icon,
; against the same daemon endpoint as the new one. Delete it on upgrade; a fresh
; install simply has nothing to remove.
;
; That name is now reused by the CLI below, which lands at the very same path.
; The order saves us: [InstallDelete] runs before [Files], so the stale GUI is
; gone before the CLI is written, and what survives is never a mix of the two.
; The taskbar pin is the loose end — post-upgrade it points at a CLI, so
; clicking it flashes a console instead of opening a window. That is louder
; than the bug it replaces (silently running last release's GUI), and the pin
; is not ours to rewrite; the Start Menu entry in [Icons] is correct either way.
[InstallDelete]
Type: files; Name: "{app}\tty7.exe"

[Files]
Source: "{#StageDir}\ntty7.exe"; DestDir: "{app}"; Flags: ignoreversion
; This installer-only marker is the authority for enabling automatic Windows
; updates. The portable archive is created before the marker enters the stage.
Source: "{#StageDir}\.tty7-inno-install"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\completions\*"; DestDir: "{app}\completions"; Flags: ignoreversion recursesubdirs
; Microsoft's redistributable ConPTY. It has to land in {app} rather than a
; subdirectory: `portable-pty` finds it through the DLL search path, which
; starts at the directory of the executable that loads it (the daemon is
; {app}\ntty7.exe). The two files are a matched pair — never update one
; alone. See assets\windows\conpty\README.md.
Source: "{#StageDir}\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE-ConPTY.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
; The Linux musl tty7-server, for serving WSL distros without a download.
; `skipifsourcedoesntexist` because a build whose server-musl leg was skipped
; still has to produce an installer. See bundle-windows.ps1.
Source: "{#StageDir}\server\*"; DestDir: "{app}\server"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist

; AppUserModelID is what lets toast notifications carry the tty7 name and icon
; instead of the notify-rust PowerShell fallback: Windows only honors an
; unpackaged app's toast identity when a shortcut stamps it. Must match
; `core::aumid::AUMID` (src/core/aumid.rs), which at startup stamps the
; per-user shortcut below if some older installer left it unstamped, and
; writes one from scratch for the portable zip. It deliberately leaves an
; all-users install alone — it cannot write {commonprograms} unelevated, and a
; per-user twin would both duplicate the Start Menu entry and outlive this
; uninstaller — so an elevated install depends on the stamp right here.
;
; AppUserModelToastActivatorCLSID names the COM class a click on a toast in
; notification history activates; without it that click goes nowhere (#1179).
; Must match `core::aumid::TOAST_ACTIVATOR_CLSID`. The app registers the class
; itself (HKCU, at every launch, so it always names the running exe); the
; uninstaller below removes that registration.
[Icons]
Name: "{autoprograms}\ntty7"; Filename: "{app}\ntty7.exe"; AppUserModelID: "com.github.ntty7"; AppUserModelToastActivatorCLSID: "7B4A9D2E-5C18-4F63-9A41-2E8D6B0C9F77"
Name: "{autodesktop}\ntty7"; Filename: "{app}\ntty7.exe"; Tasks: desktopicon; AppUserModelID: "com.github.ntty7"; AppUserModelToastActivatorCLSID: "7B4A9D2E-5C18-4F63-9A41-2E8D6B0C9F77"

[Run]
; skipifsilent because a silent run is the elevated in-place update: launching
; the app there would run it elevated and open a window.
Filename: "{app}\ntty7.exe"; Description: "{cm:LaunchProgram,ntty7}"; Flags: nowait postinstall skipifsilent

[Code]
(* Take {app} back out of the user's PATH.

  `core::cli_install` puts it there at first launch rather than the installer
  doing it, because the portable zip has no installer — but that leaves nobody
  to undo it, and an uninstall that permanently grows the user's PATH by one
  dead entry is not an uninstall. So the removal lives here, on the one install
  shape that has an uninstaller at all. (Unix has no equivalent hook: deleting
  the .app or the tarball leaves the symlink behind for the user to remove.)

  HKCU even for an all-users install: cli_install only ever writes the user
  hive, so that is the only place an entry can be. On a machine where several
  users ran tty7, this clears the one uninstalling — the others keep a dead
  entry, which is the price of not needing elevation to install in the first
  place.

  Entries are compared case-insensitively and ignoring a trailing backslash,
  and every other entry is written back verbatim: this is somebody's PATH, and
  we are here to remove one thing from it, not to tidy it. No
  WM_SETTINGCHANGE broadcast — the entry now names a deleted directory, so
  nothing is waiting on the news, and it is gone from new shells at next
  sign-in regardless. *)
procedure RemoveAppDirFromUserPath();
var
  Existing, Rebuilt, Raw, Entry, Target: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Existing) then
    exit;

  Target := RemoveBackslashUnlessRoot(ExpandConstant('{app}'));
  Rebuilt := '';
  (* The trailing ';' makes the last entry look like every other one. *)
  Existing := Existing + ';';
  repeat
    P := Pos(';', Existing);
    Raw := Copy(Existing, 1, P - 1);
    Existing := Copy(Existing, P + 1, Length(Existing));
    Entry := Trim(Raw);
    if (Entry <> '') and
       (CompareText(RemoveBackslashUnlessRoot(Entry), Target) <> 0) then
    begin
      if Rebuilt <> '' then
        Rebuilt := Rebuilt + ';';
      Rebuilt := Rebuilt + Raw;
    end;
  until Existing = '';

  if Rebuilt = '' then
    RegDeleteValue(HKEY_CURRENT_USER, 'Environment', 'Path')
  (* Inno cannot read a value's type back, so infer it: a PATH holding a '%' is
     one that has to stay expandable, and rewriting it as REG_SZ would freeze
     every other entry's variable at today's value. *)
  else if Pos('%', Rebuilt) > 0 then
    RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Rebuilt)
  else
    RegWriteStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Rebuilt);
end;

(* Remove the toast activator's COM registration.

  `core::aumid` writes HKCU\Software\Classes\CLSID\{…}\LocalServer32 at launch
  so that a click on a toast in notification history can start tty7. Left
  behind, it would point COM at a deleted exe. Only the class's own key is
  deleted; a missing key is a no-op. HKCU for the same reason as the PATH
  entry above: that is the only hive the app writes. *)
procedure RemoveToastActivator();
begin
  RegDeleteKeyIncludingSubkeys(
    HKEY_CURRENT_USER, 'Software\Classes\CLSID\{7B4A9D2E-5C18-4F63-9A41-2E8D6B0C9F77}');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    RemoveAppDirFromUserPath();
    RemoveToastActivator();
  end;
end;
