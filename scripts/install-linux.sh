#!/usr/bin/env bash
set -euo pipefail
aurora_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$aurora_root"
if [[ "$(uname -s)" != Linux ]]; then
    printf '%s\n' 'This installer is for KDE Plasma 6 on Linux.' >&2
    exit 1
fi
cargo build --release --bins -j "${CARGO_BUILD_JOBS:-2}"
install -Dm755 target/release/aurora "$HOME/.local/bin/aurora"
install -Dm755 target/release/aurora-ctl "$HOME/.local/bin/aurora-ctl"
if [[ "${1:-}" == --autostart ]]; then
    "$HOME/.local/bin/aurora" --register-autostart
fi
printf '%s\n' 'Installed aurora and aurora-ctl into ~/.local/bin.'
printf '%s\n' 'Run aurora, edit ~/.config/aurora/config.kdl, then use aurora-ctl set <path>.'
