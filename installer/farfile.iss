; The Windows installer, built by CI with Inno Setup 6:
;
;   iscc /DAppVersion=0.1.0 installer\farfile.iss
;
; It installs for the current user only, without administrator rights, into
; %LOCALAPPDATA%\Programs\Farfile, with a Start menu entry. The marker file
; it leaves next to the program tells Farfile to update itself by running
; the next release's installer (see src/update.rs), which Farfile does with
;
;   Farfile-Setup-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- [/RELAUNCH=1]
;
; /RELAUNCH=1 starts Farfile again once the update is in place.
;
; Saved connections, preferences and the keychain live in the user's
; profile (%APPDATA%\s3viewer), not here: updating or uninstalling leaves
; them alone.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z installer\farfile.iss
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\s3viewer.exe"
#endif

[Setup]
; Never change: Windows knows the installation by this id.
AppId={{A8834BA0-6061-4996-B81E-1E145BB465A1}
AppName=Farfile
AppVersion={#AppVersion}
AppVerName=Farfile {#AppVersion}
AppPublisher=Moulman
AppPublisherURL=https://github.com/moulmandev/Farfile
AppSupportURL=https://github.com/moulmandev/Farfile/issues
AppUpdatesURL=https://github.com/moulmandev/Farfile/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Farfile
DisableProgramGroupPage=yes
DisableDirPage=yes
; Per user: no elevation prompt, and updates can run silently.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=Farfile-Setup-x64
SetupIconFile=..\assets\farfile.ico
UninstallDisplayIcon={app}\farfile.exe
UninstallDisplayName=Farfile
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; A running Farfile is closed before its files are replaced (silently
; during an update), and not started again by Windows: /RELAUNCH does that.
CloseApplications=yes
RestartApplications=no
; The language of Windows when it is one of these, without asking.
ShowLanguageDialog=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "french"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "farfile.exe"; Flags: ignoreversion
Source: "farfile-installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Farfile"; Filename: "{app}\farfile.exe"
Name: "{autodesktop}\Farfile"; Filename: "{app}\farfile.exe"; Tasks: desktopicon

[Run]
; After installing by hand: offer to start it.
Filename: "{app}\farfile.exe"; Description: "{cm:LaunchProgram,Farfile}"; Flags: nowait postinstall skipifsilent
; After an update asked for with the status bar's restart button.
Filename: "{app}\farfile.exe"; Flags: nowait; Check: Relaunch

[Code]
function Relaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;
