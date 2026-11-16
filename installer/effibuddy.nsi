; EffiBuddy Windows 安装包（NSIS 3 / Modern UI 2）
;
; 编译：
;   makensis /DVERSION=0.2.0 /DSRCDIR=<仓库根> /DOUTFILE=<输出路径> installer\effibuddy.nsi
;
; 组件页让用户逐项决定：
;   - 开始菜单快捷方式（默认勾选）
;   - 桌面快捷方式（默认勾选）
;   - 固定到任务栏（默认不勾；Win10/11 可能被策略拒绝，失败静默跳过）
; 以及完成页上的“立即启动”。

Unicode true
SetCompressor /SOLID lzma
SetCompressorDictSize 64

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "StrFunc.nsh"

${StrStr}

; ---------------------------------------------------------------- 参数
!ifndef VERSION
  !error "必须通过 /DVERSION=<版本号> 指定版本"
!endif
!ifndef SRCDIR
  !define SRCDIR ".."
!endif
!ifndef OUTFILE
  !define OUTFILE "EffiBuddy-${VERSION}-windows-x86_64-setup.exe"
!endif
; VIProductVersion 只接受四段纯数字，CI 需把 0.2.0-beta.1 之类裁成 0.2.0.0 再传入。
!ifndef VI_VERSION
  !define VI_VERSION "${VERSION}.0"
!endif

!define APPNAME        "EffiBuddy"
!define APPEXE         "effibuddy.exe"
!define PUBLISHER      "EffiBuddy"
!define APPID          "EffiBuddy"
!define RUN_KEY        "Software\Microsoft\Windows\CurrentVersion\Run"
!define RUN_VALUE      "EffiBuddy"
!define UNINST_KEY     "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}"
!define UNINST_KEY_WOW "Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\${APPID}"

Name "${APPNAME} ${VERSION}"
BrandingText "${APPNAME} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\${APPNAME}" "InstallDir"
RequestExecutionLevel admin
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VI_VERSION}"
VIAddVersionKey /LANG=2052 "ProductName"     "${APPNAME}"
VIAddVersionKey /LANG=2052 "FileDescription" "${APPNAME} 安装程序"
VIAddVersionKey /LANG=2052 "FileVersion"     "${VERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion"  "${VERSION}"
VIAddVersionKey /LANG=2052 "CompanyName"     "${PUBLISHER}"
VIAddVersionKey /LANG=2052 "LegalCopyright"  "Copyright (c) ${PUBLISHER}"

; ---------------------------------------------------------------- 页面
!define MUI_ABORTWARNING
!define MUI_ICON   "${NSISDIR}\Contrib\Graphics\Icons\modern-install.ico"
!define MUI_UNICON "${NSISDIR}\Contrib\Graphics\Icons\modern-uninstall.ico"

!insertmacro MUI_PAGE_WELCOME
!ifdef LICENSE_FILE
  !insertmacro MUI_PAGE_LICENSE "${LICENSE_FILE}"
!endif
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APPEXE}"
!define MUI_FINISHPAGE_RUN_TEXT "立即启动 ${APPNAME}"
!define MUI_FINISHPAGE_RUN_NOTCHECKED
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------- 安装
Section "!${APPNAME}（必需）" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  SetOverwrite on
  File "${SRCDIR}\target\release\${APPEXE}"
  File "${SRCDIR}\installer\pin-taskbar.ps1"
  File "${SRCDIR}\installer\unpin-taskbar.ps1"
  File "${SRCDIR}\README.md"
  SetOutPath "$INSTDIR\runtime\msys"
  File /r "${SRCDIR}\runtime\msys\*.*"
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\${APPNAME}" "Version"    "${VERSION}"

  ; 控制面板“程序和功能”条目
  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayName"          "${APPNAME}"
  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayVersion"       "${VERSION}"
  WriteRegStr   HKLM "${UNINST_KEY}" "Publisher"            "${PUBLISHER}"
  WriteRegStr   HKLM "${UNINST_KEY}" "DisplayIcon"          "$INSTDIR\${APPEXE},0"
  WriteRegStr   HKLM "${UNINST_KEY}" "UninstallString"      '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr   HKLM "${UNINST_KEY}" "InstallLocation"      "$INSTDIR"
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify"  1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair"  1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoDisplay" 0
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" "$0"
  ; 32 位视图兼容（部分老卸载工具只读 WOW6432Node）
  WriteRegStr HKLM "${UNINST_KEY_WOW}" "DisplayName"     "${APPNAME}"
  WriteRegStr HKLM "${UNINST_KEY_WOW}" "DisplayVersion"  "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY_WOW}" "Publisher"       "${PUBLISHER}"
  WriteRegStr HKLM "${UNINST_KEY_WOW}" "UninstallString" '"$INSTDIR\uninstall.exe"'
