#!/usr/bin/env bash
# Headless KDE Plasma session for the punktfunk host (no KMS scanout → kwin --virtual).
#
# Brings up the full desktop, not just the compositor, and waits for it to be *actually
# ready* before starting the portals/plasma — no blind `sleep`. A bare session is missing
# pieces a full Plasma login would start, so this also: exports the Xwayland DISPLAY (X11 apps
# like Steam need it), starts the polkit authentication agent (Discover/PackageKit need it to
# authorize installs), and supervises plasmashell (it draws wallpaper + panels; a crash must
# not leave the desktop gone). The env matters: without XDG_MENU_PREFIX=plasma- the launcher
# resolves ${XDG_MENU_PREFIX}applications.menu → "applications.menu", which doesn't exist on KDE
# installs (it ships plasma-applications.menu) — plasmashell runs fine but the menu is EMPTY.
#
#   bash scripts/headless/run-headless-kde.sh [WxH]    # default 1920x1080
#
# Then in another shell:
#   WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE \
#     punktfunk-host punktfunk1-host --source virtual --seconds 14400   # zero-copy is on by default
set -euo pipefail

RES="${1:-1920x1080}"
W="${RES%x*}"
H="${RES#*x}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
export XDG_CURRENT_DESKTOP=KDE
export XDG_MENU_PREFIX=plasma-
export KDE_FULL_SESSION=true
export KDE_SESSION_VERSION=6
export DESKTOP_SESSION=plasma
export WAYLAND_DISPLAY=wayland-kde
export KWIN_WAYLAND_NO_PERMISSION_CHECKS=1

