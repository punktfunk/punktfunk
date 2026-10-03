#!/usr/bin/env bash
# punktfunk — build the HDR-capable `punktfunk-gamescope` for a SteamOS host (called by
# install.sh and update.sh; safe to run by hand).
#
# Stock gamescope offers no 10-bit PQ formats on its PipeWire capture node, so every session on
# the gamescope backend stays 8-bit SDR. This builds gamescope + punktfunk's `pipewire-hdr`
# patches (the ONE canonical recipe, packaging/gamescope/build-punktfunk-gamescope.sh) inside the
# same Debian-trixie distrobox the host is built in, installs it as
# ~/.local/bin/punktfunk-gamescope, and pins the host to it via PUNKTFUNK_GAMESCOPE_BIN in
# host.env. HDR is on by default in the host; with this binary present a 10-bit-capable client
# streams true HDR10 from Game Mode.
#
# BEST-EFFORT BY DESIGN, mirroring the CI packaging paths: streaming works without it (sessions
# stay SDR), so a failure here warns loudly and exits 0 — it must never cost the host install or
# update it runs inside. The one hard rule is honesty at the seams: host.env points at the binary
# ONLY while `--version` proves it runs on SteamOS glass and carries the `+pfhdr` marker; on any
# failure both the binary and the host.env line are removed (a stale absolute override would
# otherwise break gamescope session spawning entirely, which is worse than SDR).
#
# The build clones gamescope fresh each run (submodules included). That costs bandwidth, but it
# only runs when the packaging/gamescope tree changed (pin bump, patch change — tracked by a
# content stamp) or the installed binary stopped working; a no-op re-run is milliseconds.
set -euo pipefail

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }

SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
GS_BIN="$HOME/.local/bin/punktfunk-gamescope"
STAMP="$HOME/.local/share/punktfunk/gamescope.stamp"
HOST_ENV="$HOME/.config/punktfunk/host.env"
PKGDIR="$SRC/packaging/gamescope"

# host.env may only name the binary while it verifiably works (see header). sed -i is fine: the
# file is ours (install.sh §3) and the line is one this script wrote.
unwire() {
    [ -f "$HOST_ENV" ] || return 0
    if grep -q '^PUNKTFUNK_GAMESCOPE_BIN=' "$HOST_ENV"; then
        sed -i '/^# HDR gamescope built by scripts\/steamdeck/d;/^PUNKTFUNK_GAMESCOPE_BIN=/d' "$HOST_ENV"
        warn "host.env: removed PUNKTFUNK_GAMESCOPE_BIN (no working punktfunk-gamescope) — sessions stream SDR"
    fi
}
wire() {
    [ -f "$HOST_ENV" ] || return 0 # install.sh runs this after §3 wrote host.env; by-hand runs may predate it
    grep -q '^PUNKTFUNK_GAMESCOPE_BIN=' "$HOST_ENV" && return 0
    printf '\n# HDR gamescope built by scripts/steamdeck/build-gamescope.sh (10-bit BT.2020 PQ capture;\n# rebuilt by update.sh; PUNKTFUNK_GAMESCOPE_HDR=0 forces SDR).\nPUNKTFUNK_GAMESCOPE_BIN=%s\n' "$GS_BIN" >> "$HOST_ENV"
    ok "host.env: PUNKTFUNK_GAMESCOPE_BIN=$GS_BIN"
}
# The on-glass truth: the binary must RUN ON STEAMOS (not merely in the build container — this is
# the ABI seam the whole distrobox approach exists for) and carry the +pfhdr banner marker.
verifies() { [ -x "$GS_BIN" ] && "$GS_BIN" --version 2>&1 | head -1 | grep -q '+pfhdr'; }

[ -d "$PKGDIR/patches" ] || { warn "no $PKGDIR — skipping the HDR gamescope build"; exit 0; }

# Content stamp over the packaging tree: a pin bump or patch edit re-triggers the build; anything
# else makes this a fast no-op.
want_stamp="$(find "$PKGDIR" -type f | LC_ALL=C sort | xargs sha256sum | sha256sum | cut -d' ' -f1)"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$want_stamp" ] && verifies; then
    ok "punktfunk-gamescope up to date ($("$GS_BIN" --version 2>&1 | head -1))"
    wire
    exit 0
fi

