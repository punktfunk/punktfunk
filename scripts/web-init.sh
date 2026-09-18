#!/bin/sh
# Pre-start for the punktfunk web console (run by punktfunk-web-init.service as the user):
#
#   1. generate the login password once, in the streaming user's config dir, and surface it to
#      the journal;
#   2. name the console's bind in host.env once, so the default can change without taking a
#      running install's console off the LAN with it;
#   3. wait for the host's first-run artifacts, so the console is not started before the files it
#      cannot start without exist.
#
# The mgmt token is NOT created here — the host owns it (~/.config/punktfunk/mgmt-token); this
# script only waits for it.
#
# Usage: web-init.sh [WAIT_SECONDS]   (default 30; the CI gate passes a small value)
set -eu

DIR="${XDG_CONFIG_HOME:-$HOME/.config}/punktfunk"
mkdir -p "$DIR"
chmod 700 "$DIR" 2>/dev/null || true
PWFILE="$DIR/web-password"

# The generated password is written in clear and read back ONCE. The console salts and hashes it
# the first time it signs someone in and rewrites this file with the hash alone, so the window to
# read it is between this line and that sign-in; after it, a forgotten password is reset, not read.
if [ ! -s "$PWFILE" ]; then
    # URL/shell-safe password (no /+= so it's a clean EnvironmentFile value).
    PW=$(head -c 18 /dev/urandom | base64 | tr -d '/+=' | cut -c1-20)
    (umask 077; printf 'PUNKTFUNK_UI_PASSWORD=%s\n' "$PW" > "$PWFILE")
    chmod 600 "$PWFILE" 2>/dev/null || true
    # Do NOT echo the password itself. Anything this script prints is captured by systemd into
    # the PERSISTENT journal, which on Debian/Ubuntu is readable by the `adm` and
    # `systemd-journal` groups — so printing it published a 0600 secret to every member of them,
    # permanently. Point at the file instead: it is correctly 0600, and it stays readable only by
    # the user who owns the console.
    echo "punktfunk web console login password generated."
    echo "Read it NOW, before your first sign-in:  sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' $PWFILE"
    echo "(then open https://<host-ip>:47992 and log in — the console stores a hash after that)"
fi

# ------------------------------------------------------------------------- name the bind, once
#
# The console listens on every interface and answers only peers on the local network or a VPN.
# Write that down once so the operator finds the setting in host.env; an existing line always wins.
# Runs before punktfunk-web.service reads host.env — the ordering punktfunk-web-init.service is for.
HOST_ENV="$DIR/host.env"
if ! grep -q '^[[:space:]]*PUNKTFUNK_UI_BIND=' "$HOST_ENV" 2>/dev/null; then
    printf '\n# Where the web console listens: 0.0.0.0 (your network, never the internet), 127.0.0.1\n# (this machine only), or one address, e.g. a VPN interface.\nPUNKTFUNK_UI_BIND=0.0.0.0\n' >> "$HOST_ENV"
    echo "host.env: the web console answers on your local network (PUNKTFUNK_UI_BIND=0.0.0.0)."
    echo "To keep it to this machine, set PUNKTFUNK_UI_BIND=127.0.0.1 there and restart punktfunk-web."
fi

# ---------------------------------------------------------------- wait for the host's first run
#
# The console cannot START without two things the HOST creates on its first `serve`: `mgmt-token`
# (a MANDATORY EnvironmentFile on punktfunk-web.service) and the TLS pair `Bun.serve` listens with.
#
# punktfunk-web.service already says `After=punktfunk-host.service`, but that is not a readiness
# gate: the host is `Type=simple`, so systemd considers it started the instant it is SPAWNED —
# seconds before either file lands. Worse, the two are written far apart. `mgmt-token` is persisted
# early in `serve` (main.rs, before the listeners), while the identity cert comes last, inside
# `mgmt::run` -> `identity::load_or_adopt`. So waiting on the token alone would only move the
# failure to the cert.
#
# Losing that race was a HARD systemd-level failure — "Failed to load environment files: No such
# file or directory", result 'resources' — on the very first enable of a perfectly good install,
# and only the Restart loop brought the console up two seconds later. Field report 2026-08-28
# (Omarchy): `punktfunk-omarchy setup` enables the host and the console back to back, so it lost
# the race every time and reported "Failed to start punktfunk management web console".
#
# This unit is `Type=oneshot` and punktfunk-web.service is ordered `After=` it, so blocking HERE is
# the readiness gate that ordering already claimed to be — no new unit, no new directive, and the
# check is the console's own precondition rather than a proxy for it.
#
# Steady state is free: the host PERSISTS all of these, so every start after its first run passes
# on the first pass without sleeping.

# Exactly what web/nitro-entry/tls-paths.mjs requires to serve: a token, plus a cert/key pair from
# ONE directory — the native pair (what a current host mints) or the legacy pair (a host that
# never took the identity split). Non-empty, not merely present, for the same reason tls-paths.mjs
# tests size: `write_secret_file` is create+truncate+write, so a 0-byte file is a live mid-write.
host_ready() {
    [ -s "$DIR/mgmt-token" ] || return 1
    if [ -s "$DIR/native-cert.pem" ] && [ -s "$DIR/native-key.pem" ]; then return 0; fi
    if [ -s "$DIR/cert.pem" ] && [ -s "$DIR/key.pem" ]; then return 0; fi
    return 1
}

# ponytail: a 1 s poll, not sd_notify. Making the host `Type=notify` is the real readiness
# protocol, but it locks the unit file to the binary — and the documented install route copies
# scripts/punktfunk-host.service into ~/.config/systemd/user BY HAND, so a new unit beside an
# older host would hang for TimeoutStartSec and then fail to start at all. Upgrade to notify once
# the units only ever ship alongside the binary that answers them.
WAIT_SECS="${1:-30}"
waited=0
while ! host_ready; do
    if [ "$waited" -ge "$WAIT_SECS" ]; then
        # Not fatal: exit 0 so the console still starts and its own Restart=always retry takes
        # over (the pre-existing behaviour). Say WHY, because the bare systemd error this replaces
        # named a missing file and never the host that owes it.
        echo "punktfunk-host has not written its mgmt token + identity cert after ${WAIT_SECS}s."
        echo "Starting the console anyway — it retries every 2s until they appear."
        echo "If it stays down:  systemctl --user status punktfunk-host"
        break
    fi
    if [ "$waited" -eq 0 ]; then
        echo "waiting for punktfunk-host to write its mgmt token + identity cert..."
    fi
    sleep 1
    waited=$((waited + 1))
done
