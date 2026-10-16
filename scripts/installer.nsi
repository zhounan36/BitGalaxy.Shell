; ============================================================================
; BitGalaxy Shell 合包安装程序（NSIS 3.x / MUI2）
; 构建入口：scripts/build-installer.mjs（产物汇总到 target/pack 后编译本脚本）
; 编译：makensis -DVERSION=x.y.z installer.nsi
;
; 布局（安装后 $INSTDIR，与运行期解析约定一致）：
;   bgshell-desktop.exe      核心程序（必装；托盘 + AI 基座 7777）
;   bgshell-ssh.exe          SSH 终端组件（可选）
;   bgshell-sql.exe          SQL 客户端组件（可选，含 drivers/）
;   drivers/                 SQL 驱动宿主（DriverManager::bundled 按 exe 同级解析）
;   bgshell-serial.exe       串口调试组件（可选）
;   resources/pet-packs/     PET 桌宠内置动作包（缺失时桌宠降级无精灵形态）
;
; 特性：per-user 安装（无管理员权限）；组件可选安装/卸载；
;       WebView2 运行时检测（缺失则用内置 bootstrapper 静默安装）；
;       升级覆盖前自动结束相关进程；卸载可选清理用户数据。
; ============================================================================

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

; ── 编译期传入（build-installer.mjs 负责 -D 注入）─────────────────────────
!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef PACKDIR
  !define PACKDIR "target\pack"
!endif
!ifndef OUTFILE
  !define OUTFILE "dist\BitGalaxy-Shell_x64-setup.exe"
!endif
; NO_PET：slim 变体（不带 PET 桌宠资源，包体 ~20MB；PET Section/描述一并裁掉）

!define PRODUCTNAME "BitGalaxy Shell"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\BitGalaxy Shell"
!define DATADIRNAME "bitgalaxy-shell"

Name "${PRODUCTNAME} ${VERSION}"
OutFile "${OUTFILE}"
Unicode true
; per-user 安装：无需管理员，与 tauri v2 默认 currentUser 一致
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\BitGalaxy Shell"
; 老版本装过则沿用其目录
InstallDirRegKey HKCU "Software\BitGalaxy Shell" "InstallDir"
ShowInstDetails show
ShowUninstDetails show
SetCompressor /SOLID lzma

; ── MUI 页面 ─────────────────────────────────────────────────────────────
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC
; 安装包文件图标：主程序品牌图标（-NOCD 模式下相对 cwd 解析）
!define MUI_ICON "apps\desktop\src-tauri\icons\icon.ico"
!define MUI_UNICON "apps\desktop\src-tauri\icons\icon.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${NSISDIR}\Docs\Modern UI\License.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ── 安装前：结束旧进程（升级/覆盖场景，杀不掉不阻塞——可能未装）──────────
Function .onInit
  nsExec::Exec 'taskkill /f /im bgshell-desktop.exe /im bgshell-ssh.exe /im bgshell-sql.exe /im bgshell-serial.exe'
  Pop $0
FunctionEnd

Function un.onInit
  nsExec::Exec 'taskkill /f /im bgshell-desktop.exe /im bgshell-ssh.exe /im bgshell-sql.exe /im bgshell-serial.exe'
  Pop $0
FunctionEnd

; ── WebView2 运行时检测（EdgeUpdate 注册表，HKLM/HKCU 任一有版本即视为已装）─
Function CheckWebView2
  StrCpy $1 1
  ClearErrors
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${If} ${Errors}
    ClearErrors
    ReadRegStr $0 HKCU "SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    ${If} ${Errors}
      StrCpy $1 0
    ${EndIf}
  ${EndIf}
FunctionEnd

; ── 安装 Section ─────────────────────────────────────────────────────────

; 核心程序（必装；名称前缀 "-" = 组件页不显示该项）
Section "-核心程序" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${PACKDIR}\bgshell-desktop.exe"
  File "${PACKDIR}\MicrosoftEdgeWebview2Setup.exe"

  ; WebView2 运行时：缺失则 bootstrapper 静默安装（装完即删，不留缓存）
  Call CheckWebView2
  ${If} $1 == 0
    DetailPrint "未检测到 WebView2 运行时，正在安装……"
    ExecWait '"$INSTDIR\MicrosoftEdgeWebview2Setup.exe" /silent /install' $2
    ${If} $2 != 0
      DetailPrint "WebView2 安装返回码 $2（可能已有其他版本正在安装）"
    ${EndIf}
  ${EndIf}
  Delete "$INSTDIR\MicrosoftEdgeWebview2Setup.exe"

  ; 开始菜单（主程序 + 卸载入口；桌面快捷方式按所选组件在各组件节创建）
  CreateDirectory "$SMPROGRAMS\BitGalaxy Shell"
  CreateShortcut "$SMPROGRAMS\BitGalaxy Shell\BitGalaxy Shell.lnk" "$INSTDIR\bgshell-desktop.exe"
  CreateShortcut "$SMPROGRAMS\BitGalaxy Shell\卸载 BitGalaxy Shell.lnk" "$INSTDIR\uninstall.exe"

  ; 注册表：安装目录记忆（升级沿用）+ 卸载项
  WriteRegStr HKCU "Software\BitGalaxy Shell" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\bgshell-desktop.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" $0

  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "SSH 终端" SecSSH
  SetOutPath "$INSTDIR"
  File "${PACKDIR}\bgshell-ssh.exe"
  CreateShortcut "$SMPROGRAMS\BitGalaxy Shell\SSH 终端.lnk" "$INSTDIR\bgshell-ssh.exe"
  CreateShortcut "$DESKTOP\SSH 终端.lnk" "$INSTDIR\bgshell-ssh.exe"
SectionEnd

