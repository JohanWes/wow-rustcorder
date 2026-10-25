; SPDX-License-Identifier: GPL-3.0-or-later
; Per-user installer for the Windows build. windows/build.sh passes
; AppVersion, AppDir (the assembled app folder) and OutputDir.

#ifndef AppVersion
  #error AppVersion must be defined
#endif

[Setup]
AppId={{6B7E2F1A-4C3D-4E8B-9A51-2D7C0E9F3B44}
AppName=Warcraft Recorder
AppVersion={#AppVersion}
AppVerName=Warcraft Recorder {#AppVersion}
AppPublisher=JohanWes
AppPublisherURL=https://github.com/JohanWes/wow-recorder-linuxwayland
AppSupportURL=https://github.com/JohanWes/wow-recorder-linuxwayland/issues
VersionInfoVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\Warcraft Recorder
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 2004: Windows Graphics Capture and the Media Foundation encoders.
MinVersion=10.0.19041
OutputDir={#OutputDir}
OutputBaseFilename=WarcraftRecorder-{#AppVersion}-setup
SetupIconFile=warcraft-recorder.ico
UninstallDisplayIcon={app}\bin\warcraft-recorder.exe
UninstallDisplayName=Warcraft Recorder
Compression=lzma2/max
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

[Files]
Source: "{#AppDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\Warcraft Recorder"; Filename: "{app}\bin\warcraft-recorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRecorder"
Name: "{autodesktop}\Warcraft Recorder"; Filename: "{app}\bin\warcraft-recorder.exe"; AppUserModelID: "io.github.JohanWes.WarcraftRecorder"; Tasks: desktopicon

[Run]
Filename: "{app}\bin\warcraft-recorder.exe"; Description: "Start Warcraft Recorder"; Flags: nowait postinstall skipifsilent
; In-app updates run silently; start the new version afterwards.
Filename: "{app}\bin\warcraft-recorder.exe"; Flags: nowait; Check: WizardSilent

