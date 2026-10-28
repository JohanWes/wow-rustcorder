; SPDX-License-Identifier: GPL-3.0-or-later
; Per-user installer for the Windows build. windows/build.sh passes
; AppVersion, AppDir (the assembled app folder) and OutputDir.

#ifndef AppVersion
  #error AppVersion must be defined
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
OutputBaseFilename=WarcraftRustcorder-{#AppVersion}-setup
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
Type: filesandordirs; Name: "{app}\share"

[UninstallDelete]
; GStreamer's plugin cache. Settings, logs and recordings stay.
Type: filesandordirs; Name: "{localappdata}\io.github.JohanWes.WarcraftRustcorder"

[Files]
Source: "{#AppDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\Warcraft Rustcorder"; Filename: "{app}\bin\warcraft-rustcorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRustcorder"
Name: "{autodesktop}\Warcraft Rustcorder"; Filename: "{app}\bin\warcraft-rustcorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRustcorder"; Tasks: desktopicon

[Run]
Filename: "{app}\bin\warcraft-rustcorder.exe"; Description: "Start Warcraft Rustcorder"; Flags: nowait postinstall skipifsilent
; In-app updates run silently; start the new version afterwards, shown even
; with "Start minimized" on.
Filename: "{app}\bin\warcraft-rustcorder.exe"; Parameters: "--updated"; Flags: nowait; Check: WizardSilent

[Code]
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
  end;
end;