# The probe binary (gates readiness on KWin actually exposing zkde_screencast — not merely
# the socket existing). Prefer a source-tree build (dev box), then a packaged install on PATH
# (the RPM/.deb put it at /usr/bin, where there is no $ROOT/target or Cargo.toml), then
# `cargo run` as a last resort.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -x "$ROOT/target/release/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/release/punktfunk-host" probe-compositor)
elif [[ -x "$ROOT/target/debug/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/debug/punktfunk-host" probe-compositor)
elif command -v punktfunk-host >/dev/null 2>&1; then
    PROBE=(punktfunk-host probe-compositor)
else
    PROBE=(cargo run -q --manifest-path "$ROOT/Cargo.toml" -p punktfunk-host -- probe-compositor)
fi

# kwin to its own log (so its EGL/GPU-init errors are captured, not lost to the terminal).
# `--xwayland`: enable Xwayland so X11 apps (Discord/Electron, Steam, many launchers) work. Without
# it `kwin_wayland --virtual` brings up NO X server at all (no display reserved), and those apps die
# with "Missing X Server or $DISPLAY". KWin starts Xwayland on demand but reserves + logs the X11
# display up front, which the detection below reads.
# The log lives in the per-user 0700 XDG_RUNTIME_DIR, not at a fixed name in a world-writable
# /tmp. This file is not just a log: the DISPLAY detection below GREPS it for "Using public X11
# display :N" and exports the result, so at a predictable path in a shared directory any local user
# could pre-create it (or symlink it) and steer the DISPLAY of a shipped systemd service.
# `pf-vdisplay` already resolves XDG_RUNTIME_DIR for its own paths; this matches. Without a
# runtime dir, fall back to a private mktemp rather than a guessable name.
if [[ -n "${XDG_RUNTIME_DIR:-}" && -d "${XDG_RUNTIME_DIR}" ]]; then
    KWIN_LOG="${XDG_RUNTIME_DIR}/punktfunk-kwin.log"
    : >"$KWIN_LOG"
    chmod 600 "$KWIN_LOG"
else
    KWIN_LOG="$(mktemp -t punktfunk-kwin.XXXXXXXX.log)"
fi
kwin_wayland --virtual --xwayland --width "$W" --height "$H" --no-lockscreen \
    --socket "$WAYLAND_DISPLAY" >"$KWIN_LOG" 2>&1 &
KWIN_PID=$!

# Active readiness wait: poll until KWin is up AND advertises the zkde_screencast global
# (what the virtual-output backend needs), or fail fast with a useful message. kwin can also
# exit immediately if EGL/GPU init fails — catch that.
echo "waiting for KWin ($RES) to become ready…"
DEADLINE=$(( SECONDS + 30 ))
until "${PROBE[@]}" >/dev/null 2>&1; do
    if ! kill -0 "$KWIN_PID" 2>/dev/null; then
        echo "ERROR: kwin_wayland exited during startup — see $KWIN_LOG:" >&2
        tail -n 20 "$KWIN_LOG" >&2 || true
        exit 1
    fi
    if (( SECONDS >= DEADLINE )); then
        echo "ERROR: KWin did not become ready within 30s. Last probe:" >&2
        "${PROBE[@]}" >&2 || true
        exit 1
    fi
    sleep 0.5
done
echo "KWin ready."

# Detect the Xwayland display KWin started and export it. Without DISPLAY, X11 apps launched in
# this session (Steam, many games/launchers) fail with "can't open display" even though Xwayland
# is running — KWin sets DISPLAY only for its own children, not for apps launched via the
# plasma menu / D-Bus activation. KWin brings Xwayland up a moment after itself; poll for it.
DISPLAY_NUM=""
for _ in $(seq 1 40); do
    # `|| true`: under `set -euo pipefail`, grep/pgrep exit non-zero when nothing's there yet
    # (common a few seconds into a systemd-launched boot), which would abort the WHOLE script —
    # killing KWin — on the first iteration instead of retrying. Tolerate it so the loop polls,
    # and so a session with no Xwayland at all still proceeds (DISPLAY just stays unset → warn).
    # Primary: KWin reserves + logs the X11 display ("Using public X11 display :N") up front, even
    # with Xwayland-on-demand (no Xwayland *process* until the first X client connects), so read
    # the log; fall back to a live Xwayland process for older KWin.
    d=$(grep -oE 'Using public X11 display :[0-9]+' "$KWIN_LOG" 2>/dev/null | grep -oE '[0-9]+' | head -1 || true)
    if [[ -n "$d" ]]; then DISPLAY_NUM="$d"; break; fi
    d=$(pgrep -a Xwayland 2>/dev/null | grep -oE ' :[0-9]+' | tr -d ' :' | head -1 || true)
    if [[ -n "$d" && -S "/tmp/.X11-unix/X$d" ]]; then DISPLAY_NUM="$d"; break; fi
    # Most reliable here: KWin --xwayland reserves the /tmp/.X11-unix/X<N> socket up front (Xwayland
    # starts on demand on first connect) and on this KWin neither logs the display nor runs a process
    # until then. The socket IS the signal — take it. (Fresh boot has a tmpfs /tmp, so no stale one.)
    s=$(ls /tmp/.X11-unix/X[0-9]* 2>/dev/null | head -1 || true)
    if [[ -n "$s" && -S "$s" ]]; then DISPLAY_NUM="${s##*/X}"; break; fi
    sleep 0.25
done
if [[ -n "$DISPLAY_NUM" ]]; then
    export DISPLAY=":$DISPLAY_NUM"
    echo "Xwayland on $DISPLAY"
else
    echo "WARN: no Xwayland display detected — X11 apps (Steam) won't open a display" >&2
fi

# Only NOW restart the portals, and against the correct env: the xdg-desktop-portal chain
# binds the compositor that existed when it started, so a stale portal points at a dead
# socket and RemoteDesktop/EIS (input injection) times out. Import the session env into the
# systemd/D-Bus activation environment FIRST (prepare-session.sh does the same for Sway;
# without it the restarted portal can inherit an empty WAYLAND_DISPLAY) — including DISPLAY so
# D-Bus-activated X11 apps inherit it — then restart.
systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR ${DISPLAY:+DISPLAY} 2>/dev/null || true
dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS ${DISPLAY:+DISPLAY} 2>/dev/null || true

# Pre-seed the RemoteDesktop grant for headless input injection (libei). KWin's xdg portal would
# otherwise pop an "Allow remote control?" dialog on every session Start() — which a headless host
# can't answer, so EIS setup times out and input silently fails. The `kde-authorized` permission-
# store table (this tiny GVariant DB, shipped next to this script) pre-authorizes it. Copy it in if
# the user has none yet; never clobber an existing one.
DB="$HOME/.local/share/flatpak/db/kde-authorized"
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ ! -s "$DB" && -s "$SELF_DIR/kde-authorized" ]]; then
    mkdir -p "$(dirname "$DB")" && cp "$SELF_DIR/kde-authorized" "$DB"
    echo "seeded RemoteDesktop grant: $DB"
