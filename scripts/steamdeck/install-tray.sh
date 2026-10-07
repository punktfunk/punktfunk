#!/usr/bin/env bash
# punktfunk — build and install the host's status tray on SteamOS (called by install.sh and
# update.sh; safe to run by hand). User-scoped, where the packages use /usr: the binary in
# ~/.local/bin, an XDG autostart entry, the launcher, and the hicolor status icons.
#
# Best-effort: a failure warns and exits 0. The host runs the same without a tray.
set -euo pipefail

ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }

SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
TARGET_DIR="$SRC/target-steamos"
BUILT="$TARGET_DIR/release/punktfunk-tray"
TRAY="$HOME/.local/bin/punktfunk-tray"
LINUX="$SRC/packaging/linux"

# Its own cargo invocation: built beside the host, feature unification moves the tray's zbus onto
# tokio and it panics at start (see packaging/arch/PKGBUILD).
distrobox enter "$BOX" -- bash -lc "set -e
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR'
cd '$SRC' && cargo build -r -p punktfunk-tray" </dev/null \
    || { warn "the status tray didn't build — the host runs without it"; exit 0; }
if { ldd "$BUILT" 2>/dev/null || true; } | grep -q 'not found'; then
    warn "the status tray built, but SteamOS can't load it — skipped"
    exit 0
fi

install -Dm0755 "$BUILT" "$TRAY"
# `Exec=` expands no variables, so both entries name this install's absolute path.
sed "s|/usr/bin/punktfunk-tray|$TRAY|" "$LINUX/io.unom.Punktfunk.Tray.desktop" \
    | install -Dm0644 /dev/stdin "$HOME/.config/autostart/io.unom.Punktfunk.Tray.desktop"
sed "s|/usr/bin/punktfunk-tray|$TRAY|" "$LINUX/io.unom.Punktfunk.StartHost.desktop" \
    | install -Dm0644 /dev/stdin "$HOME/.local/share/applications/io.unom.Punktfunk.StartHost.desktop"
for png in "$LINUX"/icons/hicolor/*/apps/punktfunk-tray*.png; do
    install -Dm0644 "$png" "$HOME/.local/share/icons/${png#"$LINUX"/icons/}"
done
ok "status tray: $TRAY (shows in the Desktop-mode panel from the next login)"
