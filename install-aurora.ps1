[CmdletBinding()]
<#
.SYNOPSIS
Install Aurora outside the build tree and restart it from there.

.DESCRIPTION
Copies aurora.exe and aurora-ctl.exe from a build directory to a stable
per-user folder, registers that copy for autostart, and restarts the daemon
through restart-aurora.ps1. Autostart never points into target\, so
`cargo clean` or a disk cleanup cannot silently stop Aurora at the next login.
Binaries being replaced are kept under previous\ for rollback.

Run from a normal (non-admin) shell.
#>
param(
    [string]$From,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\aurora')
)

$ErrorActionPreference = 'Stop'
if ([String]::IsNullOrWhiteSpace($From)) {
    $From = Join-Path $PSScriptRoot 'target\release'
}
$names = 'aurora.exe', 'aurora-ctl.exe'
foreach ($name in $names) {
    if (-not (Test-Path (Join-Path $From $name))) { throw "$name not found in $From" }
}

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if ($admin) { throw "Refusing to run elevated. Install aurora from a normal shell to avoid an integrity-level pipe split-brain." }

New-Item -ItemType Directory -Force $InstallDir | Out-Null
$ctl = Join-Path $InstallDir 'aurora-ctl.exe'
# The running daemon locks its executable: stop it before replacing files.
if (Test-Path $ctl) {
    try { & $ctl quit 2>$null | Out-Null } catch {}
    Start-Sleep -Seconds 2
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
foreach ($name in $names) {
    $target = Join-Path $InstallDir $name
    if (Test-Path $target) {
        $previous = Join-Path $InstallDir "previous\$stamp"
        New-Item -ItemType Directory -Force $previous | Out-Null
        Move-Item $target (Join-Path $previous $name)
    }
    Copy-Item (Join-Path $From $name) $target
    Write-Host ("installed {0}  sha256 {1}" -f $target, (Get-FileHash $target).Hash)
}

& (Join-Path $InstallDir 'aurora.exe') --register-autostart
& (Join-Path $PSScriptRoot 'restart-aurora.ps1') -AuroraDir $InstallDir
