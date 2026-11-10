Unicode true
!ifndef VERSION
 !error "Pass /DVERSION from Cargo.toml"
!endif
!ifndef STAGE
 !error "Pass /DSTAGE with the release file directory"
!endif
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Shishi"
Name "拾事"
OutFile "${OUTPUT}\Shishi-Setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Shishi"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
SetCompressorDictSize 32
ShowInstDetails show
ShowUninstDetails show
BrandingText "拾事 ${VERSION}"
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=2052 "ProductName" "拾事"
VIAddVersionKey /LANG=2052 "FileDescription" "拾事安装程序"
VIAddVersionKey /LANG=2052 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "拾事"
Var Existing
Var RemoveRadio
Var Dialog
Var DesktopCheck
Var DesktopChoice
Var TestMode
Var UpdateMode
Var Params
Var TextFont
Var HeaderFont
!define MUI_ICON "${STAGE}\shishi.ico"
!define MUI_UNICON "${STAGE}\shishi.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\shishi.exe"
!define MUI_CUSTOMFUNCTION_GUIINIT InitTextUI
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.InitTextUI
!define MUI_WELCOMEPAGE_TEXT "此向导将引导您安装拾事。$\r$\n$\r$\n运行数据默认保存在程序目录，可在设置中更改。覆盖安装保留已有数据与目录设置。$\r$\n$\r$\n拾事仍在运行时，可在安装过程中选择强制退出（包括后台）并继续安装。"
!define MUI_PAGE_CUSTOMFUNCTION_SHOW WelcomeTextShow
!insertmacro MUI_PAGE_WELCOME
Page custom Maintenance MaintenanceLeave
!define MUI_PAGE_CUSTOMFUNCTION_SHOW DirectoryTextShow
!insertmacro MUI_PAGE_DIRECTORY
Page custom Options OptionsLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishTextShow
!insertmacro MUI_PAGE_FINISH
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmTextShow
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.FinishTextShow
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "SimpChinese"

; Keep the original dialog font/units: SetFont would enlarge the window.
!macro HeaderRect control x y w h
 System::Call '*(i ${x}, i ${y}, i ${w}, i ${h}) p.r0'
 System::Call 'user32::MapDialogRect(p $HWNDPARENT, p r0)'
 System::Call '*$0(i.r1, i.r2, i.r3, i.r4)'
 System::Free $0
 System::Call 'user32::SetWindowPos(p ${control}, p 0, i r1, i r2, i r3, i r4, i 0x14)'
!macroend

!macro TextUI prefix
Function ${prefix}InitTextUI
 CreateFont $TextFont "Microsoft YaHei UI" 9
 CreateFont $HeaderFont "Microsoft YaHei UI" 9 700
 SendMessage $mui.Header.Text ${WM_SETFONT} $HeaderFont 0
 SendMessage $mui.Header.SubText ${WM_SETFONT} $TextFont 0
 !insertmacro HeaderRect $mui.Header.Text 10 4 280 12
 !insertmacro HeaderRect $mui.Header.SubText 15 18 275 15
FunctionEnd

Function ${prefix}FinishTextShow
 SendMessage $mui.FinishPage.Text ${WM_SETFONT} $TextFont 0
FunctionEnd
!macroend
!insertmacro TextUI ""
!insertmacro TextUI "un."

Function WelcomeTextShow
 SendMessage $mui.WelcomePage.Text ${WM_SETFONT} $TextFont 0
FunctionEnd

Function DirectoryTextShow
 SendMessage $mui.DirectoryPage.Text ${WM_SETFONT} $TextFont 0
FunctionEnd

Function un.ConfirmTextShow
 SendMessage $mui.UnConfirmPage.Text ${WM_SETFONT} $TextFont 0
FunctionEnd

; Writable means no running process maps the file. $0 = 1 when free.
!macro Free path
 StrCpy $0 1
 IfFileExists "${path}" 0 +6
 System::Call 'kernel32::CreateFileW(w "${path}", i 0x40000000, i 0, p 0, i 3, i 0, p 0) p.r1'
 IntCmp $1 -1 0 +3 +3
 StrCpy $0 0
 Goto +2
 System::Call 'kernel32::CloseHandle(p r1)'