log "Building punktfunk-gamescope (HDR 10-bit capture; ~5 min, best-effort)"
# gamescope's build deps in the box (trixie names, mirroring packaging/gamescope/PKGBUILD — keep
# the two lists in step). Provisioned here, not in install.sh's main pass, so a dep problem can
# only ever cost this feature. glm/stb come in as meson wraps; wlroots/libliftoff/vkroots/
# libdisplay-info are vendored submodules — none of those need packages.
#
# ⚠ The last two names are the WSI LAYER's, and x11-xcb's absence is why this leg failed on every
# Deck from 2026-08-13 (3ac4548c turned `-Denable_gamescope_wsi_layer=true` on) until it was
# noticed as "HDR stopped working after an update". It does NOT fail the compositor build — it
# fails layer/meson.build, and build-punktfunk-gamescope.sh treats a missing layer as a hard
# error, so the whole build exits non-zero. ci/gamescope-trixie.Dockerfile walked into the
# identical trap one release later (1b28a7f7, v0.28.1) and now asserts x11-xcb at image build;
# this list never got the same fix.
#
# ⚠ Do NOT "sync this list with the CI image". That one is for a .deb that RUNS on Debian; this
# one builds in trixie for a binary that must run on SteamOS. Taking libdisplay-info-dev from it
# (tried on the lab VM, 2026-08-23) built, installed and printed its +pfhdr banner in the box —
# then died on glass with `libdisplay-info.so.2: cannot open shared object file`, because meson
# had preferred the system lib over gamescope's vendored submodule and linked it SHARED. The
# durable fix is the force_fallback_for pin in build-punktfunk-gamescope.sh, next to wlroots;
# the package has no reason to be here. Only add a name whose soname SteamOS itself ships.
# bison and libexpat1-dev only build the bundled xkbcommon and wayland-scanner; neither is linked.
if ! distrobox enter "$BOX" -- bash -lc '
set -e
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq --no-install-recommends \
    meson ninja-build cmake glslang-tools \
    libwayland-dev wayland-protocols libxkbcommon-dev \
    libx11-dev libxcomposite-dev libxdamage-dev libxext-dev libxmu-dev \
    libxrender-dev libxres-dev libxtst-dev libxxf86vm-dev libxcursor-dev \
    libxcb-errors-dev libxcb-icccm4-dev libxcb-ewmh-dev \
    libxcb1-dev libxcb-composite0-dev libxcb-render0-dev libxcb-xfixes0-dev \
    libxcb-shm0-dev libxcb-res0-dev libxcb-present-dev libxcb-xinput-dev \
    libxcb-randr0-dev libxcb-dri3-dev libxcb-shape0-dev \
    libcap-dev libdrm-dev libinput-dev libudev-dev libseat-dev \
    libvulkan-dev libglm-dev libpixman-1-dev libeis-dev \
    libavif-dev libdecor-0-dev hwdata libluajit-5.1-dev \
    libpipewire-0.3-dev libspa-0.2-dev libsdl2-dev \
    xwayland liblcms2-dev bison libexpat1-dev \
    libx11-xcb-dev libxkbcommon-x11-dev >/dev/null
' ; then
    warn "could not provision gamescope build deps in '$BOX' — sessions stay SDR (re-run update.sh to retry)"
    exit 0
fi
if ! distrobox enter "$BOX" -- bash -lc "
set -e
bash '$PKGDIR/build-punktfunk-gamescope.sh' --prefix \"\$HOME/.local\" --no-setcap
"; then
    # A failed build REPLACED NOTHING — the previously installed binary is untouched on disk. If it
    # still passes the on-glass check it is the very binary that was streaming HDR before this run,
    # so keep it wired and say it is stale. Unwiring here took HDR away from boxes whose compositor
    # still worked, on an update that changed nothing about it (field report: HDR "lost" going to
    # 0.31.2, host.env silently missing PUNKTFUNK_GAMESCOPE_BIN afterwards). `unwire` belongs only
    # where the binary itself fails `verifies` — the else branch at the bottom, which also removes
    # it. `wire` re-arms a box a previous run of this bug already unwired.
    if verifies; then
        warn "punktfunk-gamescope failed to build — keeping the installed $("$GS_BIN" --version 2>&1 | head -1) (stale; re-run update.sh to retry)"
        wire
    else
        warn "punktfunk-gamescope failed to build and none is installed — sessions stay SDR (re-run update.sh to retry)"
        unwire
    fi
    exit 0
fi

if verifies; then
    # CAP_SYS_NICE is gamescope's scheduling boost — worth it, never required. Filesystem caps
    # can't be set from the rootless box, so it happens here on the SteamOS side.
    if sudo -n true 2>/dev/null; then
        sudo setcap 'CAP_SYS_NICE=eip' "$GS_BIN" 2>/dev/null \
            && ok "setcap CAP_SYS_NICE (frame-pacing boost)" \
            || warn "could not setcap CAP_SYS_NICE (gamescope still works, pacing slightly worse)"
    fi
    mkdir -p "$(dirname "$STAMP")"
    printf '%s' "$want_stamp" > "$STAMP"
    wire
    ok "punktfunk-gamescope ready: $("$GS_BIN" --version 2>&1 | head -1)"
else
    # Built in the box but does not run on SteamOS (an ABI/soname seam) or lacks the marker. A
    # broken binary must not linger where PATH or host.env could resolve it.
    warn "built binary failed its on-glass check on SteamOS — removing it; sessions stay SDR"
    "$GS_BIN" --version 2>&1 | head -3 >&2 || true
    rm -f "$GS_BIN" "$STAMP"
    unwire
fi
exit 0
