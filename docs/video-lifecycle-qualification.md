# Windows video lifecycle qualification

This procedure qualifies PR #1's existing playback policy. A green unit-test or
process result is **not** a visual pass. Rendering arguments, source resolution,
source frame rate, scaling, and the reveal policy are unchanged by these tests.

## Deterministic and process tests

Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test --all-targets` on Windows. The normal CI Windows job runs these
plus parsing the desktop runner and compiling its embedded Win32 binding.

- `covered_release_boundary_and_cancellation_are_exact`: 30-second constant,
  one nanosecond before release, cancellation exactly at the deadline, and a
  full new countdown after cancellation.
- `repeated_cover_cycles_do_not_inherit_elapsed_time`: eight independent cycles,
  exact deadline and continued coverage; no sleeps or physical desktop required.
- `repeated_release_reset_and_drop_reap_owned_processes`: six real, owned `ping`
  child processes; process-handle waits confirm termination after release/reset,
  idempotent release, cleared reveal/pause state and no restart while frozen.
  A seventh process verifies playback drop cleanup. This tests the production
  `OwnedProcess` mechanism, not mpv rendering or the OS cover detector.
- Existing tests retain reveal deadlines (100 ms after child discovery, capped
  at two seconds) and efficient source-resolution/source-FPS rendering arguments.

Windows-specific tests are not compiled by the Linux animation backend.
The existing ignored `original_video_poster_and_owned_process_lifecycle` test
can additionally verify native mpv extraction/process ownership:

```powershell
$env:AURORA_VIDEO_TEST_SOURCE = (Resolve-Path .\fixture.mp4).Path
cargo test original_video_poster_and_owned_process_lifecycle -- --ignored --nocapture
```

## Reproducible fixture and desktop setup

Use the local, authorized interactive Windows desktop (prefer the Snapdragon X
laptop used for the efficiency baseline). Install native mpv and FFmpeg. Generate
a bright, moving 1080p60 fixture with no intentional black frames:

```powershell
ffmpeg -f lavfi -i 'testsrc2=size=1920x1080:rate=60' -t 12 -an -c:v libx264 -pix_fmt yuv420p fixture.mp4
ffprobe -v error -select_streams v:0 -show_entries stream=width,height,r_frame_rate -of json fixture.mp4
cargo build --release
```

Record FFmpeg/mpv versions and GPU driver, display refresh/scaling and resolution.
Use one enabled display, on AC, battery saver off, session unlocked, display on.
Enable `animated.enabled`, `animated.pause-when-covered`, and source extensions
`mp4,webm` in `%APPDATA%\aurora\config.kdl`; restart Aurora after editing. Disable
rotation (or pause cycling with `aurora-ctl pause`) during the run. Keep transitions
disabled for an unambiguous poster-to-player observation. The script requires an
already-running daemon; it does not alter configuration, stop the daemon, or kill
unrelated processes. It selects the fixture through the normal IPC command.

Close/minimize all other windows covering the desktop. Start the following in a
non-maximized terminal, then minimize it during the five-second startup delay:

```powershell
$auroraPid = (Get-Process aurora).Id
powershell -NoProfile -File .\scripts\qualify-video-lifecycle.ps1 -Source .\fixture.mp4 -DaemonPid $auroraPid
```

Each of three cycles first covers for five seconds and verifies the same player
survives cancellation. It then keeps a borderless magenta window over the display
until the player exits. Exit before 30 seconds fails; exit later than 36 seconds
fails (30 seconds plus policy detection/polling and observer tolerance). Three more
covered seconds must not recreate the player. Uncovering must create a different
owned PID and a visible mpv child of the selected display's wallpaper host within
10 seconds. That is a process/window timeout, not a claim that playback has drawn.
The unit tests enforce the exact logical threshold, independent of these live
sampling tolerances. The runner intentionally supports one display; multi-monitor
cover/stacking qualification is a separate manual run, not silently inferred.

The runner filters direct children by Aurora's PID-specific video IPC pipe and
`--vo=gpu`, records full command lines and creation times, and never uses global
`taskkill mpv`. `timeline.jsonl` contains monotonic/UTC timestamps, owned process
snapshots, memory, HWND/parent/class/visibility/bounds, cover/uncover markers,
shutdown and restart observations. `result.json` keeps `processChecks` separate
from `visualReview`, which always remains `NOT RUN` until a human records review.
Initial and recovery PNGs show the desktop, so review them before sharing.
Sampling and screenshots add overhead; do not use this run to measure efficiency.

The cover is disposed even on failure. Aurora and the selected fixture remain
running after the script; restore the prior selection using the saved
`previous-wallpaper.json` and `aurora-ctl set <original-source>` (use the original
video rather than a cached poster). Resume cycling if it was paused for this test.
If a different wallpaper rotates in, the daemon exits, or power state changes,
discard the run and repeat with controlled setup.

## Visual and efficiency evidence (requires the physical desktop)

Record the display continuously at 60 FPS or faster, ideally with an external
camera. Start before initial playback and keep recording across each uncover and
recovery. Screen capture may omit shell/GPU surfaces; validate the capture against
what is physically visible. Screenshots are only sparse supporting evidence and
cannot establish the absence of transient black frames/flicker.

Review initial startup and every recovery frame by frame: static poster is
present while the new window is hidden; video becomes visibly moving; no black
flash, flicker, icon/desktop stacking error, uncovered top-level player window,
or unexpectedly delayed visible motion. Record first moving frame time relative
to the uncover marker and the expected up-to-two-second covered-policy wake plus
startup/reveal delay. A visible HWND/child is not evidence of a presented frame:
the current reveal heuristic detects window creation, not presentation readiness.
Report defects with recording timestamps, timeline and source hash before changing
that heuristic. Repeat with a representative real wallpaper after the test pattern.

Separately, stop the observer/recorder and repeat the prior 1080p60-on-2560x1600
steady-state efficiency measurement. Record mpv private memory and GPU 3D engine
utilization, decoder engine utilization, AC/power mode, driver and mpv version,
and sample duration. Compare with PR #1's ~7.8–8% GPU 3D / ~141 MiB baseline under
matched conditions; this PR neither remeasures nor claims to reproduce it.

## Qualification report template

- Commit, machine/OS/GPU/driver, mpv and fixture SHA256:
- Automated tests (command, counts, failures, CI URL):
- Process/window run (timeline, result, observed release/recovery durations):
- Visual run (recording, frame-review timestamps, pass/fail for each cycle):
- Efficiency run (matched settings, observer off, duration, GPU and memory):
- Unrun items and reasons:

Until filled with actual evidence, process-level desktop execution, visual
playback recovery and efficiency remeasurement remain **NOT RUN**.