SectionEnd

; 安装到 $PROGRAMFILES64 + 写 HKLM，快捷方式也应落在"所有用户"上下文；
; 否则从标准账户经 UAC 提权安装时，$SMPROGRAMS/$DESKTOP 会指向提权账户的
; 个人目录，普通用户看不到这两个快捷方式。
Section "创建开始菜单快捷方式" SecStartMenu
  SetShellVarContext all
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${APPEXE}" "" "$INSTDIR\${APPEXE}" 0 SW_SHOWNORMAL "" "EffiBuddy"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\卸载 ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

Section "创建桌面快捷方式" SecDesktop
  SetShellVarContext all
  CreateShortCut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${APPEXE}" "" "$INSTDIR\${APPEXE}" 0 SW_SHOWNORMAL "" "EffiBuddy"
SectionEnd

; /o = 默认不勾选，用户自己决定是否固定到任务栏
Section /o "固定到任务栏" SecTaskbar
  ; 先生成临时 lnk，再交给 PowerShell 走 shell verb taskbarpin。
  ; Win11 起该 verb 常被策略拦截，退出码非 0 时只记录日志，不让安装失败。
  InitPluginsDir
  CreateShortCut "$PLUGINSDIR\${APPNAME}.lnk" "$INSTDIR\${APPEXE}" "" "$INSTDIR\${APPEXE}" 0 SW_SHOWNORMAL "" "EffiBuddy"
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\pin-taskbar.ps1" -Shortcut "$PLUGINSDIR\${APPNAME}.lnk"'
  Pop $0
  Pop $1
  DetailPrint "任务栏固定脚本退出码 $0"
  ${If} $0 == "0"
    WriteRegDWORD HKLM "${UNINST_KEY}" "TaskbarPinned" 1
  ${Else}
    DetailPrint "未能固定到任务栏（系统策略或系统不支持），已跳过：$1"
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "程序本体、随包 bash 运行时与卸载器。此项必须安装。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} "在开始菜单创建 ${APPNAME} 快捷方式，并放入卸载入口。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}   "在桌面创建 ${APPNAME} 快捷方式。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SecTaskbar}   "把 ${APPNAME} 固定到任务栏。Windows 10/11 在受限策略下会拒绝固定，失败时自动跳过，不影响安装结果。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------- 生命周期
; 默认勾选状态由 Section /o 声明决定，.onInit 不再动态改选区。
Function .onInit
  ${IfNot} ${RunningX64}
    ; 静默安装时 MessageBox 必须有 /SD，否则会永久阻塞无人值守流程。
    MessageBox MB_ICONSTOP "此安装包仅支持 64 位 Windows。" /SD IDOK
    Abort
  ${EndIf}

  ; 旧版本仍在运行时提示关闭，避免覆盖写失败留下半装状态。
  ; 静默安装跳过这项检查（/S 通常来自自动化或更新器）。
  IfSilent done_check
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${APPEXE}" /NH'
  Pop $0
  Pop $1
  ${If} $0 == "0"
    ${StrStr} $2 $1 "${APPEXE}"
    ${If} $2 != ""
      MessageBox MB_YESNO|MB_ICONQUESTION "${APPNAME} 正在运行，继续安装会覆盖其文件。$\r$\n建议先退出程序。是否继续安装？" /SD IDYES IDNO abort_install
    ${EndIf}
  ${EndIf}
  Goto done_check
  abort_install:
    Abort
  done_check:
FunctionEnd

; ---------------------------------------------------------------- 卸载
Section "Uninstall"
  ; 与安装时保持一致：快捷方式创建/清理都在"所有用户"上下文里。
  SetShellVarContext all
  ; 快捷方式按固定名字清理；用户自己改过名字的副本不由安装器负责。
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\卸载 ${APPNAME}.lnk"
  RMDir  "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"

  ReadRegDWORD $0 HKLM "${UNINST_KEY}" "TaskbarPinned"
  ${If} $0 == 1
    ${If} ${FileExists} "$INSTDIR\unpin-taskbar.ps1"
      nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\unpin-taskbar.ps1"'
      Pop $0
      Pop $1
      DetailPrint "任务栏取消固定退出码 $0"
    ${EndIf}
  ${EndIf}

  Delete "$INSTDIR\${APPEXE}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\pin-taskbar.ps1"
  Delete "$INSTDIR\unpin-taskbar.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /r "$INSTDIR\runtime"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "${UNINST_KEY_WOW}"
  DeleteRegKey HKLM "Software\${APPNAME}"
  DeleteRegValue HKLM "${RUN_KEY}" "${RUN_VALUE}"
SectionEnd

Function un.onInit
  ${IfNot} ${RunningX64}
    Abort
  ${EndIf}
FunctionEnd
