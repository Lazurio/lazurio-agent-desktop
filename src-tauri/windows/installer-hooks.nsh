; Lazurio Agent Desktop: NSIS hooks (bundle.windows.nsis.installerHooks).
;
; Windows cannot replace a running .exe, and Tauri's own check covers only
; the app's main executable, not the Agent Desktop engine next to it. Before
; copying files (and before uninstalling), end every process that runs from
; this installation folder: the app, its engine and anything the engine
; started. Only executables inside $INSTDIR are touched, never processes of
; the same name elsewhere.
;
; The installer is 32-bit: it runs the 64-bit PowerShell through Sysnative,
; and paths come from Win32_Process.ExecutablePath, which a 32-bit process
; reads for 64-bit processes too (Get-Process .Path does not). The folder is
; passed in an environment variable, so the path needs no quoting.

!macro LAD_END_PROCESSES_IN_INSTDIR
  Push $0
  Push $1
  System::Call 'Kernel32::SetEnvironmentVariable(t "LAD_INSTDIR", t "$INSTDIR")i'
  StrCpy $1 "$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe"
  IfFileExists "$1" +2 0
    StrCpy $1 "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe"
  nsExec::ExecToLog `"$1" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$dir = $$env:LAD_INSTDIR.TrimEnd('\') + '\'; Get-CimInstance Win32_Process | Where-Object { $$_.ExecutablePath -and $$_.ExecutablePath.StartsWith($$dir, [System.StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { Stop-Process -Id $$_.ProcessId -Force -ErrorAction SilentlyContinue; Wait-Process -Id $$_.ProcessId -Timeout 10 -ErrorAction SilentlyContinue }"`
  Pop $0
  Pop $1
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro LAD_END_PROCESSES_IN_INSTDIR
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro LAD_END_PROCESSES_IN_INSTDIR
!macroend
