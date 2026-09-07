; Run this package's CLI from temporary storage, never the old installed binary.
; Both upgrade and uninstall must stop Agent before changing installed files.
!define NEXUS_INSTALLER_STOP_BINARY "${__FILEDIR__}\..\resources\nexus-launcher.exe"
!macro NexusStopInstalledAgent
  Push $0
  Push $OUTDIR
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File /oname=nexus-installer-stop.exe "${NEXUS_INSTALLER_STOP_BINARY}"
  nsExec::ExecToLog '"$PLUGINSDIR\nexus-installer-stop.exe" installer-stop --install-dir "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Nexus Agent could not be stopped safely. Close Nexus and retry. No application files have been replaced or removed." /SD IDOK
    Abort
  ${EndIf}
  Pop $0
  SetOutPath "$0"
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro NexusStopInstalledAgent
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro NexusStopInstalledAgent
  !insertmacro NexusAskDataCleanup
!macroend

; The temporary native helper asks explicitly, with Keep as the default.
; Update/passive/silent uninstall never asks and never deletes user data.
!macro NexusAskDataCleanup
  ${If} $UpdateMode = 1
  ${OrIf} $PassiveMode = 1
    Goto NexusCleanupDone
  ${EndIf}
  IfSilent NexusCleanupDone
  ; Tauri also invokes an existing uninstaller with _?= during ordinary
  ; reinstall/upgrade flows, even when /UPDATE was not supplied.
  Push $R9
  ClearErrors
  ${GetOptions} $CMDLINE "_?=" $R9
  ${IfNot} ${Errors}
    Pop $R9
    Goto NexusCleanupDone
  ${EndIf}
  Pop $R9
  Push $0
  nsExec::ExecToLog '"$PLUGINSDIR\nexus-installer-stop.exe" installer-cleanup --data-dir "$LOCALAPPDATA\Nexus" --install-dir "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    Pop $0
    Abort
  ${EndIf}
  Pop $0
NexusCleanupDone:
!macroend
