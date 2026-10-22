Unicode true
!ifndef VERSION
 !error "Pass /DVERSION from Cargo.toml"
!endif
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
Name "拾事"
OutFile "${OUTPUT}\Shishi-Setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Shishi"
InstallDirRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show
BrandingText "拾事 ${VERSION}"
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=2052 "ProductName" "拾事"
VIAddVersionKey /LANG=2052 "FileDescription" "拾事安装程序"
VIAddVersionKey /LANG=2052 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "拾事"
Var Existing
Var UpgradeRadio
Var RemoveRadio
Var Dialog
Var DesktopCheck
Var DesktopChoice
Var TestMode
Var Params
Var UpdateMode
Var ProcessShell
!define MUI_ICON "${SOURCE}\assets\shishi.ico"
!define MUI_UNICON "${SOURCE}\assets\shishi.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\shishi.exe"
!define MUI_WELCOMEPAGE_TEXT "此向导将引导您安装拾事。$\r$\n$\r$\n运行数据默认保存在程序目录，可在设置中更改。覆盖安装保留已有数据与目录设置。$\r$\n$\r$\n检测到该目录的拾事正在运行时，会询问是否强制退出并继续安装。"
!insertmacro MUI_PAGE_WELCOME
Page custom Maintenance MaintenanceLeave
!insertmacro MUI_PAGE_DIRECTORY
Page custom Options OptionsLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "SimpChinese"
Function .onInit
 SetRegView 64
 StrCpy $DesktopChoice ${BST_CHECKED}
 ${GetParameters} $Params
 ClearErrors
 ${GetOptions} $Params "/UPDATE" $UpdateMode
 ${IfNot} ${Errors}
  StrCpy $UpdateMode "1"
 ${Else}
  StrCpy $UpdateMode "0"
 ${EndIf}
 ClearErrors
 ${GetOptions} $Params "/TEST" $TestMode
 ${IfNot} ${Errors}
  StrCpy $TestMode "1"
 ${Else}
  StrCpy $TestMode "0"
 ${EndIf}
 ${If} $TestMode != "1"
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "InstallLocation"
  ${If} $0 != ""
   StrCpy $INSTDIR $0
  ${EndIf}
 ${EndIf}
 StrCpy $Existing ""
 IfFileExists "$INSTDIR\shishi.exe" 0 done_init
 StrCpy $Existing $INSTDIR
 done_init:
FunctionEnd
Function Maintenance
 ${If} $Existing == ""
  Abort
 ${EndIf}
 !insertmacro MUI_HEADER_TEXT "已安装拾事" "选择覆盖安装或卸载已有版本。"
 nsDialogs::Create 1018
 Pop $Dialog
 ${NSD_CreateLabel} 0 0 100% 32u "检测到安装位置：$\r$\n$Existing"
 Pop $0
 ${NSD_CreateRadioButton} 0 48u 100% 18u "覆盖安装（保留事项、配置、报表和日志）"
 Pop $UpgradeRadio
 ${NSD_Check} $UpgradeRadio
 ${NSD_CreateRadioButton} 0 78u 100% 18u "卸载拾事（保留数据与目录设置）"
 Pop $RemoveRadio
 nsDialogs::Show
FunctionEnd
Function MaintenanceLeave
 ${NSD_GetState} $RemoveRadio $0
 ${If} $0 == ${BST_CHECKED}
  IfFileExists "$Existing\Uninstall.exe" 0 no_uninstaller
  FileOpen $0 "$Existing\install-info.txt" r
  FileRead $0 $1
  FileClose $0
  StrCpy $1 $1 12
  ${If} $1 == "Shishi 0.1.0"
   ExecWait '"$Existing\Uninstall.exe" --uninstall "$Existing"'
  ${Else}
   ExecWait '"$Existing\Uninstall.exe" _?=$Existing'
  ${EndIf}
  Quit
  no_uninstaller:
  MessageBox MB_ICONEXCLAMATION "未找到卸载程序，请先覆盖安装以修复。"
  Abort
 ${EndIf}
FunctionEnd
Function Options
 !insertmacro MUI_HEADER_TEXT "安装选项" "选择快捷方式，应用路径将在首次启动时自动检测。"
 nsDialogs::Create 1018
 Pop $Dialog
 ${NSD_CreateCheckbox} 0 6u 100% 18u "创建桌面快捷方式"
 Pop $DesktopCheck
 ${NSD_SetState} $DesktopCheck $DesktopChoice
 ${NSD_CreateLabel} 0 40u 100% 65u "安装后可在“消息来源”中安装插件并配置所需文件和目录。支持插件自动查找。$\r$\n$\r$\n默认数据目录：$INSTDIR\data$\r$\n若已更改目录，继续使用原设置。覆盖安装与卸载保留数据。"
 Pop $0
 nsDialogs::Show
FunctionEnd
Function OptionsLeave
 ${NSD_GetState} $DesktopCheck $DesktopChoice
