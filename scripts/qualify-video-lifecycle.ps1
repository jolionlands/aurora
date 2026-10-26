# Run from an interactive Windows desktop, with Aurora already configured/running.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][int]$DaemonPid,
    [string]$Ctl = '.\target\release\aurora-ctl.exe',
    [int]$ScreenIndex = 0,
    [ValidateRange(2,20)][int]$Cycles = 3,
    [string]$Output = (Join-Path $PWD ('qualification-' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class WallpaperWindows {
    public delegate bool Visitor(IntPtr h, IntPtr p);
    [DllImport("user32.dll")] static extern bool EnumWindows(Visitor f, IntPtr p);
    [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr h, Visitor f, IntPtr p);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetParent(IntPtr h);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public class Row {
        public long Hwnd, Parent; public uint Pid; public string Class;
        public bool Visible; public RECT Bounds;
    }
    public static Row[] Snapshot(int[] owners) {
        var rows = new Dictionary<long,Row>();
        Visitor collect = (h,p) => {
            uint pid; GetWindowThreadProcessId(h,out pid);
            if (Array.IndexOf(owners,(int)pid) >= 0) {
                var name = new StringBuilder(256); GetClassName(h,name,256);
                RECT rect; GetWindowRect(h,out rect);
                rows[h.ToInt64()] = new Row { Hwnd=h.ToInt64(), Parent=GetParent(h).ToInt64(),
                    Pid=pid, Class=name.ToString(), Visible=IsWindowVisible(h), Bounds=rect };
            }
            return true;
        };
        EnumWindows((h,p) => { collect(h,p); EnumChildWindows(h,collect,p); return true; },IntPtr.Zero);
        var result = new Row[rows.Count]; rows.Values.CopyTo(result,0); return result;
    }
}
'@
if (-not [Environment]::UserInteractive) { throw 'An interactive Windows desktop is required.' }
$sourcePath = (Resolve-Path $Source).Path
$ctlPath = (Resolve-Path $Ctl).Path
if ([IO.Path]::GetExtension($sourcePath).ToLowerInvariant() -notin @('.mp4','.webm')) { throw 'Source must be MP4 or WebM.' }
$daemon = Get-CimInstance Win32_Process -Filter "ProcessId=$DaemonPid"
if (-not $daemon -or $daemon.Name -ne 'aurora.exe') { throw 'DaemonPid must identify the running aurora.exe.' }
$screen = [Windows.Forms.Screen]::AllScreens[$ScreenIndex]
if (-not $screen) { throw 'Invalid ScreenIndex.' }
New-Item -ItemType Directory -Path $Output -ErrorAction Stop | Out-Null
$Output = (Resolve-Path $Output).Path
$clock = [Diagnostics.Stopwatch]::StartNew()
$writer = [IO.StreamWriter]::new((Join-Path $Output 'timeline.jsonl'), $false)
$writer.AutoFlush = $true
$form = $null
$result = @{ processChecks='failed'; visualReview='NOT RUN'; error=$null }
function Event($kind, $data) {
    $writer.WriteLine((@{ ms=$clock.Elapsed.TotalMilliseconds; utc=[DateTime]::UtcNow.ToString('o'); event=$kind; data=$data } | ConvertTo-Json -Depth 10 -Compress))
}
function Sample {
    # Include only direct children whose unique wallpaper IPC pipe belongs to this daemon.
    $children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$DaemonPid" | Where-Object {
        $_.CommandLine -like "*aurora-video-$DaemonPid-*" -and $_.CommandLine -like '*--vo=gpu*'
    })
    $ids = @($children | ForEach-Object { [int]$_.ProcessId })
    $windows = @([WallpaperWindows]::Snapshot([int[]](@($DaemonPid) + $ids)))
    $sample = @{ players=@($children | Select-Object ProcessId,ParentProcessId,CreationDate,ExecutablePath,CommandLine,PrivatePageCount);
        windows=$windows }
    Event 'sample' $sample
    return $sample
}
function Shot($name) {
    $bounds = $screen.Bounds
    $bitmap = [Drawing.Bitmap]::new($bounds.Width,$bounds.Height)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($bounds.Location,[Drawing.Point]::Empty,$bounds.Size)
        $bitmap.Save((Join-Path $Output ($name + '.png')),[Drawing.Imaging.ImageFormat]::Png)
    } finally { $graphics.Dispose(); $bitmap.Dispose() }
}
function Pump { [Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 100 }
function Wait-Playing($label, $deadline = ($clock.Elapsed.TotalSeconds + 10), $capturePrefix = $null) {
    $frame = 0
    do {
        Pump
        $s = Sample
        # On multiple monitors all wallpaper players must recover; selected monitor is also visible.
        $hosts = @($s.windows | Where-Object { $_.Class -eq 'AuroraAnimatedWallpaper' -and $_.Visible -and
            $_.Bounds.Left -eq $screen.Bounds.Left -and $_.Bounds.Top -eq $screen.Bounds.Top })
        $embedded = @($s.windows | Where-Object { $_.Visible -and $_.Pid -ne $DaemonPid -and
            $_.Parent -in @($hosts | ForEach-Object { $_.Hwnd }) })
        if ($s.players.Count -eq 1 -and $s.players[0].CommandLine.IndexOf($sourcePath, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and $hosts.Count -gt 0 -and $embedded.Count -gt 0) {
            if ($clock.Elapsed.TotalSeconds -gt $deadline) { throw "$label was observed after its deadline." }
            $s.observedSeconds = $clock.Elapsed.TotalSeconds
            Event $label $s
            return $s
        }
        if ($capturePrefix) { Shot "$capturePrefix-$frame"; $frame++ }
    } while ($clock.Elapsed.TotalSeconds -lt $deadline)
    throw "$label did not produce one owned player with a visible embedded window within 10 seconds."
}
try {
    Event 'environment' @{ daemon=$daemon | Select-Object ProcessId,CreationDate,ExecutablePath,CommandLine;
        source=$sourcePath; sha256=(Get-FileHash $sourcePath -Algorithm SHA256).Hash;
        screen=$screen.DeviceName; bounds=$screen.Bounds; os=[Environment]::OSVersion.ToString();
        note='Single-monitor qualification; close other covering windows; stay on AC without battery saver.' }
    # Preserve a record of the previous wallpaper; this runner intentionally leaves the test video selected.
    & $ctlPath --json current-wallpaper | Set-Content (Join-Path $Output 'previous-wallpaper.json')
    if ($LASTEXITCODE -ne 0) { throw 'Cannot query the current wallpaper.' }
    if ([Windows.Forms.Screen]::AllScreens.Count -ne 1) { throw 'Use a single enabled monitor for unambiguous ownership qualification.' }
    & $ctlPath set $sourcePath
    if ($LASTEXITCODE -ne 0) { throw 'Cannot select video wallpaper.' }
    Write-Host 'Uncover the desktop now; starting in five seconds.'
    Start-Sleep -Seconds 5
    $initial = Wait-Playing 'initial-player-visible'
    Shot 'initial'
    $form = [Windows.Forms.Form]::new()
    $form.FormBorderStyle = 'None'
    $form.StartPosition = 'Manual'
    $form.Bounds = $screen.Bounds
    $form.TopMost = $true
    $form.BackColor = [Drawing.Color]::Magenta
    for ($cycle=0; $cycle -lt $Cycles; $cycle++) {
        # First cancel a cover before the release threshold.
        $before = Sample
        $old = [int]$before.players[0].ProcessId
        $form.Show(); $form.Activate(); [Windows.Forms.Application]::DoEvents()
        $shortStart = $clock.Elapsed.TotalSeconds
        Event 'short-cover-shown' @{cycle=$cycle; pid=$old}
        do {
            Pump; $s = Sample
            if ($s.players.Count -ne 1 -or [int]$s.players[0].ProcessId -ne $old) { throw 'Player changed during canceled cover.' }
        } while ($clock.Elapsed.TotalSeconds - $shortStart -lt 5)
        $form.Hide(); Event 'short-cover-hidden' @{cycle=$cycle}
        $resumed = Wait-Playing 'short-cover-recovered'
        if ([int]$resumed.players[0].ProcessId -ne $old) { throw 'Canceled cover restarted the player.' }
        # Give policy a visible sample so the next cover starts a fresh timer.
        $visibleUntil = $clock.Elapsed.TotalSeconds + 3
        do { Pump; $null = Sample } while ($clock.Elapsed.TotalSeconds -lt $visibleUntil)
        $coveredAt = $clock.Elapsed.TotalSeconds
        Event 'long-cover-request' @{cycle=$cycle; pid=$old}
        $form.Show(); $form.Activate(); [Windows.Forms.Application]::DoEvents()
        Event 'long-cover-shown' @{cycle=$cycle; pid=$old}
        $stopped = $false
        do {
            Pump; $s = Sample
            $elapsed = $clock.Elapsed.TotalSeconds - $coveredAt
            if ($s.players.Count -eq 0) {
                if ($elapsed -gt 36) { throw "Player stop was not observed within 36 seconds: $elapsed." }
                if ($elapsed -lt 30) { throw "Player stopped early at $elapsed seconds." }
                Event 'player-stopped' @{cycle=$cycle; pid=$old; coverSeconds=$elapsed}
                $stopped = $true; break
            }
            if ($s.players.Count -ne 1 -or [int]$s.players[0].ProcessId -ne $old) { throw 'Player replaced while covered.' }
        } while ($elapsed -lt 36)
        if (-not $stopped) { throw 'Player did not stop within 36 seconds (30s threshold plus policy/sampling tolerance).' }
        # Remain covered for another policy interval: no premature recreation.
        $until = $clock.Elapsed.TotalSeconds + 3
        do { Pump; $s = Sample; if ($s.players.Count -ne 0) { throw 'Player recreated while covered.' } } while ($clock.Elapsed.TotalSeconds -lt $until)
        $form.Hide(); $uncoveredAt = $clock.Elapsed.TotalSeconds
        Event 'long-cover-hidden' @{cycle=$cycle}
        # Stop timing at the first observed replacement, not after a fixed capture workload.
        $recovered = Wait-Playing 'replacement-visible' ($uncoveredAt + 10) "cycle-$cycle-recovery"
        if ([int]$recovered.players[0].ProcessId -eq $old) { throw 'Recovery did not create a new player PID.' }
        Event 'recovery-complete' @{cycle=$cycle; seconds=$recovered.observedSeconds-$uncoveredAt}
        Shot "cycle-$cycle-recovered"
    }
    $result.processChecks = 'passed'
} catch {
    $result.error = $_.Exception.Message
    Event 'failure' $result.error
    throw
} finally {
    if ($form) { $form.Hide(); $form.Dispose() }
    Event 'result' $result
    $result | ConvertTo-Json | Set-Content (Join-Path $Output 'result.json')
    $writer.Dispose()
    Write-Host "Evidence: $Output. Visual review remains NOT RUN. The test wallpaper remains selected."
}
