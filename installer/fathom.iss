; Windows installer for Fathom, built with Inno Setup 6:
;
;   iscc /DAppVersion=0.2.0 /DSourceExe=..\target\release\fathom.exe
;        /DSourceCliExe=..\target\release\fth.exe installer\fathom.iss
;
; AppId must never change: it is how a newer installer finds the installed copy and
; updates it in place, keeping the install folder and the user's connections.

#if VER < EncodeVer(6, 6, 0)
  #error Inno Setup 6.6 or later is required (dark mode and .png wizard images)
#endif

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\fathom.exe"
#endif
#ifndef SourceCliExe
  #define SourceCliExe "..\target\release\fth.exe"
#endif
; Windows file versions are four numbers, so drop any pre-release suffix.
#define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1) + ".0"

[Setup]
AppId={{ECFF7C6D-44ED-43C5-AAA0-A05516A53EC0}
AppName=Fathom
AppVersion={#AppVersion}
AppVerName=Fathom {#AppVersion}
AppPublisher=Den Delimarsky
AppPublisherURL=https://github.com/dend/fathom
AppSupportURL=https://github.com/dend/fathom/issues
AppUpdatesURL=https://github.com/dend/fathom/releases
AppCopyright=Copyright (c) Den Delimarsky
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
VersionInfoDescription=Fathom Setup

; Installed for the current user, so neither installing nor updating asks for admin.
PrivilegesRequired=lowest
DefaultDirName={autopf}\Fathom
DisableDirPage=auto
DisableProgramGroupPage=yes
DisableWelcomePage=no
UsePreviousAppDir=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

; Updating while Fathom is open closes it first. Fathom doesn't answer the Restart
; Manager's polite shutdown request, so it has to be forced.
CloseApplications=force
RestartApplications=no
; Tells open windows when the PATH changes, so new terminals find fth.
ChangesEnvironment=yes

; Follows the Windows light or dark setting, like the app. Each image comes in the
; sizes Setup picks from for different display scaling. The same images work in both
; modes, but dark mode needs them named again or it uses Inno Setup's own.
#define WizardImages "wizard-164x314.png,wizard-192x386.png,wizard-246x459.png,wizard-273x556.png,wizard-328x604.png,wizard-355x700.png,wizard-410x797.png"
#define WizardSmallImages "wizard-small-55x55.png,wizard-small-64x68.png,wizard-small-83x80.png,wizard-small-92x97.png,wizard-small-110x106.png,wizard-small-119x123.png,wizard-small-138x140.png"
WizardStyle=modern dynamic
WizardImageFile={#WizardImages}
WizardImageFileDynamicDark={#WizardImages}
WizardSmallImageFile={#WizardSmallImages}
WizardSmallImageFileDynamicDark={#WizardSmallImages}
; Setup has its own icon, a cargo crate sunk on the app icon's seabed, so the
; installer and the installed app are easy to tell apart.
SetupIconFile=setup.ico
UninstallDisplayIcon={app}\fathom.exe
UninstallDisplayName=Fathom

OutputBaseFilename=fathom-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "addtopath"; Description: "Add fth to the &PATH, to run it from any terminal"; GroupDescription: "Command line:"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "fathom.exe"; Flags: ignoreversion
Source: "{#SourceCliExe}"; DestDir: "{app}"; DestName: "fth.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Fathom"; Filename: "{app}\fathom.exe"
Name: "{autodesktop}\Fathom"; Filename: "{app}\fathom.exe"; Tasks: desktopicon

; Programs that updating moved aside because they were running.
[InstallDelete]
Type: files; Name: "{app}\fathom.exe.old"
Type: files; Name: "{app}\fth.exe.old"

[UninstallDelete]
Type: files; Name: "{app}\*.old"

[Run]
Filename: "{app}\fathom.exe"; Description: "Open Fathom"; Flags: nowait postinstall skipifsilent
; Fathom updating itself runs Setup silently with /relaunch=yes, to be opened again after.
Filename: "{app}\fathom.exe"; Flags: nowait; Check: RelaunchRequested

[Code]
// The version recorded by the copy already installed, if any.
function InstalledVersion(): String;
var
  Version: String;
begin
  Result := '';
  if RegQueryStringValue(HKA,
    'Software\Microsoft\Windows\CurrentVersion\Uninstall\{ECFF7C6D-44ED-43C5-AAA0-A05516A53EC0}_is1',
    'DisplayVersion', Version) then
    Result := Version;
end;

function RelaunchRequested(): Boolean;
begin
  Result := ExpandConstant('{param:relaunch|no}') = 'yes';
end;

// Puts the install folder in the user's PATH, or takes it out.
procedure UpdatePath(Add: Boolean);
var
  Path, Padded, Folder: String;
  Index: Integer;
begin
  Folder := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Path) then
    Path := '';
  // With semicolons around both, C:\Fathom can't match inside C:\Fathom2.
  Padded := ';' + Path + ';';
  Index := Pos(';' + Uppercase(Folder) + ';', Uppercase(Padded));
  if Add and (Index = 0) then
  begin
    if (Path <> '') and (Path[Length(Path)] <> ';') then
      Path := Path + ';';
    RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Path + Folder);
  end
  else if not Add and (Index > 0) then
  begin
    Delete(Padded, Index, Length(Folder) + 1);
    RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Copy(Padded, 2, Length(Padded) - 2));
  end;
end;

// The checkbox decides, so unticking it when updating takes the folder out again.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    UpdatePath(WizardIsTaskSelected('addtopath'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    UpdatePath(False);
end;

procedure InitializeWizard();
var
  Installed: String;
begin
  Installed := InstalledVersion();
  if Installed = '{#AppVersion}' then
    WizardForm.WelcomeLabel2.Caption :=
      'Fathom {#AppVersion} is already installed. Setup will reinstall it.' + #13#10#13#10 +
      'Your connections and settings are kept.'
  else if Installed <> '' then
    WizardForm.WelcomeLabel2.Caption :=
      'Setup will update Fathom from ' + Installed + ' to {#AppVersion}.' + #13#10#13#10 +
      'Your connections and settings are kept. If Fathom is open, Setup will close it first.';
end;
