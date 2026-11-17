#!/usr/bin/env bash
# One-shot setup so the punktfunk host can INJECT INPUT while streaming the Bazzite KDE *Desktop*
# session. Run ONCE as the streaming user (no root needed). Gaming Mode (gamescope) needs none of
# this — it auto-attaches. Idempotent: safe to re-run.
#
#   bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh
#
# The VIRTUAL OUTPUT (video) needs no setup: the host package ships io.unom.punktfunk.host.desktop,
# whose X-KDE-Wayland-Interfaces grants the host KWin's restricted zkde_screencast protocol (and two
# others) on a normal interactive Plasma session — least-privilege (only the host, only the listed
# interfaces), the same mechanism krfb/krdp use. No session-wide KWIN_WAYLAND_NO_PERMISSION_CHECKS
# hack is needed. KWin caches the grant per-executable on first connect, so after a FRESH host
# install log out + back into the Desktop session once so KWin re-reads the file.
#
# The one thing a normal KDE login still lacks is the `kde-authorized` RemoteDesktop grant — so the
# host's libei input setup auto-approves instead of popping an "Allow remote control?" dialog the
# headless host can't answer. That's what this script seeds.
set -euo pipefail

GRANT_SRC="${PUNKTFUNK_GRANT_SRC:-/usr/share/punktfunk/headless/kde-authorized}"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"
# Older versions of this script wrote a session-wide KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 env file to
# unlock screencast. The shipped .desktop replaces it; remove the stale, over-broad override.
STALE_ENVD="$HOME/.config/environment.d/10-punktfunk-kwin.conf"

echo "punktfunk: KDE Desktop-mode input setup"

if [[ -f "$STALE_ENVD" ]] && grep -q KWIN_WAYLAND_NO_PERMISSION_CHECKS "$STALE_ENVD" 2>/dev/null; then
    rm -f "$STALE_ENVD"
    echo "  removed stale $STALE_ENVD (screencast is now granted via the shipped .desktop)"
fi

# RemoteDesktop portal grant for headless libei input (never clobber an existing one).
if [[ -s "$GRANT_DST" ]]; then
    echo "  grant DB already present ($GRANT_DST) — leaving it"
elif [[ -s "$GRANT_SRC" ]]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    cp "$GRANT_SRC" "$GRANT_DST"
    echo "  seeded RemoteDesktop grant -> $GRANT_DST"
    # Pick it up now without a relogin (the portal store caches at start).
    systemctl --user restart xdg-permission-store 2>/dev/null || true
    systemctl --user restart plasma-xdg-desktop-portal-kde xdg-desktop-portal-kde 2>/dev/null || true
    systemctl --user restart xdg-desktop-portal 2>/dev/null || true
else
    echo "  WARN: grant source not found at $GRANT_SRC — input will need a manual portal approval" >&2
fi

echo "punktfunk: done. On a fresh host install, log out + back into the KDE Desktop session once"
echo "          (so KWin authorizes the host's virtual output), then connect a client in Desktop Mode."