fi

# Virtual "Punktfunk" speaker: a null sink (shipped next to this script) that the host captures +
# streams, set default so desktop audio goes there instead of a real/AirPlay device — a headless
# host has no speakers, and on a LAN with AirPlay gear PipeWire otherwise picks a random HomePod.
# pipewire reads its own config at start, so on FIRST install (config not yet present) restart it
# once to load the sink; later boots already have it. (Also disable AirPlay discovery out of band:
# `sudo dnf remove pipewire-config-raop`.)
PWSINK="$HOME/.config/pipewire/pipewire.conf.d/50-punktfunk-sink.conf"
if [[ ! -s "$PWSINK" && -s "$SELF_DIR/punktfunk-sink.conf" ]]; then
    mkdir -p "$(dirname "$PWSINK")" && cp "$SELF_DIR/punktfunk-sink.conf" "$PWSINK"
    echo "installed Punktfunk virtual speaker → restarting pipewire to load it"
    systemctl --user restart pipewire 2>/dev/null || true
fi

# Reach graphical-session.target so xdg-desktop-portal (which is ordered After / fails its start
# job without it) can come up — a headless linger session never gets there on its own, and Fedora's
# target carries RefuseManualStart=yes, so drop that in once. Without the portal, libei input fails.
GST_DROPIN="$HOME/.config/systemd/user/graphical-session.target.d"
if [[ ! -f "$GST_DROPIN/10-punktfunk-headless.conf" ]]; then
    mkdir -p "$GST_DROPIN"
    printf '[Unit]\nRefuseManualStart=no\n' > "$GST_DROPIN/10-punktfunk-headless.conf"
    systemctl --user daemon-reload 2>/dev/null || true
fi
systemctl --user start graphical-session.target 2>/dev/null || true

# Bring the portal up against the env imported above. `start`/`restart`, not `try-restart`, a no-op
# when inactive (the headless first-boot case), so it actually comes up; `--no-block` since
# xdg-desktop-portal is Type=dbus and blocks ~30s waiting for its bus name, and it only needs to be
# ready before the FIRST client streams (seconds later), not before plasmashell.
systemctl --user --no-block restart plasma-xdg-desktop-portal-kde.service 2>/dev/null || true
systemctl --user --no-block start xdg-desktop-portal.service 2>/dev/null || true

# Polkit authentication agent: without it, Discover / PackageKit can't get authorization to
# install packages (polkitd is the policy engine; the *agent* is the GUI prompt). A full Plasma
# session starts it; our bare session must do it explicitly. Force the Qt Wayland platform — it
# exits immediately if it can't pick one. comm is truncated to 15 chars ("polkit-kde-auth").
POLKIT_AGENT=/usr/lib/x86_64-linux-gnu/libexec/polkit-kde-authentication-agent-1
if [[ -x "$POLKIT_AGENT" ]] && ! pgrep -x polkit-kde-auth >/dev/null; then
    QT_QPA_PLATFORM=wayland setsid "$POLKIT_AGENT" \
        >"${TMPDIR:-/tmp}/punktfunk-polkit-agent.log" 2>&1 &
fi

kbuildsycoca6 >/dev/null 2>&1 || true # rebuild the menu cache under the correct env

# Supervise plasmashell: it draws the desktop (wallpaper + panels). It can crash (a transient
# GPU/QML hiccup), and a single unsupervised `plasmashell &` then leaves the streamed session
# with only app windows — no desktop, no wallpaper, no launcher. Restart it for as long as KWin
# lives, so the desktop self-heals.
( while kill -0 "$KWIN_PID" 2>/dev/null; do
      plasmashell
      echo "plasmashell exited — restarting in 2s" >&2
      sleep 2
  done ) &

echo "headless KDE up on $WAYLAND_DISPLAY ($RES), kwin pid $KWIN_PID (log: $KWIN_LOG)"
wait "$KWIN_PID"