!macroend

; Ask before closing this installation; silent automatic updates already
; handed off. Restart Manager targets these files, never all shishi.exe's.
Function WaitFree
 !insertmacro Free "$INSTDIR\shishi.exe"
 ${If} $0 == 1
  Return
 ${EndIf}
 IfSilent silent ask
 silent:
 ${If} $UpdateMode == 1
  Goto close_running
 ${EndIf}
 Goto give_up
 ask:
 MessageBox MB_YESNO|MB_DEFBUTTON2|MB_ICONQUESTION "拾事仍在运行。是否强制退出拾事（包括后台）并继续安装？$\r$\n$\r$\n正在执行的任务会停止，已保存的数据保留。" IDYES close_running
 Goto give_up
 close_running:
 DetailPrint "正在退出当前安装目录的拾事及后台..."
 System::Call 'rstrtmgr::RmStartSession(*i .r1, i 0, w .r2) i.r0'
 ${If} $0 == 0
  System::Call '*(&w${NSIS_MAX_STRLEN} "$INSTDIR\shishi.exe") p.r3'
  System::Call '*(p r3) p.r5'
  System::Call 'rstrtmgr::RmRegisterResources(i r1, i 1, p r5, i 0, p 0, i 0, p 0) i.r0'
  System::Free $5
  System::Free $3
  ${If} $0 == 0
   System::Call 'rstrtmgr::RmShutdown(i r1, i 1, p 0) i.r0'
  ${EndIf}
  System::Call 'rstrtmgr::RmEndSession(i r1)'
 ${EndIf}
 !insertmacro Free "$INSTDIR\shishi.exe"
 ${If} $0 == 1
  Return
 ${EndIf}
 DetailPrint "拾事未能退出，安装已停止。"
 MessageBox MB_ICONEXCLAMATION "拾事未能退出，安装已停止。请结束占用后重新运行安装包。" /SD IDOK
 give_up:
 SetErrorLevel 2
 Abort
FunctionEnd

Function .onInit
 StrCpy $DesktopChoice ${BST_CHECKED}
 ${GetParameters} $Params
 ClearErrors
 ${GetOptions} $Params "/UPDATE" $0
 ${If} ${Errors}
  StrCpy $UpdateMode 0
 ${Else}
  StrCpy $UpdateMode 1
 ${EndIf}
 ClearErrors
 ${GetOptions} $Params "/TEST" $0
 ${If} ${Errors}
  StrCpy $TestMode 0
 ${Else}
  StrCpy $TestMode 1
 ${EndIf}
 StrCpy $Existing ""
 IfFileExists "$INSTDIR\shishi.exe" 0 +2
 StrCpy $Existing $INSTDIR
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
 SendMessage $0 ${WM_SETFONT} $TextFont 0
 ${NSD_CreateRadioButton} 0 48u 100% 18u "覆盖安装（保留事项、配置、报表和日志）"
 Pop $0
 SendMessage $0 ${WM_SETFONT} $TextFont 0
 ${NSD_Check} $0
 ${NSD_CreateRadioButton} 0 78u 100% 18u "卸载拾事（保留数据与目录设置）"
 Pop $RemoveRadio
 SendMessage $RemoveRadio ${WM_SETFONT} $TextFont 0
 nsDialogs::Show
FunctionEnd

Function MaintenanceLeave
 ${NSD_GetState} $RemoveRadio $0
 ${If} $0 == ${BST_CHECKED}
  IfFileExists "$Existing\Uninstall.exe" 0 +3
  ExecWait '"$Existing\Uninstall.exe" _?=$Existing'
  Quit
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
 SendMessage $DesktopCheck ${WM_SETFONT} $TextFont 0
 ${NSD_SetState} $DesktopCheck $DesktopChoice
 ${NSD_CreateLabel} 0 40u 100% 90u "安装后可在“消息来源”中安装插件并配置所需文件和目录。支持插件自动查找。$\r$\n$\r$\n默认数据目录：$INSTDIR\data$\r$\n若已更改目录，继续使用原设置。覆盖安装与卸载保留数据。"
 Pop $0
 SendMessage $0 ${WM_SETFONT} $TextFont 0
 nsDialogs::Show
