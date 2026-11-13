# Updates a running Windows installation the way the app's updater does and
# checks that it goes through.
#
#   scripts/windows-update-test.ps1 -NewInstaller <new -setup.exe> -BuildDir <target\...\release> [-OldInstaller <old -setup.exe>]
#
# 1. Installs the old version (or the new one when no old one is given).
# 2. Starts the installed engine (agent-desktop.exe mcp) with its stdin held
#    open, like a connected channel, plus the installed app from a scheduled
#    task of this user (how the first pilot was started).
# 3. With the old installer, which has no pre-install hook, it shows what
#    happened on the pilot Windows computer: an update over a running engine
#    does not replace it. Informational only.
# 4. Runs the new installer exactly as the updater does
#    (/P /UPDATE /R /ARGS) over the running installation and requires: exit
#    code 0, no hang, the installed files equal to the new build, and the
#    app running again from the installation folder (/R).
param(
    [Parameter(Mandatory = $true)][string]$NewInstaller,
    [Parameter(Mandatory = $true)][string]$BuildDir,
    [string]$OldInstaller = ""
)
$ErrorActionPreference = "Stop"
$InstallDir = Join-Path $env:LOCALAPPDATA "Lazurio Agent Desktop"
$App = Join-Path $InstallDir "lazurio-agent-desktop.exe"
$Engine = Join-Path $InstallDir "agent-desktop.exe"
$TaskName = "LazurioAgentDesktopUpdateTest"

function Run-Installer([string]$Installer, [string[]]$Arguments, [int]$TimeoutSeconds) {
    $p = Start-Process -FilePath $Installer -ArgumentList $Arguments -PassThru
    if (-not $p.WaitForExit($TimeoutSeconds * 1000)) {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
        return "hung"
    }
    return $p.ExitCode
}

function Installed-Processes {
    Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir + "\", [System.StringComparison]::OrdinalIgnoreCase) }
}

function Start-Engine {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $Engine
    $info.Arguments = "mcp"
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.CreateNoWindow = $true
    $process = [System.Diagnostics.Process]::Start($info)
    Start-Sleep -Seconds 2
    if ($process.HasExited) { throw "the installed engine did not keep running" }
    return $process
}

function Start-AppFromTask {
    try {
        $action = New-ScheduledTaskAction -Execute $App -Argument "--at-login"
        $principal = New-ScheduledTaskPrincipal -UserId ([System.Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
        Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Force | Out-Null
        Start-ScheduledTask -TaskName $TaskName
        for ($i = 0; $i -lt 20; $i++) {
            if (Get-Process -Name "lazurio-agent-desktop" -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $App }) { return $true }
            Start-Sleep -Milliseconds 500
        }
        Write-Host "::warning::the app did not start from the scheduled task on this runner"
        return $false
    } catch {
        Write-Host "::warning::scheduled task not available on this runner: $_"
        return $false
    }
}

function Hash([string]$Path) { (Get-FileHash -Algorithm SHA256 $Path).Hash }

# 1. Old installation.
$first = if ($OldInstaller) { $OldInstaller } else { $NewInstaller }
$code = Run-Installer $first @("/S") 300
if ($code -ne 0) { throw "first install failed: $code" }
if (-not (Test-Path $Engine)) { throw "no engine at $Engine" }
Get-Process -Name "lazurio-agent-desktop" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

# 2. Running engine and app, as on a connected computer.
$engineProcess = Start-Engine
$fromTask = Start-AppFromTask
Write-Host "running from the installation: $((Installed-Processes | ForEach-Object { $_.Name }) -join ', ')"

# 3. The pilot computer's situation: an update without the hook.
if ($OldInstaller) {
    $oldHash = Hash $Engine
    $code = Run-Installer $OldInstaller @("/S", "/UPDATE") 180
    $stillRunning = -not $engineProcess.HasExited
    Write-Host "old installer (no hook) over the running engine: exit $code, engine still running: $stillRunning"
    if ($engineProcess.HasExited) { $engineProcess = Start-Engine }
}

# 4. The new installer, exactly as the updater runs it.
$code = Run-Installer $NewInstaller @("/P", "/UPDATE", "/R", "/ARGS", "--at-login") 300
Write-Host "new installer /P /UPDATE /R over the running installation: exit $code"
if ("$code" -ne "0") { throw "the update over a running installation did not complete (exit $code)" }
if (-not $engineProcess.HasExited) { throw "the running engine survived the update" }
foreach ($name in @("lazurio-agent-desktop.exe", "agent-desktop.exe")) {
    $installed = Hash (Join-Path $InstallDir $name)
    $built = Hash (Join-Path $BuildDir $name)
    if ($installed -ne $built) { throw "$name was not replaced by the new build" }
}
Write-Host "installed files match the new build"

# /R: the installer starts the app again.
$relaunched = $false
for ($i = 0; $i -lt 60; $i++) {
    if (Get-Process -Name "lazurio-agent-desktop" -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $App }) { $relaunched = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $relaunched) { throw "the app did not start again after the update (/R)" }
Write-Host "the app is running again after the update (started from a scheduled task before: $fromTask)"

# Clean up.
Installed-Processes | Stop-Process -Force -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
Write-Host "update over a running installation: ok"
