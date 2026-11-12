; AmberSight installer for Windows (NSIS, Modern UI 2).
;
; Built by scripts/package-windows-installer.sh from the folder that
; scripts/package.ps1 makes (ambersight.exe, the AI runtime's DLLs,
; config.example.toml, README.md, LICENSE, THIRD-PARTY-NOTICES.txt):
;   makensis -DVERSION=0.1.0 -DVIVERSION=0.1.0.0 -DSRCDIR=<folder>
;            -DICON=<ambersight.ico> -DOUTFILE=<setup.exe> ambersight.nsi
;
; The installer only puts the files in place. The program does the rest
; itself with `ambersight setup` (one source of truth for every way of
; installing): settings and recordings in %ProgramData%\AmberSight, the
; Windows service, the firewall rule and the Start menu and desktop
; shortcuts. The uninstaller runs `ambersight setup --uninstall`, which keeps
; the settings and recordings.

Unicode true
!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

; Names of its own ("AmberSight NVR"): another program called AmberSight
; may already have a folder or registry key named just "AmberSight".
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AmberSight NVR"
!define APP_KEY "Software\AmberSight NVR"
; Beside the installed files: marks the folder as this installer's.
!define MARKER "ambersight-installer.txt"
; FILES: the generated list of the package's files (macros INSTALL_FILES
; and UNINSTALL_FILES), so the uninstaller removes exactly those.
!include "${FILES}"

Name "AmberSight"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\AmberSight NVR"
InstallDirRegKey HKLM "${APP_KEY}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "AmberSight ${VERSION}"

VIProductVersion "${VIVERSION}"
VIAddVersionKey "ProductName" "AmberSight"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "AmberSight"
VIAddVersionKey "FileDescription" "AmberSight setup"
VIAddVersionKey "LegalCopyright" "Copyright (c) 2026 huskerminion. PolyForm Noncommercial License 1.0.0"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "This installs AmberSight ${VERSION}, a camera recorder you use from a web browser.$\r$\n$\r$\nIt runs as a background service that starts with Windows. Your settings and recordings are kept in $\"ProgramData\AmberSight$\" and stay there when AmberSight is updated or removed.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_TEXT "AmberSight is installed and running. The AmberSight shortcut on the desktop and in the Start menu opens it in the web browser."
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open AmberSight"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenAmberSight

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${SRCDIR}/LICENSE"
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE CheckFolder
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "AmberSight needs 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  SetShellVarContext all
FunctionEnd

; A folder that holds a program this installer did not put there (another
; program called AmberSight): never install over it.
!macro FOREIGN_CHECK
  ${If} ${FileExists} "$INSTDIR\ambersight.exe"
  ${AndIfNot} ${FileExists} "$INSTDIR\${MARKER}"
    MessageBox MB_ICONSTOP "$INSTDIR already holds a program called AmberSight that this installer did not install.$\r$\n$\r$\nChoose another folder." /SD IDOK
    Abort
  ${EndIf}
!macroend

Function CheckFolder
  !insertmacro FOREIGN_CHECK
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
FunctionEnd

; Through Explorer, so the browser does not run as Administrator. The
; shortcut holds the right address (port, http or https).
Function OpenAmberSight
  Exec '"$WINDIR\explorer.exe" "$SMPROGRAMS\AmberSight.url"'
FunctionEnd

Section "AmberSight" SecMain
  SectionIn RO
  ; (Also here: a silent install skips the folder page.)
  !insertmacro FOREIGN_CHECK
  SetOutPath "$INSTDIR"

  ; An earlier version is stopped first so its files can be replaced.
  ${If} ${FileExists} "$INSTDIR\ambersight.exe"
    DetailPrint "Stopping the AmberSight service"
    nsExec::ExecToLog '"$INSTDIR\ambersight.exe" service stop'
    Pop $0
  ${EndIf}

  !insertmacro INSTALL_FILES
  File "/oname=ambersight.ico" "${ICON}"
  FileOpen $1 "$INSTDIR\${MARKER}" w
  FileWrite $1 "Installed by the AmberSight ${VERSION} installer. The uninstaller removes only the files it put here.$\r$\n"
  FileClose $1
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Settings, service, firewall rule and shortcuts.
  DetailPrint "Setting up AmberSight"
  nsExec::ExecToLog '"$INSTDIR\ambersight.exe" setup --installed --no-browser'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "AmberSight was copied, but setting it up did not finish (code $0). Show details lists the reason. To try again, run as Administrator:$\r$\n$\r$\n$\"$INSTDIR\ambersight.exe$\" setup --installed" /SD IDOK
  ${EndIf}

  ; Add/Remove Programs.
  WriteRegStr HKLM "${APP_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "AmberSight NVR"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "AmberSight"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\ambersight.ico"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  ; Service, firewall rule and shortcuts; settings and recordings stay.
  ${If} ${FileExists} "$INSTDIR\ambersight.exe"
    DetailPrint "Removing the AmberSight service, firewall rule and shortcuts"
    nsExec::ExecToLog '"$INSTDIR\ambersight.exe" setup --uninstall --installed'
    Pop $0
  ${EndIf}

  ; Exactly the files the installer put here; anything else stays.
  !insertmacro UNINSTALL_FILES
  Delete "$INSTDIR\ambersight.ico"
  Delete "$INSTDIR\${MARKER}"
  ; (Written by the service: where its ffmpeg is, for the capture helper.)
  Delete "$INSTDIR\ffmpeg-path.txt"
  Delete "$INSTDIR\uninstall.exe"
  ; (Left by the built-in updater: the version before the last update, and
  ; files renamed while in use.)
  RMDir /r "$INSTDIR\previous"
  Delete "$INSTDIR\*.old"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "${APP_KEY}"
SectionEnd
