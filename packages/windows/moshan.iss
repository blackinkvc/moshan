; Moshan 的 Windows 安装程序（Inno Setup 6）
; 由 packages/windows/build.ps1 调用：
;   iscc /DAppVersion=0.1.0 /DSourceDir=..\..\dist\Moshan moshan.iss
;
; 关键要求：升级安装时不能动用户数据。
; 数据在 %APPDATA%\Moshan（配置、日志）与 %USERPROFILE%\Documents\Moshan（资料库），
; 都不在 {app} 里，所以「覆盖安装」天然不会碰到。

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\Moshan"
#endif

#define AppName "Moshan"
#define AppNameCN "墨山中台"
#define AppPublisher "Moshan"
#define AppExe "Moshan.exe"

[Setup]
AppId={{8E2C1F5A-3B7D-4A61-9C2E-1D4F6A8B0C93}
AppName={#AppNameCN}
AppVersion={#AppVersion}
AppVerName={#AppNameCN} {#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppNameCN}
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=Moshan-{#AppVersion}-windows-x64-Setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\{#AppExe}
; 允许用户在安装向导里改目录
DisableDirPage=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务："; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppNameCN}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppNameCN}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; 安装完直接启动，启动器会自己开浏览器到引导页
Filename: "{app}\{#AppExe}"; Description: "立即启动 {#AppNameCN}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; 只清程序目录；用户数据（APPDATA / Documents\Moshan）刻意保留，
; 并在卸载前提示用户「数据仍保留」。
Type: filesandordirs; Name: "{app}"
