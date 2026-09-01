; NOBD Desktop installer (NSIS).
;
; Build:  makensis /DVERSION=0.6.0 installer\nobd.nsi
; Expects the staged payload in _release\NOBD-Desktop-v${VERSION}\.
;
; This installer deliberately does NOT install the driver or create the virtual
; controller. That happens in the app, behind its own disclosure of what it adds
; to the machine (a signed driver, a certificate in the machine trust store, a
; controller that survives reboots). Burying that consent inside a silent
; installer step would be the wrong trade.
;
; The uninstaller DOES remove all of it, by running `nobd.exe --uninstall`
; before deleting the files — an entry in Add/Remove Programs promises a clean
; removal, so it has to deliver one.

!ifndef VERSION
  !define VERSION "0.0.0"
!endif

!define APPNAME   "NOBD Desktop"
!define COMPANY   "NOBD"
!define EXENAME   "nobd.exe"
!define PAYLOAD   "..\_release\NOBD-Desktop-v${VERSION}"
!define REGKEY    "Software\Microsoft\Windows\CurrentVersion\Uninstall\NOBDDesktop"

Name "${APPNAME} ${VERSION}"
OutFile "..\_release\NOBD-Desktop-Setup-${VERSION}.exe"
Unicode True
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\${COMPANY}\${APPNAME}" "InstallDir"
; Program Files, the driver install, and the uninstall all need it.
RequestExecutionLevel admin
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName"     "${APPNAME}"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${COMPANY}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "LegalCopyright"  "MIT"

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define MUI_ABORTWARNING
!define MUI_ICON   "nobd.ico"
!define MUI_UNICON "nobd.ico"

!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Open NOBD Desktop"
!define MUI_FINISHPAGE_TEXT "NOBD is installed.$\r$\n$\r$\nOpen it and click Install NOBD Controller to add the virtual controller Windows needs. Then pick NOBD Controller in your game."
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Refuse to build against a payload that isn't there, rather than shipping an
; installer with no application in it.
!if /FileExists "${PAYLOAD}\${EXENAME}"
!else
  !error "payload missing: ${PAYLOAD}\${EXENAME} — stage the release bundle first"
!endif

Function .onInit
  ; An in-place upgrade over a running app cannot replace nobd.exe.
  nsExec::Exec 'taskkill /IM ${EXENAME} /F'
  Pop $0
  Sleep 500
FunctionEnd

Section "NOBD Desktop" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${PAYLOAD}\${EXENAME}"
  ; The in-game hook. The app copies this into the game folder itself, so it
  ; MUST sit next to nobd.exe - `gameinstall::dll_source` looks there.
  File "${PAYLOAD}\DINPUT8.dll"
  File "${PAYLOAD}\README.txt"
  File "${PAYLOAD}\gamecontrollerdb.txt"

  ; The app resolves its driver bundle as <exe dir>\driver, so this layout is
  ; load-bearing, not cosmetic.
  SetOutPath "$INSTDIR\driver"
  File "${PAYLOAD}\driver\*.*"

  SetOutPath "$INSTDIR\hidhide"
  File /nonfatal "${PAYLOAD}\hidhide\*.*"

  SetOutPath "$INSTDIR"
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
  CreateShortCut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"

  WriteRegStr HKLM "Software\${COMPANY}\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\${COMPANY}\${APPNAME}" "Version" "${VERSION}"

  WriteRegStr   HKLM "${REGKEY}" "DisplayName"     "${APPNAME}"
  WriteRegStr   HKLM "${REGKEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKLM "${REGKEY}" "Publisher"       "${COMPANY}"
  WriteRegStr   HKLM "${REGKEY}" "DisplayIcon"     "$INSTDIR\${EXENAME}"
  WriteRegStr   HKLM "${REGKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKLM "${REGKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr   HKLM "${REGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKLM "${REGKEY}" "URLInfoAbout"    "https://github.com/t3chnicallyinclined/nobd-desktop"
  WriteRegDWORD HKLM "${REGKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${REGKEY}" "NoRepair" 1

  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  ; Stop the app first: it holds the exe, and its sync loop is still driving the
  ; virtual pad we are about to remove.
  nsExec::Exec 'taskkill /IM ${EXENAME} /F'
  Pop $0
  Sleep 500

  ; Hand the machine back: un-cloak the stick, release the pad, remove the
  ; devnodes, the logon task, the driver packages and the signing certificate.
  ; Without this an uninstall would leave a driver, a machine-wide trusted
  ; certificate, a reboot-surviving controller, an elevated scheduled task — and,
  ; worst of all, possibly a stick still hidden from every game.
  ${If} ${FileExists} "$INSTDIR\${EXENAME}"
    nsExec::ExecToLog '"$INSTDIR\${EXENAME}" --uninstall'
    Pop $0
  ${EndIf}

  Delete "$INSTDIR\${EXENAME}"
  Delete "$INSTDIR\DINPUT8.dll"
  Delete "$INSTDIR\README.txt"
  Delete "$INSTDIR\gamecontrollerdb.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /r "$INSTDIR\driver"
  RMDir /r "$INSTDIR\hidhide"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir  "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"

  DeleteRegKey HKLM "${REGKEY}"
  DeleteRegKey HKLM "Software\${COMPANY}\${APPNAME}"
SectionEnd
