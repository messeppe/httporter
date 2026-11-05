; HTTPorter installer, for Inno Setup 6.7 or later (https://jrsoftware.org/isinfo.php).
;
; tools/package.ps1 builds it; by hand, from the repository root:
;   ISCC.exe /DAppVersion=1.0.0 /DSourceDir=..\dist\HTTPorter-1.0.0 /O..\dist installer\httporter.iss
; Relative paths are relative to this folder (installer\).
;
; Defines (ISCC /D<name>=<value>):
;   AppVersion   required. The version, from Cargo.toml (package.ps1 reads it with cargo metadata).
;   SourceDir    required. The staged folder: httporter.exe, LICENSE-MIT.txt, LICENSE-APACHE.txt,
;                THIRD-PARTY-NOTICES.txt, README.txt.
;   VersionInfo  the numeric x.y.z for the setup's file version; default AppVersion.
;   IconFile     the setup icon; default ..\assets\icon\httporter.ico.
;   RepoUrl      the project page (https://github.com/<owner>/<repo>), shown in Apps & features.
;   Sign         sign the uninstaller and the setup with the sign tool named "httporter", which the
;                command line defines: /Shttporter="powershell -NoProfile -File tools\sign.ps1 $f".
;                Without it nothing here is signed (package.ps1 signs the setup afterwards).
;
; Per-user install, no admin rights: %LOCALAPPDATA%\Programs\HTTPorter. Uninstalling removes the
; program and keeps the user's data (%APPDATA%\HTTPorter, %LOCALAPPDATA%\HTTPorter and the
; Credential Manager entries); Settings > Security > Reset HTTPorter removes those.

#ifndef AppVersion
  #error Pass the version: ISCC /DAppVersion=1.0.0
#endif
#ifndef SourceDir
  #error Pass the staged folder: ISCC /DSourceDir=..\dist\HTTPorter-1.0.0
#endif
#ifndef VersionInfo
  #define VersionInfo AppVersion
#endif
#ifndef IconFile
  #define IconFile "..\assets\icon\httporter.ico"
#endif

[Setup]
; Fixed for every release: Windows knows the installed HTTPorter by it. Never change it.
AppId={{81E95390-CEDF-4D02-85DF-ABFBA19FD9D7}
AppName=HTTPorter
AppVersion={#AppVersion}
AppVerName=HTTPorter {#AppVersion}
AppPublisher=The HTTPorter contributors
AppCopyright=Copyright (c) 2026 the HTTPorter contributors. MIT OR Apache-2.0.
#ifdef RepoUrl
AppPublisherURL={#RepoUrl}
AppSupportURL={#RepoUrl}/issues
AppUpdatesURL={#RepoUrl}/releases
#endif
VersionInfoVersion={#VersionInfo}
VersionInfoProductName=HTTPorter
VersionInfoDescription=HTTPorter Setup
; Per user: {autopf} is %LOCALAPPDATA%\Programs, where HTTPorter has been installed so far.
PrivilegesRequired=lowest
DefaultDirName={autopf}\HTTPorter
DisableProgramGroupPage=yes
DisableReadyPage=no
; The app holds this mutex while it runs (crates/httporter-app/src/single_instance.rs): Setup
; and the uninstaller ask the user to close it first.
AppMutex=Local\HTTPorter.Running
CloseApplications=yes
RestartApplications=no
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\httporter.exe
UninstallDisplayName=HTTPorter
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=HTTPorter-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
#ifdef Sign
SignTool=httporter
SignedUninstaller=yes
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
ConfirmUninstall=Remove %1 from this PC?%n%nYour sites, settings, history and saved keys are kept, so installing HTTPorter again picks up where you left off. To remove them too, choose No, open HTTPorter and use Settings › Security › Reset HTTPorter… first.
UninstalledAll=%1 was removed from this PC.%n%nYour sites, settings and saved keys were kept. To remove them, install HTTPorter again and choose Settings › Security › Reset HTTPorter.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\httporter.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE-MIT.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE-APACHE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The app gives its process this id (crates/httporter-platform/src/win/notify.rs), so the
; shortcut, the taskbar button and the toasts are one HTTPorter.
Name: "{autoprograms}\HTTPorter"; Filename: "{app}\httporter.exe"; AppUserModelID: "HTTPorter.App"
Name: "{autodesktop}\HTTPorter"; Filename: "{app}\httporter.exe"; AppUserModelID: "HTTPorter.App"; Tasks: desktopicon

[Run]
Filename: "{app}\httporter.exe"; Description: "{cm:LaunchProgram,HTTPorter}"; Flags: nowait postinstall skipifsilent
