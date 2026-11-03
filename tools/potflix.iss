; Potflix installer (Inno Setup 6).
;
; Relative Source/Output paths resolve against this file's own directory
; (`tools\`), hence the `..\` prefixes. Keep ArchitecturesAllowed /
; ArchitecturesInstallIn64BitMode unset: minimal and version-proof.

#define MyAppName "Potflix"
#define MyAppVersion "1.1.0"
#define MyAppPublisher "nathwn12"
#define MyAppExeName "potflix.exe"

[Setup]
AppId={{8F3C2A54-6E1B-4E2D-9C77-2B5A0D4E7F31}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\Potflix
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Potflix-Setup
SetupIconFile=..\build\potflix.ico
UninstallDisplayIcon={app}\potflix.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes

[Files]
Source: "..\dist\potflix.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\client-id.txt"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: checkedonce

[Icons]
Name: "{autoprograms}\Potflix"; Filename: "{app}\potflix.exe"; WorkingDir: "{app}"; IconFilename: "{app}\potflix.exe"
Name: "{autodesktop}\Potflix"; Filename: "{app}\potflix.exe"; WorkingDir: "{app}"; IconFilename: "{app}\potflix.exe"; Tasks: desktopicon

[InstallDelete]
Type: files; Name: "{app}\potflix.ico"

[Run]
Filename: "{app}\potflix.exe"; Description: "{cm:LaunchProgram,Potflix}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: files; Name: "{app}\potflix-debug*.log*"
Type: files; Name: "{app}\m1-selftest.out"
Type: files; Name: "{app}\potflix.ico"
Type: dirifempty; Name: "{app}"
