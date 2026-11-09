; SPDX-License-Identifier: GPL-3.0-or-later
; Per-user installer for the Windows build. windows/build.sh passes
; AppVersion, AppDir (the assembled app folder) and OutputDir.
;
; With UpdateOnly and SpellsSha256 it builds the update package instead: the
; same installer without the spell bundle, which the app's updater downloads
; when the installed bundle already has that hash.

#ifndef AppVersion
  #error AppVersion must be defined
#endif
#ifdef UpdateOnly
  #define Kind "update"
#else
  #define Kind "setup"
#endif

[Setup]
AppId={{6B7E2F1A-4C3D-4E8B-9A51-2D7C0E9F3B44}
AppName=Warcraft Rustcorder
AppVersion={#AppVersion}
AppVerName=Warcraft Rustcorder {#AppVersion}
AppPublisher=JohanWes
AppPublisherURL=https://github.com/JohanWes/wow-rustcorder
AppSupportURL=https://github.com/JohanWes/wow-rustcorder/issues
VersionInfoVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\Warcraft Rustcorder
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 2004: Windows Graphics Capture and the Media Foundation encoders.
MinVersion=10.0.19041
OutputDir={#OutputDir}
OutputBaseFilename=WarcraftRustcorder-{#AppVersion}-{#Kind}
SetupIconFile=warcraft-rustcorder.ico
UninstallDisplayIcon={app}\bin\warcraft-rustcorder.exe
UninstallDisplayName=Warcraft Rustcorder
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; An update closes the running app (and its recorder) first.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[InstallDelete]
; Files a previous version shipped must not linger next to the new ones.
Type: filesandordirs; Name: "{app}\bin"
Type: filesandordirs; Name: "{app}\lib"
Type: filesandordirs; Name: "{app}\libexec"
#ifdef UpdateOnly
; Everything in share except the spell bundle.
Type: filesandordirs; Name: "{app}\share\glib-2.0"
Type: filesandordirs; Name: "{app}\share\icons"
Type: filesandordirs; Name: "{app}\share\licenses"
#else
Type: filesandordirs; Name: "{app}\share"
#endif

[UninstallDelete]
; GStreamer's plugin cache. Settings, logs and recordings stay.
Type: filesandordirs; Name: "{localappdata}\io.github.JohanWes.WarcraftRustcorder"

[Files]
#ifdef UpdateOnly
Source: "{#AppDir}\*"; DestDir: "{app}"; Excludes: "spells.gresource"; Flags: recursesubdirs ignoreversion
#else
Source: "{#AppDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion
#endif

[Icons]
Name: "{autoprograms}\Warcraft Rustcorder"; Filename: "{app}\bin\warcraft-rustcorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRustcorder"
Name: "{autodesktop}\Warcraft Rustcorder"; Filename: "{app}\bin\warcraft-rustcorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRustcorder"; Tasks: desktopicon

[Run]
Filename: "{app}\bin\warcraft-rustcorder.exe"; Description: "Start Warcraft Rustcorder"; Flags: nowait postinstall skipifsilent
; In-app updates run silently; start the new version afterwards, shown even
; with "Start minimized" on.
Filename: "{app}\bin\warcraft-rustcorder.exe"; Parameters: "--updated"; Flags: nowait; Check: WizardSilent

[Code]
#ifdef UpdateOnly
// The update package only fits an install that already has this release's
// spell bundle; over anything else it would leave old or no spell icons.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Spells: String;
begin
  Result := '';
  Spells := ExpandConstant('{app}\share\warcraft-rustcorder\spells.gresource');
  if not FileExists(Spells) or (CompareText(GetSHA256OfFile(Spells), '{#SpellsSha256}') <> 0) then
    Result := 'This update package does not fit the installed version. Download WarcraftRustcorder-{#AppVersion}-setup.exe instead.';
end;
#endif

// The uninstaller does not close running apps the way setup does: stop the
// app, its recorder and its gdbus helper, all of which run from {app}.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    Dir := ExpandConstant('{app}');
    StringChangeEx(Dir, '''', '''''', True);
    Exec('powershell.exe', '-NoProfile -NonInteractive -Command "Get-Process | Where-Object { $_.Path -like ''' + Dir + '\*'' } | Stop-Process -Force; Start-Sleep 1"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    // "Start with system" (native/src/ui/autostart.rs).
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Warcraft Rustcorder');
  end;
end;
