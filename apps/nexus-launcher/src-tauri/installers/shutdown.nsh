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
!macroend

; Uninstall experience: the data root (configuration, release slots, cached
; runtimes) survives by default; offer an explicit cleanup. Harness user data
; under the user's .dsh home is never touched.
!macro NexusAskDataCleanup
  MessageBox MB_YESNO|MB_ICONQUESTION "Also remove version slots and cached runtimes from the default data folder ($LOCALAPPDATA\Nexus)? Custom NEXUS_DATA_DIR locations are not removed. Your Harness data in the .dsh folder is always kept." IDYES NexusCleanupData
  Goto NexusCleanupDone
NexusCleanupData:
  RMDir /r "$LOCALAPPDATA\Nexus\releases"
  RMDir /r "$LOCALAPPDATA\Nexus\runtimes"
NexusCleanupDone:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro NexusAskDataCleanup
!macroend
