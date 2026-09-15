; Bộ cài Windows.
;
; Vì sao phải có bộ cài thay cho gói .zip: máy mất điện rồi bật lại phải tự
; chia sẻ tiếp mà không ai ngồi trước máy. Muốn thế thì đường dẫn tới tệp chạy
; phải cố định — gói .zip thường được mở thẳng trong Explorer, chương trình
; chạy từ %TEMP%, và đường dẫn ấy biến mất ngay sau lần khởi động sau. Thêm
; nữa, đăng ký việc đã hẹn và mở tường lửa đều cần quyền quản trị, thứ mà chỉ
; bộ cài mới xin được một lần cho xong.
;
; Dựng bằng makensis, chạy được cả trên macOS:
;   makensis -DVERSION=0.1.19 -DSRC=dist/windows -DOUT=dist/RemoteDesktop-0.1.19-windows-bo-cai.exe installer.nsi

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef SRC
  !define SRC "..\..\dist\windows"
!endif
!ifndef OUT
  !define OUT "..\..\dist\RemoteDesktop-${VERSION}-windows-bo-cai.exe"
!endif

!define APP "Remote Desktop"
!define EXE "remote-desktop.exe"
!define KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\RemoteDesktop"

Name "${APP} ${VERSION}"
OutFile "${OUT}"
InstallDir "$PROGRAMFILES64\RemoteDesktop"
InstallDirRegKey HKLM "Software\RemoteDesktop" "InstallDir"
; Quyền quản trị: cần để ghi vào Program Files, đăng ký việc đã hẹn lúc đăng
; nhập, và mở cổng tường lửa. Xin một lần ở đây, để lúc chạy thì không phải xin.
RequestExecutionLevel admin
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "Bộ cài ${APP}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" ""

!define MUI_ICON "..\icon.ico"
!define MUI_UNICON "..\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Mở ${APP} ngay"

!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "Vietnamese"
!insertmacro MUI_LANGUAGE "English"

; Chương trình đang chạy thì tệp .exe bị khoá, chép đè sẽ hỏng giữa chừng.
!macro DongChuongTrinh
  nsExec::Exec 'taskkill /f /im "${EXE}"'
  Pop $0
!macroend

Section "Phần mềm" SecApp
  SectionIn RO
  !insertmacro DongChuongTrinh
  SetOutPath "$INSTDIR"
  File "${SRC}\${EXE}"
  File "${SRC}\rd-rendezvous.exe"
  File "${SRC}\tu-kiem.ps1"

  WriteRegStr HKLM "Software\RemoteDesktop" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${KEY}" "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKLM "${KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKLM "${KEY}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKLM "${KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${KEY}" "NoRepair" 1
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortCut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\${EXE}"
  CreateShortCut "$SMPROGRAMS\${APP}\Gỡ ${APP}.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

!ifdef TAILSCALE_MSI
Section "Tailscale" SecTailscale
  ; Tailscale là đường nối chính giữa hai máy (địa chỉ 100.x.y.z cố định, không
  ; cần mở cổng router). Kèm sẵn bộ cài chính thức để khỏi bắt người dùng đi tìm.
  ${If} ${FileExists} "$PROGRAMFILES64\Tailscale\tailscale.exe"
    DetailPrint "Tailscale đã có sẵn, bỏ qua."
  ${Else}
    InitPluginsDir
    SetOutPath "$PLUGINSDIR"
    File "/oname=tailscale-setup.msi" "${TAILSCALE_MSI}"
    DetailPrint "Đang cài Tailscale..."
    ExecWait 'msiexec.exe /i "$PLUGINSDIR\tailscale-setup.msi" /passive /norestart' $0
    ; 3010 = cài xong, cần khởi động lại máy — vẫn tính là thành công.
    ${If} $0 != 0
    ${AndIf} $0 != 3010
      MessageBox MB_ICONEXCLAMATION|MB_OK "Không cài được Tailscale (mã lỗi $0). Tải bản cài tại tailscale.com/download rồi cài tay."
    ${EndIf}
    SetOutPath "$INSTDIR"
  ${EndIf}