FunctionEnd

Function OptionsLeave
 ${NSD_GetState} $DesktopCheck $DesktopChoice
FunctionEnd

Section "拾事" SEC_MAIN
 SetShellVarContext current
 SetOutPath "$INSTDIR"
 Call WaitFree
 ; A skipped extraction must not publish a successful upgrade.
 ClearErrors
 File "${STAGE}\shishi.exe"
 File "${STAGE}\WebView2Loader.dll"
 File "${STAGE}\README.txt"
 File "${STAGE}\CLI.md"
 File "${STAGE}\LICENSE"
 File "${STAGE}\THIRD_PARTY_NOTICES.md"
 File "${STAGE}\LICENSE-WRY-MIT"
 File "${STAGE}\LICENSE-WRY-APACHE"
 SetOutPath "$INSTDIR\skills\shishi-cli"
 File "${STAGE}\SKILL.md"
 SetOutPath "$INSTDIR"
 ${If} ${Errors}
  MessageBox MB_ICONEXCLAMATION "程序文件未完整写入，安装未完成。请关闭占用安装目录文件的程序后重新安装；现有数据已保留。" /SD IDOK
  SetErrorLevel 1
  Abort
 ${EndIf}
 CreateDirectory "$INSTDIR\data"
 WriteUninstaller "$INSTDIR\Uninstall.exe"
 FileOpen $0 "$INSTDIR\install-info.txt" w
 FileWrite $0 "Shishi ${VERSION}$\r$\n$INSTDIR$\r$\n"
 FileClose $0
 ${If} $TestMode != 1
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "拾事"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "拾事"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\shishi.exe"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${If} $UpdateMode != 1
   ${If} $DesktopChoice == ${BST_CHECKED}
    CreateShortCut "$DESKTOP\拾事.lnk" "$INSTDIR\shishi.exe"
   ${EndIf}
   CreateShortCut "$SMPROGRAMS\拾事.lnk" "$INSTDIR\shishi.exe"
  ${EndIf}
 ${EndIf}
SectionEnd

Function un.onInit
 ${GetParameters} $Params
 ClearErrors
 ${GetOptions} $Params "/TEST" $0
 ${If} ${Errors}
  StrCpy $TestMode 0
 ${Else}
  StrCpy $TestMode 1
 ${EndIf}
FunctionEnd

Section "Uninstall"
 SetShellVarContext current
 IfFileExists "$INSTDIR\install-info.txt" +3
 MessageBox MB_ICONEXCLAMATION "该目录没有安装标识，未删除任何文件。" /SD IDOK
 Abort
 !insertmacro Free "$INSTDIR\shishi.exe"
 ${If} $0 != 1
  MessageBox MB_ICONEXCLAMATION "请退出拾事图形端，并用 shishi.exe backend stop 关闭 CLI 后台后重新卸载；叉掉窗口仅隐藏。" /SD IDOK
  SetErrorLevel 2
  Abort
 ${EndIf}
 Delete "$INSTDIR\shishi.exe"
 Delete "$INSTDIR\WebView2Loader.dll"
 Delete "$INSTDIR\README.txt"
 Delete "$INSTDIR\CLI.md"
 Delete "$INSTDIR\skills\shishi-cli\SKILL.md"
 RMDir "$INSTDIR\skills\shishi-cli"
 RMDir "$INSTDIR\skills"
 Delete "$INSTDIR\LICENSE"
 Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
 Delete "$INSTDIR\LICENSE-WRY-MIT"
 Delete "$INSTDIR\LICENSE-WRY-APACHE"
 Delete "$INSTDIR\install-info.txt"
 Delete "$INSTDIR\Uninstall.exe"
 ${If} $TestMode != 1
  ReadRegStr $0 HKCU "${UNINSTALL_KEY}" "InstallLocation"
  ${If} $0 == $INSTDIR
   DeleteRegKey HKCU "${UNINSTALL_KEY}"
   Delete "$DESKTOP\拾事.lnk"
   Delete "$SMPROGRAMS\拾事.lnk"
  ${EndIf}
 ${EndIf}
 DetailPrint "已保留运行数据与数据目录设置；配置过的外部数据目录也不会删除。"
SectionEnd
