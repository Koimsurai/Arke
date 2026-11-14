; Arke's installer (Inno Setup 6.6 or later, for dark mode).
;
;   iscc /DAppVersion=0.4.0 /DArch=x64 /DSourceExe=..\dist\arke-0.4.0.exe /O..\dist installer\arke.iss
;
; `cargo xtask build` runs this when Inno Setup is installed. It installs for the current user by
; default (%LOCALAPPDATA%\Programs\Arke, no administrator approval); the first dialog also offers
; "all users" (Program Files). Besides the files it only writes the uninstall entry Windows lists
; under Settings > Apps.
;
; Arke's own updater runs a newer setup with /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /LAUNCH;
; /LAUNCH starts Arke again when it is done.
; Uninstalling runs `arke.exe --uninstall --quiet` first, which undoes what Arke changed itself
; (the search service, "Open folders in Arke", saved passwords, its data folders). Settings are
; kept or removed as asked; /KEEPSETTINGS or /REMOVESETTINGS answer for the in-app "Remove Arke".

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif
; Previews only: /DWizardMode=light or dark; releases follow Windows (dynamic).
#ifndef WizardMode
  #define WizardMode "dynamic"
#endif
#ifndef SourceExe
  #error Pass /DSourceExe=<path to arke.exe>
#endif
#if Arch == "arm64"
  #define ArchSuffix "-arm64"
  #define ArchAllowed "arm64"
#else
  #define ArchSuffix ""
  #define ArchAllowed "x64compatible"
#endif

[Setup]
AppId={{3593FF9D-559F-4CAF-98F5-21A310C846D4}
AppName=Arke
AppVersion={#AppVersion}
AppVerName=Arke {#AppVersion}
AppPublisher=Arke contributors
AppPublisherURL=https://github.com/Koimsurai/Arke
AppSupportURL=https://github.com/Koimsurai/Arke/issues
AppUpdatesURL=https://github.com/Koimsurai/Arke/releases
VersionInfoVersion={#Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1)}
VersionInfoProductName=Arke
VersionInfoDescription=Arke setup
DefaultDirName={autopf}\Arke
DisableProgramGroupPage=yes
DisableReadyPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchAllowed}
MinVersion=10.0.17763
OutputBaseFilename=arke-{#AppVersion}{#ArchSuffix}-setup
SetupIconFile=..\crates\app\assets\brand\arke.ico
UninstallDisplayIcon={app}\arke.exe
UninstallDisplayName=Arke
WizardStyle=modern {#WizardMode}
WizardSizePercent=100
WizardImageFile=wizard-100.bmp,wizard-125.bmp,wizard-150.bmp,wizard-175.bmp,wizard-200.bmp
WizardSmallImageFile=wizard-small-100.bmp,wizard-small-125.bmp,wizard-small-150.bmp,wizard-small-175.bmp,wizard-small-200.bmp
; Dynamic mode takes separate images for dark; the transparent ones work on both backgrounds.
WizardImageFileDynamicDark=wizard-100.bmp,wizard-125.bmp,wizard-150.bmp,wizard-175.bmp,wizard-200.bmp
WizardSmallImageFileDynamicDark=wizard-small-100.bmp,wizard-small-125.bmp,wizard-small-150.bmp,wizard-small-175.bmp,wizard-small-200.bmp
WizardImageAlphaFormat=defined
WizardImageStretch=no
WizardImageBackColor=$fbfbfb
WizardImageBackColorDynamicDark=$272727
Compression=lzma2/max
SolidCompression=yes
; Arke is asked to quit first (PrepareToInstall); whatever is left is closed by Windows.
CloseApplications=force
CloseApplicationsFilter=arke.exe
RestartApplications=no

; Traditional and Simplified Chinese when this Inno Setup has them (they ship as unofficial
; translations in some versions).
#define ZhTw FileExists(CompilerPath + "Languages\ChineseTraditional.isl")
#define ZhCn FileExists(CompilerPath + "Languages\ChineseSimplified.isl")

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
#if ZhTw
Name: "zhtw"; MessagesFile: "compiler:Languages\ChineseTraditional.isl"
#endif
#if ZhCn
Name: "zhcn"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
#endif

[Messages]
; The tasks page starts the install (no Ready page), and its button says so.
en.SelectTasksLabel2=Select the additional tasks you would like Setup to perform while installing [name], then click Install.

[CustomMessages]
en.KeepSettings=Keep your Arke settings, pinned folders and workspaces for next time?
#if ZhTw
zhtw.KeepSettings=要保留 Arke 的設定、釘選的資料夾和工作區，下次安裝時繼續使用嗎？
#endif
#if ZhCn
zhcn.KeepSettings=要保留 Arke 的设置、固定的文件夹和工作区，下次安装时继续使用吗？
#endif

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "arke.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Arke"; Filename: "{app}\arke.exe"
Name: "{autodesktop}\Arke"; Filename: "{app}\arke.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\arke.exe"; Description: "{cm:LaunchProgram,Arke}"; Flags: nowait postinstall skipifsilent runasoriginaluser
Filename: "{app}\arke.exe"; Check: LaunchAfterUpdate; Flags: nowait runasoriginaluser

[Code]
var
  KeepSettings: Boolean;

function HasParam(Name: String): Boolean;
begin
  Result := Pos(Uppercase(Name), Uppercase(GetCmdTail)) > 0;
end;

{ Arke's updater passes /LAUNCH so the new version starts when the silent update is done. }
function LaunchAfterUpdate: Boolean;
begin
  Result := WizardSilent and HasParam('/LAUNCH');
end;

{ Without the Ready page, the page before it starts the install: say so on its button. }
procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpSelectTasks then
    WizardForm.NextButton.Caption := SetupMessage(msgButtonInstall);
end;

{ Ask a running Arke to quit before its exe is replaced. Nothing happens when none is running. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\arke.exe')) then
  begin
    { Returns once Arke is gone (up to 10 s); whatever is left is closed by Windows. }
    Exec(ExpandConstant('{app}\arke.exe'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Params: String;
  Code, I: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    if HasParam('/KEEPSETTINGS') then
      KeepSettings := True
    else if HasParam('/REMOVESETTINGS') then
      KeepSettings := False
    else if UninstallSilent then
      KeepSettings := True
    else
      KeepSettings := MsgBox(CustomMessage('KeepSettings'), mbConfirmation, MB_YESNO) = IDYES;
    Params := '--uninstall --quiet';
    if KeepSettings then
      Params := Params + ' --keep-settings';
    Exec(ExpandConstant('{app}\arke.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, Code);
    { Arke has quit, but Windows may hold its exe a moment longer: wait until it can go. }
    for I := 1 to 30 do
    begin
      if not FileExists(ExpandConstant('{app}\arke.exe')) then
        Break;
      if DeleteFile(ExpandConstant('{app}\arke.exe')) then
        Break;
      Sleep(500);
    end;
  end;
end;
