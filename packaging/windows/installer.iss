; Bộ cài Windows, dựng bằng Inno Setup 6.
;
; Cài vào thư mục người dùng chứ không phải Program Files: chương trình không
; cần quyền quản trị để chạy, mà bắt nâng quyền lúc cài chỉ làm người dùng ngần
; ngại thêm ở đúng bước đầu tiên.
;
; Dựng:  iscc /DVersion=0.1.0 /DSourceDir=..\..\dist\windows packaging\windows\installer.iss

#ifndef Version
  #define Version "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\windows"
#endif

[Setup]
AppId={{8E1C0B4E-4F2E-4C33-9E4B-4E7E2C1A9D77}
AppName=Remote Desktop
AppVersion={#Version}
AppPublisher=Luong Xuan Hoa
DefaultDirName={localappdata}\RemoteDesktop
DefaultGroupName=Remote Desktop
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=.
OutputBaseFilename=RemoteDesktop-{#Version}-windows-setup
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
; Bộ cài không biểu tượng, không mô tả là một trong những dấu hiệu khiến
; SmartScreen và phần mềm diệt virus nghi ngờ. Đường dẫn tính từ file .iss này.
SetupIconFile=..\icon.ico
UninstallDisplayIcon={app}\remote-desktop.exe
AppPublisherURL=https://github.com/meshop86/RemoteDesktop
AppSupportURL=https://github.com/meshop86/RemoteDesktop/issues
VersionInfoDescription=Bộ cài Remote Desktop
VersionInfoProductName=Remote Desktop
VersionInfoCompany=Luong Xuan Hoa
VersionInfoCopyright=Copyright (c) 2026 Luong Xuan Hoa

[Languages]
Name: "vi"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#SourceDir}\remote-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rd-rendezvous.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Remote Desktop"; Filename: "{app}\remote-desktop.exe"
Name: "{userdesktop}\Remote Desktop"; Filename: "{app}\remote-desktop.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Tạo lối tắt ngoài màn hình"; GroupDescription: "Lối tắt:"

[Run]
Filename: "{app}\remote-desktop.exe"; Description: "Chạy Remote Desktop"; Flags: nowait postinstall skipifsilent

[Registry]
; Cổng UDP mà máy chia sẻ màn hình lắng nghe. Ghi lại để người dùng biết mà mở
; cổng trên router khi nối thẳng qua Internet mà không dùng rendezvous.
Root: HKCU; Subkey: "Software\RemoteDesktop"; ValueType: string; ValueName: "Port"; ValueData: "47823"; Flags: uninsdeletekey