FunctionEnd
Section "拾事" SEC_MAIN
 SetShellVarContext current
 SetOutPath "$INSTDIR"
 IfFileExists "$INSTDIR\shishi.exe" 0 copy_app
 InitPluginsDir
 File /oname=$PLUGINSDIR\shishi-install-process.ps1 "${SOURCE}\installer\process.ps1"
 File /oname=$PLUGINSDIR\shishi-prompt.exe "${SOURCE}\target\release\shishi.exe"
 File /oname=$PLUGINSDIR\WebView2Loader.dll "${LOADER}"
 StrCpy $ProcessShell "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe"
 ${If} ${RunningX64}
  StrCpy $ProcessShell "$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe"
 ${EndIf}
 nsExec::ExecToStack '"$ProcessShell" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\shishi-install-process.ps1" -InstallDir "$INSTDIR" -Action Probe'
 Pop $0
 Pop $1
 ${If} $0 == 10
  IfSilent cancel_running_upgrade
  nsExec::ExecToStack '"$PLUGINSDIR\shishi-prompt.exe" --installer-confirm $HWNDPARENT'
  Pop $0
  Pop $1
  ${If} $0 == 6
   Goto stop_for_upgrade
  ${ElseIf} $0 != 7
   MessageBox MB_ICONEXCLAMATION "无法打开运行中程序的确认窗口。请退出拾事后重试。" /SD IDOK
  ${EndIf}
  cancel_running_upgrade:
  Abort
  stop_for_upgrade:
  nsExec::ExecToStack '"$ProcessShell" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\shishi-install-process.ps1" -InstallDir "$INSTDIR" -Action Stop'
  Pop $0
  Pop $1
  ${If} $0 != 0
   MessageBox MB_ICONEXCLAMATION "无法退出当前程序，请手动退出拾事后重试。$\r$\n$1" /SD IDOK
   Abort
  ${EndIf}
 ${ElseIf} $0 != 0
  MessageBox MB_ICONEXCLAMATION "无法检查运行中的拾事，请手动退出后重试。$\r$\n$1" /SD IDOK
  Abort
 ${EndIf}
 System::Call 'kernel32::CreateFileW(w "$INSTDIR\shishi.exe", i 0x40000000, i 0, p 0, i 3, i 0, p 0) p.r0'
 ${If} $0 == -1
  MessageBox MB_ICONEXCLAMATION "无法更新程序文件，请先右键拾事托盘图标选择“退出”（叉掉窗口仅隐藏），并确认安装目录可以写入。" /SD IDOK
  Abort
 ${EndIf}
 System::Call 'kernel32::CloseHandle(p r0)'
 copy_app:
 File "${SOURCE}\target\release\shishi.exe"
 File "${LOADER}"
 File "${SOURCE}\README.txt"
 File "${SOURCE}\LICENSE"
 File "${SOURCE}\THIRD_PARTY_NOTICES.md"
 File /oname=LICENSE-WRY-MIT "${SOURCE}\vendor\wry\LICENSE-MIT"
 File /oname=LICENSE-WRY-APACHE "${SOURCE}\vendor\wry\LICENSE-APACHE"
 CreateDirectory "$INSTDIR\data"
 WriteUninstaller "$INSTDIR\Uninstall.exe"
 FileOpen $0 "$INSTDIR\install-info.txt" w
 FileWrite $0 "Shishi ${VERSION}$\r$\n$INSTDIR$\r$\n"
 FileClose $0
 ${If} $TestMode != "1"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "DisplayName" "拾事"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "UninstallString" '$\"$INSTDIR\Uninstall.exe$\"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "DisplayIcon" "$INSTDIR\shishi.exe"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "NoRepair" 1
  ${If} $DesktopChoice == ${BST_CHECKED}
  ${AndIf} $UpdateMode != "1"
   CreateShortCut "$DESKTOP\拾事.lnk" "$INSTDIR\shishi.exe"
  ${EndIf}
  ${If} $UpdateMode != "1"
   CreateShortCut "$SMPROGRAMS\拾事.lnk" "$INSTDIR\shishi.exe"
  ${EndIf}
 ${EndIf}
SectionEnd
Function un.onInit
 SetRegView 64
 ${GetParameters} $Params
 ClearErrors
 ${GetOptions} $Params "/TEST" $TestMode
 ${IfNot} ${Errors}
  StrCpy $TestMode "1"
 ${Else}
  StrCpy $TestMode "0"
 ${EndIf}
FunctionEnd
Section "Uninstall"
 SetShellVarContext current
 IfFileExists "$INSTDIR\install-info.txt" 0 uninstall_abort
 IfFileExists "$INSTDIR\shishi.exe" 0 delete_app
 System::Call 'kernel32::CreateFileW(w "$INSTDIR\shishi.exe", i 0x40000000, i 0, p 0, i 3, i 0, p 0) p.r0'
 ${If} $0 == -1
  MessageBox MB_ICONEXCLAMATION "请右键拾事托盘图标选择“退出”后重新卸载；叉掉窗口仅隐藏。" /SD IDOK
  Abort
 ${EndIf}
 System::Call 'kernel32::CloseHandle(p r0)'
 delete_app:
 Delete "$INSTDIR\shishi.exe"
 Delete "$INSTDIR\WebView2Loader.dll"
 Delete "$INSTDIR\README.txt"
 Delete "$INSTDIR\LICENSE"
 Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
 Delete "$INSTDIR\LICENSE-WRY-MIT"
 Delete "$INSTDIR\LICENSE-WRY-APACHE"
 Delete "$INSTDIR\plugins\json-file\shishi_json_source.dll"
 Delete "$INSTDIR\plugins\json-file\plugin.json"
 RMDir "$INSTDIR\plugins\json-file"
 RMDir "$INSTDIR\plugins"
 Delete "$INSTDIR\install-info.txt"
 Delete "$INSTDIR\Uninstall.exe"
 ${If} $TestMode != "1"
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi" "InstallLocation"
  ${If} $0 == $INSTDIR
   DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi"
   Delete "$DESKTOP\拾事.lnk"
   Delete "$SMPROGRAMS\拾事.lnk"
  ${EndIf}
 ${EndIf}
 DetailPrint "已保留运行数据与数据目录设置；配置过的外部数据目录也不会删除。"
 Goto uninstall_done
 uninstall_abort:
 MessageBox MB_ICONEXCLAMATION "该目录没有安装标识，未删除任何文件。" /SD IDOK
 Abort
 uninstall_done:
SectionEnd