Section "SQL 客户端（含数据库驱动）" SecSQL
  SetOutPath "$INSTDIR"
  File "${PACKDIR}\bgshell-sql.exe"
  CreateShortcut "$SMPROGRAMS\BitGalaxy Shell\SQL 客户端.lnk" "$INSTDIR\bgshell-sql.exe"
  CreateShortcut "$DESKTOP\SQL 客户端.lnk" "$INSTDIR\bgshell-sql.exe"
  SetOutPath "$INSTDIR\drivers"
  File /r "${PACKDIR}\drivers\*.*"
SectionEnd

Section "串口调试" SecSerial
  SetOutPath "$INSTDIR"
  File "${PACKDIR}\bgshell-serial.exe"
  CreateShortcut "$SMPROGRAMS\BitGalaxy Shell\串口调试.lnk" "$INSTDIR\bgshell-serial.exe"
  CreateShortcut "$DESKTOP\串口调试.lnk" "$INSTDIR\bgshell-serial.exe"
SectionEnd

!ifndef NO_PET
Section "PET 桌宠资源（约 90MB）" SecPET
  SetOutPath "$INSTDIR\resources\pet-packs"
  File /r "${PACKDIR}\resources\pet-packs\*.*"
SectionEnd
!endif

; ── 组件描述（鼠标悬停提示，双语）────────────────────────────────────────
LangString DESC_SSH ${LANG_SIMPCHINESE} "远程终端 / SFTP 文件管理 / 运行状态客户端"
LangString DESC_SQL ${LANG_SIMPCHINESE} "MySQL / PostgreSQL / SQLite 客户端（内置数据库驱动）"
LangString DESC_Serial ${LANG_SIMPCHINESE} "本机 COM 口双工调试（DTR/RTS/Break/热插拔）"
LangString DESC_SSH ${LANG_ENGLISH} "Remote terminal / SFTP file manager / runtime client"
LangString DESC_SQL ${LANG_ENGLISH} "MySQL / PostgreSQL / SQLite client (bundled database drivers)"
LangString DESC_Serial ${LANG_ENGLISH} "Local COM port duplex debugging (DTR/RTS/Break/hotplug)"
!ifndef NO_PET
LangString DESC_PET ${LANG_SIMPCHINESE} "桌宠内置动作包（不装则桌宠以无精灵形态运行，聊天/菜单不受影响）"
LangString DESC_PET ${LANG_ENGLISH} "Desktop pet built-in animation packs (skipped = pet runs without sprites)"
!endif

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecSSH} $(DESC_SSH)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecSQL} $(DESC_SQL)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecSerial} $(DESC_Serial)
  !ifndef NO_PET
  !insertmacro MUI_DESCRIPTION_TEXT ${SecPET} $(DESC_PET)
  !endif
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ── 卸载 Section（组件按已安装文件存在性生效；文件不在则本节空跑）────────

Section "un.SSH 终端" unSecSSH
  IfFileExists "$INSTDIR\bgshell-ssh.exe" 0 skip_ssh
    Delete "$INSTDIR\bgshell-ssh.exe"
    Delete "$SMPROGRAMS\BitGalaxy Shell\SSH 终端.lnk"
    Delete "$DESKTOP\SSH 终端.lnk"
  skip_ssh:
SectionEnd

Section "un.SQL 客户端（含数据库驱动）" unSecSQL
  IfFileExists "$INSTDIR\bgshell-sql.exe" 0 skip_sql
    Delete "$INSTDIR\bgshell-sql.exe"
    Delete "$SMPROGRAMS\BitGalaxy Shell\SQL 客户端.lnk"
    Delete "$DESKTOP\SQL 客户端.lnk"
  skip_sql:
  IfFileExists "$INSTDIR\drivers\*.*" 0 skip_sqldrv
    RMDir /r "$INSTDIR\drivers"
  skip_sqldrv:
SectionEnd

Section "un.串口调试" unSecSerial
  IfFileExists "$INSTDIR\bgshell-serial.exe" 0 skip_serial
    Delete "$INSTDIR\bgshell-serial.exe"
    Delete "$SMPROGRAMS\BitGalaxy Shell\串口调试.lnk"
    Delete "$DESKTOP\串口调试.lnk"
  skip_serial:
SectionEnd

Section "un.PET 桌宠资源" unSecPET
  IfFileExists "$INSTDIR\resources\pet-packs\*.*" 0 skip_pet
    RMDir /r "$INSTDIR\resources"
  skip_pet:
SectionEnd

; 核心程序（卸载必选）
Section "un.核心程序" unSecCore
  SectionIn RO
  Delete "$INSTDIR\bgshell-desktop.exe"
  Delete "$INSTDIR\MicrosoftEdgeWebview2Setup.exe"
  Delete "$SMPROGRAMS\BitGalaxy Shell\BitGalaxy Shell.lnk"
  Delete "$SMPROGRAMS\BitGalaxy Shell\卸载 BitGalaxy Shell.lnk"
  RMDir "$SMPROGRAMS\BitGalaxy Shell"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "Software\BitGalaxy Shell"
  Delete "$INSTDIR\uninstall.exe"
SectionEnd

; 残留清理：用户数据（默认不勾，防误删）
Section /o "un.删除用户数据（主机/对话/连接记录）" unSecData
  IfFileExists "$APPDATA\${DATADIRNAME}\*.*" 0 skip_data
    RMDir /r "$APPDATA\${DATADIRNAME}"
  skip_data:
SectionEnd

Function un.onUnInstSuccess
  ; 组件都卸完了目录为空 → 删目录；还剩文件（有组件未勾卸载）则保留
  IfFileExists "$INSTDIR\*.*" +2 0
  RMDir "$INSTDIR"
FunctionEnd
