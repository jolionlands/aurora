# aurora

Efficient desktop background swapper for Windows.

- Cycles through a directory tree of photos (JPEG, PNG, GIF, WebP, AVIF, BMP, TIFF, ICO native; HEIC via Windows WIC when available)
- Per-monitor wallpapers via `IDesktopWallpaper` COM (Windows position mode is global)
- Multiple transition styles (crossfade, slide, wipe, dissolve, zoom) — GPU-accelerated via Direct2D or CPU fallback
- IPC over a length-prefixed JSON named pipe (`\\.\pipe\aurora`) with `aurora-ctl` client
- Schedule modes: interval, at-time, on-idle, on-wiri-workspace-change
- KDL config at `%APPDATA%\aurora\config.kdl`
- Prometheus `/metrics` endpoint (opt-in) for monitoring
- LRU decode cache + prefetch for instant transitions
- Single-instance check, autostart hook

## Status

Early scaffolding. See `docs/PLAN.md` for the full design.

## Pairs with

- [`wiri`](https://github.com/jolionlands/wiri) — Windows tiling window manager. Aurora subscribes to wiri's IPC event stream for workspace-change triggers.
- [`crest`](https://github.com/jolionlands/crest) — modular status bar.
