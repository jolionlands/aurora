[CmdletBinding()]
<#
.SYNOPSIS
Cleanly restart the Aurora wallpaper daemon.

.DESCRIPTION
Performs a graceful 'aurora-ctl quit' followed by a NON-ELEVATED relaunch.
This is useful after rebuilding, changing restart-only configuration, or
recovering from an unexpected process failure. Never run elevated: an elevated
Aurora creates an integrity-level split-brain where the user's aurora-ctl gets
AccessDenied on the pipe.

Run from a normal (non-admin) shell. Idempotent.
#>
param(
    [string]$AuroraDir,
    [int]$ReadyTimeoutSec = 600
)

$ErrorActionPreference = 'Stop'
if ([String]::IsNullOrWhiteSpace($AuroraDir)) {
    $AuroraDir = Join-Path $PSScriptRoot 'target\release'
}
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
$process = Start-Process -FilePath $exe -WorkingDirectory $AuroraDir -WindowStyle Hidden -PassThru

# 3. Wait for the control pipe to answer (daemon indexes its library on boot).
$deadline = (Get-Date).AddSeconds($ReadyTimeoutSec)
$ready = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    if ($process.HasExited) {
        throw "aurora exited before its control pipe became ready."
    }
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
