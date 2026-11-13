#!/usr/bin/env bash
# punktfunk stream runner — the target of the non-Steam shortcuts the plugin creates.
#
# WHY A WRAPPER SCRIPT (load-bearing, from MoonDeck's hard-won knowledge): the stream client
# must be a descendant of the process Steam launches via `reaper`, or gamescope never gives
# its window focus/fullscreen in Gaming Mode (gamescope detects the "current app" by AppID,
# which only attaches to reaper's descendants — see gamescope#484). So the Decky plugin
# launches THIS script through SteamClient.Apps.RunGame; the script then runs the client,
# which inherits the shortcut's AppID and is focused. Launching the client directly from the
# (root) Decky backend produces an unfocused, invisible window.
#
# Per-session parameters arrive as environment variables, set as the shortcut's Steam launch
# options by the plugin (SteamClient.Apps.SetAppLaunchOptions), so ONE generic shortcut serves
# every host:
#   PF_REF     host reference — a saved host's stable id, or addr[:port]  (required to stream)
#   PF_PRESET  preset id for a pinned card (old name: PF_PROFILE)         (optional)
#   PF_GAME    store-qualified library id (steam:570) the host launches into the stream —
#              set by a stream started from a Steam game's page          (optional)
#   PF_AS      profile id to play as, for this stream only               (optional)
#   PF_BROWSE  non-empty = open the client's console home instead of streaming
#   PF_APPID   flatpak app id                        (default io.unom.punktfunk)
#   PF_FLATPAK override the flatpak binary path      (default: `flatpak` on PATH)
#   PF_CLIENT_BIN  absolute path of a NATIVE client  (optional; set by the plugin when it
#                  resolved a non-flatpak install — then the client is run directly and
#                  PF_APPID/PF_FLATPAK are unused)
#
# A REFERENCE, NEVER A VALUE. Host refs and preset ids are the only things that ride this
# channel; no resolution, bitrate or codec ever does. The client resolves both against its own
# stores, which is what keeps a Steam launch option from becoming a second settings surface.
# The plugin validates them to space/quote-free ASCII before they reach Steam's tokenizer.
#
# Runs as the `deck` user (Steam launched it), so the --user flatpak install is visible and
# WAYLAND_DISPLAY / XDG_RUNTIME_DIR are already correct for gamescope.
#
# NO EXEC BIT REQUIRED: the Steam shortcut's exe is `/bin/sh` and this script rides behind
# `%command%` as an argument (see src/steam.ts). Decky extracts plugin zips without preserving
# permission bits and ~/homebrew/plugins is root-owned (the unprivileged plugin backend can't
# chmod), so the launch path must never depend on +x. Keep this script POSIX-sh clean.
set -u

APPID="${PF_APPID:-io.unom.punktfunk}"
FLATPAK="${PF_FLATPAK:-flatpak}"
# A Deck still on the CamelCase id runs that until its next client update rebases it.
if [ -z "${PF_APPID:-}" ] && [ -z "${PF_CLIENT_BIN:-}" ] \
    && ! "$FLATPAK" info "$APPID" >/dev/null 2>&1 \
    && "$FLATPAK" info io.unom.Punktfunk >/dev/null 2>&1; then
    APPID=io.unom.Punktfunk
fi

# Steam's launch environment is a game's. Its LD_LIBRARY_PATH names the Steam runtime and
# shim libraries a native client must not load, and LD_PRELOAD is the overlay hook. Steam
# keeps the pre-launch value in SYSTEM_LD_LIBRARY_PATH; `flatpak run` drops both itself.
if [ -n "${SYSTEM_LD_LIBRARY_PATH+x}" ]; then
    export LD_LIBRARY_PATH="$SYSTEM_LD_LIBRARY_PATH"
fi
unset LD_PRELOAD

# The flatpak manifest binds gamescope's socket as `gamescope-0`. Where Steam's gamescope is a
# later one (Armada starts a splash gamescope first), the vendored WSI layer reaches nobody:
# no HDR, no bypass. Hand the sandbox the live name and its socket.
FLATPAK_ARGS=""
case "${GAMESCOPE_WAYLAND_DISPLAY:-}" in
    gamescope-[0-9] | gamescope-[0-9][0-9])
        FLATPAK_ARGS="--env=GAMESCOPE_WAYLAND_DISPLAY=$GAMESCOPE_WAYLAND_DISPLAY \
--filesystem=xdg-run/$GAMESCOPE_WAYLAND_DISPLAY"
        ;;
esac

# The client is not always the flatpak: a sysext, a .deb/.rpm, an AUR build or a nix profile
# installs a native `punktfunk-client` with the CLI as its sibling, and the plugin passes the
# client's absolute path here when that is what it resolved.
#
# run_cli execs the HEADLESS CLI (`punktfunk`); run_session execs the GTK/console shell
# (`punktfunk-client`). Both live in the same place in both install kinds — /app/bin inside the
# flatpak, reachable with `--command=`, and one bindir natively.
run_cli() {
    if [ -n "${PF_CLIENT_BIN:-}" ]; then
        # `${VAR%/*}` rather than `dirname`: pure parameter expansion, so this works with no
        # PATH at all — which is the environment a Steam launch option can leave us in.
        exec "${PF_CLIENT_BIN%/*}/punktfunk" "$@"
    fi
    # shellcheck disable=SC2086 # FLATPAK_ARGS is two options, split on purpose
    exec "$FLATPAK" run $FLATPAK_ARGS --command=punktfunk "$APPID" "$@"
}

run_session() {
    if [ -n "${PF_CLIENT_BIN:-}" ]; then
        exec "$PF_CLIENT_BIN" "$@"
    fi
    # shellcheck disable=SC2086
    exec "$FLATPAK" run $FLATPAK_ARGS "$APPID" "$@"
}

# What we are about to run, for the log line each branch prints.
CLIENT_LABEL="${PF_CLIENT_BIN:-$APPID}"

# The console home: the client's own gamepad UI (host picker, pairing, add-host by address, the
# library browser and the full settings screen). UNCHANGED from before this rework — the shell
# binary already execs the session for `--browse`, so there is nothing to repoint here.
if [ -n "${PF_BROWSE:-}" ]; then
    echo "punktfunkrun: gamepad UI $CLIENT_LABEL --browse (console home)" >&2
    run_session --browse --fullscreen
fi

if [ -z "${PF_REF:-}" ]; then
    echo "punktfunkrun: PF_REF is not set (the plugin sets it as a launch option)" >&2
    exit 2
fi

set -- --fullscreen
# PF_PROFILE is the old name, still in shortcuts an older plugin wrote. `--profile` is the
# spelling every client version accepts.
PF_PRESET="${PF_PRESET:-${PF_PROFILE:-}}"
if [ -n "$PF_PRESET" ]; then
    set -- --profile "$PF_PRESET" "$@"
fi
# A title to launch into the stream: the CLI hands the id to the host, which resolves it
# against its own library — the Deck never learns what the launch recipe is.
if [ -n "${PF_GAME:-}" ]; then
    set -- --game "$PF_GAME" "$@"
fi
if [ -n "${PF_AS:-}" ]; then
    set -- --as "$PF_AS" "$@"
fi

# The ordinary stream. `--exec` is the documented gamescope-wrapper mode: the CLI becomes the
# session, so the process tree stays flat and Steam's "game" ends exactly when the stream does.
echo "punktfunkrun: streaming $CLIENT_LABEL launch $PF_REF" >&2
run_cli launch "$PF_REF" --exec "$@"