SectionEnd
!endif

Section "Lối tắt ngoài màn hình" SecDesktop
  CreateShortCut "$DESKTOP\${APP}.lnk" "$INSTDIR\${EXE}"
SectionEnd

Section "Mở cổng trong tường lửa" SecFirewall
  ; Hình ảnh đi bằng QUIC trên UDP. Không mở thì máy bên kia gõ đúng địa chỉ
  ; vẫn nằm chờ, mà Windows lại không hiện hộp thoại hỏi khi chương trình chạy
  ; ngầm lúc chưa ai đăng nhập.
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="${APP}"'
  Pop $0
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="${APP}" dir=in action=allow program="$INSTDIR\${EXE}" enable=yes profile=any'
  Pop $0
SectionEnd

Section "Tự mở lại khi máy có điện" SecAutostart
  ; Gọi ngược lại chính chương trình vừa cài: chỗ dựng mô tả việc đã hẹn nằm
  ; trong mã Rust, có test đi kèm, nên đừng chép lại logic ấy vào đây.
  nsExec::ExecToLog '"$INSTDIR\${EXE}" --tu-khoi-dong on'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION|MB_OK "Không đăng ký được mục tự mở lại. Vào phần mềm, ở thẻ chia sẻ, đánh dấu ô $\"Máy bật lại là tự chia sẻ tiếp$\"."
  ${EndIf}
SectionEnd

LangString DESC_SecApp ${LANG_VIETNAMESE} "Tệp chạy chính, đặt ở nơi cố định để lần sau máy bật lên còn tìm thấy."
LangString DESC_SecDesktop ${LANG_VIETNAMESE} "Thêm biểu tượng ngoài màn hình nền."
LangString DESC_SecFirewall ${LANG_VIETNAMESE} "Cho phép máy khác nối vào. Không có mục này thì kết nối nằm chờ rồi hết giờ."
LangString DESC_SecAutostart ${LANG_VIETNAMESE} "Máy mất điện bật lại, vừa đăng nhập là phần mềm tự lên ở chế độ chia sẻ."
LangString DESC_SecApp ${LANG_ENGLISH} "Main program files."
LangString DESC_SecDesktop ${LANG_ENGLISH} "Add a desktop shortcut."
LangString DESC_SecFirewall ${LANG_ENGLISH} "Allow incoming connections."
LangString DESC_SecAutostart ${LANG_ENGLISH} "Start sharing automatically at logon."

!ifdef TAILSCALE_MSI
LangString DESC_SecTailscale ${LANG_VIETNAMESE} "Cài Tailscale để hai máy thấy nhau qua Internet. Máy đã có Tailscale thì tự bỏ qua."
LangString DESC_SecTailscale ${LANG_ENGLISH} "Install Tailscale so machines can reach each other. Skipped if already installed."
!endif

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} $(DESC_SecApp)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_SecDesktop)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecFirewall} $(DESC_SecFirewall)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} $(DESC_SecAutostart)
!ifdef TAILSCALE_MSI
  !insertmacro MUI_DESCRIPTION_TEXT ${SecTailscale} $(DESC_SecTailscale)
!endif
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  ; Gỡ mục tự khởi động *trước* khi xoá tệp: nhờ chính chương trình gỡ, kẻo để
  ; lại một việc đã hẹn trỏ vào chỗ trống, mỗi lần đăng nhập lại báo lỗi.
  nsExec::Exec '"$INSTDIR\${EXE}" --tu-khoi-dong off'
  Pop $0
  !insertmacro DongChuongTrinh
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${APP}"'
  Pop $0

  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\rd-rendezvous.exe"
  Delete "$INSTDIR\tu-kiem.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  Delete "$SMPROGRAMS\${APP}\Gỡ ${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"
  Delete "$DESKTOP\${APP}.lnk"

  DeleteRegKey HKLM "${KEY}"
  DeleteRegKey HKLM "Software\RemoteDesktop"
SectionEnd
