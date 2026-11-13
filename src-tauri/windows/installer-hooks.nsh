; Lazurio Agent Desktop: NSIS hooks (bundle.windows.nsis.installerHooks).
;
; Windows cannot replace a running .exe. Before copying files (and before
; uninstalling), end every process that runs from this installation folder:
; the app, its Agent Desktop engine and anything the engine started. Only
; executables inside $INSTDIR of this user are touched, never processes of
; the same name elsewhere. The folder goes through an environment variable,
; so no quoting of the path is needed.

!macro LAD_END_PROCESSES_IN_INSTDIR
  System::Call 'Kernel32::SetEnvironmentVariable(t "LAD_INSTDIR", t "$INSTDIR")i'
  nsExec::ExecToLog `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$dir = $$env:LAD_INSTDIR.TrimEnd('\') + '\'; Get-Process | Where-Object { $$_.Path -and $$_.Path.StartsWith($$dir, [System.StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { Stop-Process -Id $$_.Id -Force -ErrorAction SilentlyContinue; Wait-Process -Id $$_.Id -Timeout 10 -ErrorAction SilentlyContinue }"`
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro LAD_END_PROCESSES_IN_INSTDIR
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro LAD_END_PROCESSES_IN_INSTDIR
!macroend
