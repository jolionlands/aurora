<div align="center">

# Aurora

**Native, multi-monitor wallpaper rotation for Windows.**

Fast indexing. Smooth transitions. Content-aware playlists. Local control.

[![CI](https://github.com/jolionlands/aurora/actions/workflows/ci.yml/badge.svg)](https://github.com/jolionlands/aurora/actions/workflows/ci.yml)
![Platform](https://img.shields.io/badge/platform-Windows-0078D4?logo=windows)
![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

[Quick start](#quick-start) | [Transitions](#enable-transitions) | [Animated](#animated-wallpapers) | [Playlists](#playlists-and-metadata) | [Autotagging](#ai-assisted-tagging) | [Scheduling](#scheduling-behavior) | [Configuration](#configuration-and-storage)

</div>

Aurora is a small Windows daemon that keeps each display fresh without getting
in your way. It uses native Windows wallpaper and graphics APIs, exposes a
scriptable command-line client, and remembers image metadata by exact content
rather than fragile file paths.

The daemon uses two async workers, with apartment-bound image decoding on its
main thread. Short-lived wallpaper helpers and duplicate launches do not create
an async worker pool. The decoded-image cache grows on demand up to its configured
limit; with transitions disabled, wallpaper commits bypass that decode cache.

## Highlights

| | |
| --- | --- |
| **Multi-monitor** | Native `IDesktopWallpaper` commits with per-display transitions and failure isolation |
| **Smooth** | Crossfade, slide, wipe, dissolve, and zoom using Direct2D with a CPU fallback |
| **Fast** | Persistent validated photo index and a bounded decoded-image LRU cache |
| **Organized** | Static and dynamic playlists, ratings, frequency weights, grouped tags, and filters |
| **Content-aware** | Exact BLAKE3 identity reconnects indexed renames and shares metadata across duplicates |
| **Automatable** | Named-pipe IPC, real-time events, optional Prometheus metrics, and a companion CLI |
| **Animated** | Opt-in GIF and animated WebP playback on the desktop layer, frozen on battery, fullscreen, and lock screen |
| **Flexible** | Interval or fixed-time schedules, fullscreen/idle pauses, and optional wiri workspace triggers |

Bundled decoding supports JPEG, PNG, GIF, WebP, BMP, TIFF, and ICO. Explicit
AVIF, HEIC, or HEIF files can also work when Windows WIC has the codec required
by that file's AV1 or HEVC payload.

## Quick start

Aurora currently builds from source with the stable Rust toolchain on Windows.

```powershell
git clone https://github.com/jolionlands/aurora.git
cd aurora
cargo build --release
.\target\release\aurora.exe
```

The first launch writes
[`%APPDATA%\aurora\config.kdl`](resources/default_config.kdl). Add your wallpaper
folders there, restart Aurora, then control the running daemon from another
terminal:

```powershell
.\target\release\aurora-ctl.exe status
.\target\release\aurora-ctl.exe next
.\target\release\aurora-ctl.exe pause
.\target\release\aurora-ctl.exe resume
```

Register or remove startup with Windows:

```powershell
.\target\release\aurora.exe --register-autostart
.\target\release\aurora.exe --unregister-autostart
```

## Enable transitions

Transitions are off in the default configuration, so wallpaper changes are
instant until they are enabled. Set `enabled true`, then restart Aurora:

```kdl
transitions {
    enabled true
    duration-ms 800
    style "crossfade"
    renderer "auto"
}
```

Available styles are `crossfade`, `slide-left`, `slide-right`, `wipe-left`,
`wipe-right`, `dissolve`, `zoom-in`, `zoom-out`, and `none`. The renderer can be
`gpu`, `cpu`, or `auto`.

Aurora enumerates attached displays and commits each one through a short-lived
hidden helper with a 10-second timeout. A hung or failed apply is isolated to
that display; successful display updates are kept and reported. Disabling
transitions skips only the animation—the normal commit remains per-display so
one wallpaper change does not force an all-monitor desktop refresh. Aurora
falls back to one native all-monitor commit only if Windows cannot enumerate
the displays.

## Animated wallpapers

Animated GIF and WebP files always work as ordinary wallpapers: Windows shows
their first frame. To play them, opt in and restart Aurora:

```kdl
animated {
    enabled true
    max-fps 15
    pause-on-battery true
    pause-when-covered true
    max-memory-mb 32
    max-frames 240
    scaling "auto"
}
```

Remember to include `gif` (and `webp`) in a source's `extensions`.

Aurora still commits the file through `IDesktopWallpaper`, then draws the
frames in a window on the desktop layer, below the icons. On Windows 11 24H2
and later it uses the "raised desktop" layout, and a Direct3D swap chain
presents the frames. That is the only presentation method the new layout
composes. Older layouts use the classic `WorkerW` host.

Playback is bounded and power-aware:

- **Nothing runs** until an animated file is shown. The player thread,
  windows, and GPU device exist only while something animates.
- **Frame rate:** frames shorter than `1 / max-fps` merge into their
  neighbours, and so do identical consecutive frames.
- **Pixel art:** with `scaling "auto"`, frames that are made of uniform
  NxN blocks count as upscaled pixel art. They are stored at their native
  size, which uses N² times less memory. The GPU scales them up by the largest
  whole factor that fits, with nearest-neighbour, so they stay crisp. They are
  centered and never cropped, and the bars are filled with the art's edge
  colour. `smooth` turns detection off; `nearest` always uses
  nearest-neighbour.
- **Transparency:** transparent pixels show the same edge colour instead of
  black.
- **GPU-resident frames:** each frame is uploaded to the GPU once. After that,
  playback is a draw and a present, with no per-frame CPU copy.
- **Memory:** frames are downscaled to the display, and further until they fit
  `max-memory-mb` per display. Past `max-frames`, evenly spaced frames are
  dropped.
- **Freezing:** animation freezes on the current frame when:
  - battery saver is on;
  - the session is locked;
  - the display is off;
  - the laptop is on battery power (`pause-on-battery`);
  - a maximized or fullscreen window covers that display
    (`pause-when-covered`).

  A freeze caused by battery, battery saver, the lock screen, or the display
  being off releases the decoded frames and the GPU device after a minute.
  They are decoded again when playback resumes.

To try a file without the daemon:

```powershell
cargo run --release --example animated_preview -- "D:\Wallpapers\loop.gif" 15
```

## Control

| Command | Purpose |
| --- | --- |
| `aurora-ctl status` | Show daemon, index, scheduler, and playlist state |
| `aurora-ctl next` / `prev` | Move through wallpaper history |
| `aurora-ctl set <path>` | Apply one image immediately |
| `aurora-ctl pause` / `resume` | Control automatic rotation |
| `aurora-ctl folder <path>` | Narrow selection to a folder for this session |
| `aurora-ctl ban <hash>` | Ban an exact BLAKE3 content hash from future selection |
| `aurora-ctl reload` | Refresh sources, playlists, content metadata, and bans |
| `aurora-ctl events` | Stream newline-delimited JSON events |
| `aurora-ctl stats` | Print the metrics snapshot |
| `aurora-ctl current-wallpaper` | Show the last successful wallpaper per monitor |
| `aurora-ctl content ...` | Browse or edit shared metadata by content ID, alias, or path |

Add `--json` before a command for machine-readable output.

## Playlists and metadata

Start the Aurora daemon before using playlist commands. A playlist can mix
explicit files, selection weights, shared content metadata, and optional tag
filters:

```powershell
aurora-ctl playlist create favorites
aurora-ctl playlist add favorites current
aurora-ctl playlist tag favorites current --kind theme night neon
aurora-ctl playlist tag favorites current --kind artist studio-name
aurora-ctl playlist rate favorites current 4
aurora-ctl playlist frequency favorites current 2
aurora-ctl playlist shuffle favorites true
aurora-ctl playlist filter favorites --include theme=night --include color=blue --exclude safety=nsfw
aurora-ctl playlist activate favorites
aurora-ctl playlist show favorites --offset 0 --limit 100
aurora-ctl playlist deactivate
```

Selection weight is `frequency * (rating + 1)`. Both factors default to `1`
when unset, and frequency remains local to each playlist.

### Dynamic playlists

A dynamic playlist stores a query instead of a list of paths. Its live
membership is the current non-banned photo index filtered by shared content
tags:

```powershell
aurora-ctl playlist create night-library --dynamic
aurora-ctl playlist filter night-library --include theme=night --exclude safety=nsfw
aurora-ctl playlist shuffle night-library true
aurora-ctl playlist activate night-library
aurora-ctl playlist show night-library --offset 0 --limit 100
```

`playlist list` reports the current match count, and `playlist show` pages the
current matches in index order. Dynamic selection uses `rating + 1` as its
weight because it has no path-local frequency. Path membership, frequency, and
path-local metadata commands are intentionally rejected for dynamic playlists;
use the global `content` commands to change what their filters see.

### Tags and filters

Built-in tag groups are `general`, `theme`, `content`, `color`, `source`,
`medium`, `safety`, `franchise`, and `character`. Any other non-empty,
kebab-case group is stored as custom metadata.

Omit tags to clear a group:

```powershell
aurora-ctl playlist tag favorites current --kind artist
```

Repeat `--include` or `--exclude` with `KIND=TAG`. Includes are OR within one
kind and AND across kinds; any excluded tag rejects an image. Clear all rules
with:

```powershell
aurora-ctl playlist filter favorites
```

An active filtered playlist never silently falls back to the full library when
nothing matches.

### Shared content metadata

The `content` commands edit metadata independently of playlist membership.
Targets may be a BLAKE3 content ID, a known path alias, an image path, or
`current` when every attached display reports the same wallpaper:

```powershell
aurora-ctl content list --include theme=night --offset 0 --limit 100
aurora-ctl content show current
aurora-ctl content show "D:\Wallpapers\lake.jpg"
aurora-ctl content tag "D:\Wallpapers\lake.jpg" --kind theme night neon
aurora-ctl content rate "D:\Wallpapers\lake.jpg" 5
aurora-ctl content clear "D:\Wallpapers\lake.jpg"
```

### Content identity

Tags, dimensions, default rating, and bounded autotag provenance are keyed by
the image's exact BLAKE3 content ID. This means:

- Exact duplicates share metadata.
- Persisted aliases can reconnect renamed content after an index refresh.
- Replacement bytes at the same path do not inherit stale metadata after an
  index refresh.
- One image can keep consistent metadata across multiple playlists.

## AI-assisted tagging

Aurora can ask an OpenAI-compatible vision endpoint to tag one image or a
batch. Supply the API base URL without `/chat/completions`; Aurora appends it.
HTTPS is required unless `--allow-http` is explicitly used for a trusted
endpoint.

Standalone tagging of an explicit file can run by itself. Applying tags to a
playlist, including batch tagging, persists through the running daemon.

The API key comes from `PYLON_KEY` by default, another variable selected with
`--api-key-env`, or a user-protected file selected with `--api-key-file`. A key
file takes precedence.

```powershell
$env:PYLON_KEY = "..."
aurora-ctl autotag "D:\Wallpapers\lake.jpg" --base-url "https://gateway.example/v1" --apply-playlist favorites
aurora-ctl autotag-batch "D:\Wallpapers" --playlist catalog --base-url "https://gateway.example/v1"
aurora-ctl autotag-batch --manifest scan.json --playlist catalog --base-url "https://gateway.example/v1"
```

Batch input is either a folder or a manifest. A manifest is a JSON object with
a `rows` array; only rows with `"status": "ok"` are processed. Those rows
require `absolute_path` and can include a 64-hex-character `sha256` (normalized
to lowercase), `width`, and `height`:

```json
{
  "rows": [
    {
      "status": "ok",
      "absolute_path": "D:\\Wallpapers\\lake.jpg",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
      "width": 3840,
      "height": 2160
    }
  ]
}
```

Existing content metadata and backward-compatible playlist metadata drive
resume. Images that already carry either are skipped unless `--force` is used.
The append-only JSONL resume file records tagged, skipped, and failed paths;
failed items remain retryable.

## Scheduling behavior

In `interval` mode, cadence is measured from the last successful wallpaper
change, including a manual change. If Aurora starts with an existing wallpaper,
it starts that cadence without immediately replacing the image. A failed
automatic apply is retried after 30 seconds, and the delay doubles with each
further failure, up to 30 minutes. Duplicate automatic requests are coalesced.
The scheduler sleeps until the next swap is due instead of polling.

In `at` mode, each configured `at "HH:MM"` slot is recorded only after a
successful apply, preventing duplicate successful fires while allowing a
failure to retry during that minute. Fullscreen and idle policies suppress
automatic scheduling while active. A held-back swap is checked again every 15
seconds. `aurora-ctl pause` also blocks automatic and
workspace-triggered changes, while manual `next`, `prev`, and `set` commands
remain available.

## Configuration and storage

Aurora keeps its state together under `%APPDATA%\aurora`:

| File | Purpose |
| --- | --- |
| `config.kdl` | Sources, scheduling, transitions, animation, monitors, metrics, and cache limits |
| `playlists.kdl` | Playlist membership, order, shuffle, frequency, and compatibility metadata copies |
| `content.json` | Versioned tags, ratings, dimensions, aliases, dynamic playlist markers and filters, and autotag provenance |
| `index-cache.json` | Validated photo index used for fast restart and reload |
| `bans.txt` | Exact content hashes that Aurora must not display |
| `autotag-batch.jsonl` | Default append-only batch audit trail |
| `playlist-content.txn.json` | Transient recovery marker for coordinated playlist/content updates |
| `aurora.log` | Daemon log when started without a console (rotated to `aurora.log.1` at 2 MB) |

Single-file stores are written through synchronized temporary files before
replacement. Updates spanning both `playlists.kdl` and `content.json` first
commit a recovery marker; startup, reload, or the next mutation finishes an
interrupted committed install. The marker is normally removed immediately
after both files are installed.

`aurora-ctl reload` refreshes configured sources, playlists, content metadata,
and bans together. Schedule, transition, animation, monitor, cache-budget,
metrics, and log-level changes require a daemon restart.

## Development

The same checks run locally and in GitHub Actions:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Aurora is Windows-only and under active development.

## License

Aurora is available under the [MIT License](LICENSE).

## Companion projects

- [`wiri`](https://github.com/jolionlands/wiri) - Windows tiling window manager;
  Aurora can rotate on wiri workspace changes.
- [`crest`](https://github.com/jolionlands/crest) - modular status bar.
