; NusaDB Studio — Windows installer (NSIS 3, Unicode). Built by build-installer.ps1 on Windows
; or build-installer.sh on Linux; both pass:
;   /DVERSION=<x.y.z>   /DEXE=<path to nusadb-studio.exe>   /DOUTDIR=<output folder>
;
; Installs for the current user only (no administrator rights, no UAC prompt) into
; %LOCALAPPDATA%\Programs\NusaDB Studio, registers in "Installed apps", and can add a desktop
; shortcut and an "Open with" entry for .sql files.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!ifndef VERSION
  !error "Pass /DVERSION=x.y.z"
!endif
!ifndef EXE
  !error "Pass /DEXE=path\to\nusadb-studio.exe"
!endif
!ifndef OUTDIR
  !define OUTDIR "."
!endif

!define APP        "NusaDB Studio"
!define PUBLISHER  "NusaDB"
!define URL        "https://nusadb.com"
!define PROGID     "NusaDBStudio.sql"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\NusaDBStudio"
!define ROOT       "..\.."

Name "${APP}"
OutFile "${OUTDIR}\NusaDB-Studio-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APP} Setup"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "${PUBLISHER}"

!include "MUI2.nsh"
!include "x64.nsh"

!define MUI_ICON   "..\icons\nusadb-studio.ico"
!define MUI_UNICON "..\icons\nusadb-studio.ico"
!define MUI_WELCOMEFINISHPAGE_BITMAP   "wizard.bmp"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "wizard.bmp"
!define MUI_HEADERIMAGE
!define MUI_HEADERIMAGE_RIGHT
!define MUI_HEADERIMAGE_BITMAP   "header.bmp"
!define MUI_HEADERIMAGE_UNBITMAP "header.bmp"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_NODESC

!define MUI_WELCOMEPAGE_TEXT "This installs ${APP} ${VERSION} for your Windows account.$\r$\n$\r$\nNo administrator rights are needed. Saved servers and settings from an earlier version are kept.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_RUN "$INSTDIR\nusadb-studio.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
!define MUI_FINISHPAGE_LINK "Documentation at nusadb.com"
!define MUI_FINISHPAGE_LINK_LOCATION "https://nusadb.com/docs/"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; A running Studio keeps nusadb-studio.exe locked, so it cannot be replaced or removed. Ask
; to close it: first politely (it can still ask about unsaved work), then by force if needed.
!macro CLOSE_RUNNING UN
Function ${UN}CloseRunning
  StrCpy $R1 0
  check:
    IfFileExists "$INSTDIR\nusadb-studio.exe" 0 done
    ClearErrors
    FileOpen $R0 "$INSTDIR\nusadb-studio.exe" a
    IfErrors locked
    FileClose $R0
    Goto done
  locked:
    StrCmp $R1 0 0 stubborn
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "${APP} is running.$\r$\n$\r$\nClick OK to close it and continue, or Cancel to stop." IDOK close
    Abort
  close:
    nsExec::Exec 'taskkill /IM nusadb-studio.exe'
    Pop $R0
    StrCpy $R1 1
    Sleep 2500
    Goto check
  stubborn:
    MessageBox MB_YESNO|MB_ICONEXCLAMATION "${APP} is still running (it may be asking about unsaved work).$\r$\n$\r$\nClose it by force? Unsaved work in it will be lost." IDYES force
    Abort
  force:
    nsExec::Exec 'taskkill /F /IM nusadb-studio.exe'
    Pop $R0
    Sleep 1500
    StrCpy $R1 2
    ClearErrors
    FileOpen $R0 "$INSTDIR\nusadb-studio.exe" a
    IfErrors 0 +3
      MessageBox MB_ICONSTOP "${APP} could not be closed. Close it from Task Manager and run the setup again."
      Abort
    FileClose $R0
  done:
FunctionEnd
!macroend
!insertmacro CLOSE_RUNNING ""
!insertmacro CLOSE_RUNNING "un."

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "${APP} needs 64-bit Windows."
    Abort
  ${EndIf}
FunctionEnd

Section "${APP}" SecApp
  SectionIn RO
  Call CloseRunning
  SetOutPath "$INSTDIR"
  File "/oname=nusadb-studio.exe" "${EXE}"
  File "${ROOT}\README.md"
  SetOutPath "$INSTDIR\licenses"
  File "${ROOT}\ui\fonts\OFL-Inter.txt"
  File "${ROOT}\ui\fonts\OFL-JetBrainsMono.txt"
  SetOutPath "$INSTDIR"

  CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\nusadb-studio.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayName"          "${APP}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayVersion"       "${VERSION}"
  WriteRegStr   HKCU "${UNINST_KEY}" "Publisher"            "${PUBLISHER}"
  WriteRegStr   HKCU "${UNINST_KEY}" "URLInfoAbout"         "${URL}"
  WriteRegStr   HKCU "${UNINST_KEY}" "DisplayIcon"          "$INSTDIR\nusadb-studio.exe,0"
  WriteRegStr   HKCU "${UNINST_KEY}" "InstallLocation"      "$INSTDIR"
  WriteRegStr   HKCU "${UNINST_KEY}" "UninstallString"      '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  SectionGetSize ${SecApp} $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\nusadb-studio.exe"
SectionEnd

Section /o "Open .sql files with ${APP}" SecSql
  ; Offered under "Open with"; the user's current default for .sql is left alone.
  WriteRegStr HKCU "Software\Classes\.sql\OpenWithProgids" "${PROGID}" ""
  WriteRegStr HKCU "Software\Classes\${PROGID}" "" "SQL script"
  WriteRegStr HKCU "Software\Classes\${PROGID}\DefaultIcon" "" "$INSTDIR\nusadb-studio.exe,0"
  WriteRegStr HKCU "Software\Classes\${PROGID}\shell\open\command" "" '"$INSTDIR\nusadb-studio.exe" "%1"'
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

Section "Uninstall"
  Call un.CloseRunning
  Delete "$INSTDIR\nusadb-studio.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\licenses\OFL-Inter.txt"
  Delete "$INSTDIR\licenses\OFL-JetBrainsMono.txt"
  RMDir  "$INSTDIR\licenses"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}.lnk"
  Delete "$DESKTOP\${APP}.lnk"

  DeleteRegValue HKCU "Software\Classes\.sql\OpenWithProgids" "${PROGID}"
  DeleteRegKey   HKCU "Software\Classes\${PROGID}"
  DeleteRegKey   HKCU "${UNINST_KEY}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ; Saved servers (%APPDATA%\nusadb-studio) and keychain entries stay, so a reinstall finds them.
SectionEnd
