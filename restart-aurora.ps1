[CmdletBinding()]
<#
.SYNOPSIS
Cleanly restart the aurora wallpaper daemon (recovers a wedged instance).

.DESCRIPTION
aurora occasionally wedges: the process is alive but stops advancing the
wallpaper (or its control pipe stops responding). The fix is a graceful
'aurora-ctl quit' followed by a NON-ELEVATED relaunch -- never run elevated, as
an elevated aurora creates an integrity-level split-brain where the user's
aurora-ctl gets AccessDenied on the pipe.

Run from a normal (non-admin) shell. Idempotent.
#>
param(
    [string]$AuroraDir = 'C:\Users\kalli\Development\tools\WM\aurora\target\release',
    [int]$ReadyTimeoutSec = 40
)

$ErrorActionPreference = 'Stop'
$exe = [IO.Path]::GetFullPath((Join-Path $AuroraDir 'aurora.exe'))
$ctl = Join-Path $AuroraDir 'aurora-ctl.exe'
if (-not (Test-Path $exe)) { throw "aurora.exe not found: $exe" }
if (-not (Test-Path $ctl)) { throw "aurora-ctl.exe not found: $ctl" }

# Guard against elevation (would split-brain the control pipe).
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if ($admin) { throw "Refusing to run elevated. Relaunch aurora from a normal shell to avoid an integrity-level pipe split-brain." }

# 1. Ask the running daemon to quit; fall back to killing leftover processes.
Write-Host "Stopping aurora..."
try { & $ctl quit 2>$null | Out-Null } catch {}
Start-Sleep -Seconds 2
$currentSessionId = (Get-Process -Id $PID).SessionId
$leftover = @(Get-Process aurora -ErrorAction SilentlyContinue | Where-Object {
    try {
        $_.SessionId -eq $currentSessionId -and
            [String]::Equals([IO.Path]::GetFullPath($_.Path), $exe, [StringComparison]::OrdinalIgnoreCase)
    } catch {
        $false
    }
})
foreach ($p in $leftover) {
    Write-Host ("  killing leftover aurora PID {0}" -f $p.Id)
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Seconds 1

# 2. Relaunch (detached, non-elevated).
Write-Host "Launching aurora..."
Start-Process -FilePath $exe -WorkingDirectory $AuroraDir | Out-Null

# 3. Wait for the control pipe to answer (daemon indexes its library on boot).
$deadline = (Get-Date).AddSeconds($ReadyTimeoutSec)
$ready = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    try {
        $status = & $ctl status 2>$null | Out-String
        if ($status -match '"running"\s*:\s*true') { $ready = $true; break }
    } catch {}
}

if ($ready) {
    Write-Host "aurora is up and responding."
    & $ctl current-wallpaper 2>$null
} else {
    Write-Warning ("aurora process started but its control pipe did not respond within {0}s." -f $ReadyTimeoutSec)
    exit 1
}
