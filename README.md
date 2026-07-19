# aurora

Efficient desktop background swapper for Windows.

- Bundled defaults are JPEG, PNG, GIF, WebP, BMP, TIFF, and ICO. Explicitly configured or direct AVIF, HEIC, and HEIF files may work only when Windows WIC and the file's AV1 or HEVC payload codec can decode that exact file
- Per-monitor wallpapers via `IDesktopWallpaper` COM (Windows position mode is global)
- Multiple transition styles (crossfade, slide, wipe, dissolve, zoom) — GPU-accelerated via Direct2D or CPU fallback
- IPC over a length-prefixed JSON named pipe (`\\.\pipe\aurora-<session-id>`) with `aurora-ctl` client
- Interval or fixed-time schedules, plus optional wiri workspace-change swaps
- KDL config at `%APPDATA%\aurora\config.kdl`
- Prometheus `/metrics` endpoint (opt-in) for monitoring
- Bounded LRU decode cache
- Single-instance check, autostart hook

## Playlists and tagging

Start the `aurora` daemon before using playlist commands. Batch autotagging and
`autotag --apply-playlist` also persist through the daemon; standalone autotagging
of an explicit file can run without it.

```powershell
aurora-ctl playlist create favorites
aurora-ctl playlist add favorites current
aurora-ctl playlist tag favorites current --kind theme night neon
aurora-ctl playlist tag favorites current --kind artist studio-name
aurora-ctl playlist rate favorites current 4
aurora-ctl playlist frequency favorites current 2
aurora-ctl playlist shuffle favorites true
aurora-ctl playlist activate favorites
aurora-ctl playlist show favorites --offset 0 --limit 100
aurora-ctl playlist deactivate
```

The built-in tag groups are `general`, `theme`, `content`, `color`, `source`,
`medium`, `safety`, `franchise`, and `character`; any other non-empty kebab-case
group is stored as custom metadata. Omit the tags to clear one group, for example
`aurora-ctl playlist tag favorites current --kind artist`. Tags are metadata,
not selection rules. Rating and frequency affect selection with effective weight
`frequency * (rating + 1)`; both default to weight 1 when unset.

Playlists are persisted at `%APPDATA%\aurora\playlists.kdl`. Paths supplied
through `aurora-ctl` are normalized to absolute paths. Legacy or hand-written
relative entries remain supported and are resolved against each configured
source root.

Autotagging uses an OpenAI-compatible vision endpoint. Pass the API base URL,
without `/chat/completions`; Aurora appends that path. HTTPS is required unless
`--allow-http` is explicitly used for a trusted endpoint. The API key comes from
`PYLON_KEY` by default, another variable selected with `--api-key-env`, or a
user-protected file selected with `--api-key-file` (the file takes precedence).
Each image normally uses two model passes, identity and aesthetics; invalid or
failed responses can trigger retries and fallback prompts, so a run can use more
than two requests.

```powershell
$env:PYLON_KEY = "..."
aurora-ctl autotag "D:\Wallpapers\lake.jpg" --base-url "https://gateway.example/v1" --apply-playlist favorites
aurora-ctl autotag-batch "D:\Wallpapers" --playlist catalog --base-url "https://gateway.example/v1"
aurora-ctl autotag-batch --manifest scan.json --playlist catalog --base-url "https://gateway.example/v1"
```

Batch input is either a folder or a manifest, not both. A manifest is a JSON
object with a `rows` array. Only rows whose `status` is `"ok"` are processed;
those rows require `absolute_path` and may include a 64-hex-digit `sha256`
(normalized to lowercase), `width`, and `height` for duplicate and small-image
filtering:

```json
{"rows":[{"status":"ok","absolute_path":"D:\\Wallpapers\\lake.jpg","sha256":"0000000000000000000000000000000000000000000000000000000000000000","width":3840,"height":2160}]}
```

Playlist metadata is the source of truth for batch resume: already-tagged paths
are skipped unless `--force` is used. The JSONL resume file is an append-only
audit log of tagged, skipped, and failed paths; failures remain retryable. Use
`--resume-file` to select a particular log when separate audit trails are useful.
Single-image playlist apply and batch `--force` replace all metadata for that
path; ordinary batch runs leave existing metadata untouched.

## Status

Under active development.

## Pairs with

- [`wiri`](https://github.com/jolionlands/wiri) — Windows tiling window manager. Aurora subscribes to wiri's IPC event stream for workspace-change triggers.
- [`crest`](https://github.com/jolionlands/crest) — modular status bar.
