#!/usr/bin/env bash
# Build the punktfunk-client .deb (the native GTK4 client) for Ubuntu/Debian desktops.
#
# Counterpart to build-deb.sh (the host package); same conventions: runtime Depends are
# computed by dpkg-shlibdeps from the binaries' DT_NEEDED (GTK4/libadwaita, SDL3, the
# PipeWire/Opus sonames), so build inside the Ubuntu 26.04 rust-ci image to pin
# the package names the target boxes ship. The client links no NVIDIA libs — no filter
# needed.
#
# The client decodes with pf-vkdecode / pf-vaapi (libva is dlopen'd, never linked) /
# openh264+rav1d, so neither binary carries a codec-library DT_NEEDED and shlibdeps emits
# no soname the target box must match. There is no list here to prune, which is exactly the
# property to keep.
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [ARCH=amd64] [TARGET=<rust triple>] \
#          bash packaging/debian/build-client-deb.sh
# Output: dist/punktfunk-client_<version>_<arch>.deb
#
# TARGET cross-compiles (and moves the binaries under target/<triple>/release). Set it
# together with ARCH — e.g. ARCH=arm64 TARGET=aarch64-unknown-linux-gnu inside the
# ci/rust-ci-arm64cross.Dockerfile image, which carries the matching :arm64 sysroot.
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
ARCH="${ARCH:-amd64}"
TARGET="${TARGET:-}"
PKG="punktfunk-client"
CRATE="punktfunk-client-linux"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Cargo drops a cross build under target/<triple>/release, a native one straight in
# target/release.
if [ -n "$TARGET" ]; then
  OUTDIR="target/$TARGET/release"
  CARGO_TARGET_ARGS=(--target "$TARGET")
else
  OUTDIR="target/release"
  CARGO_TARGET_ARGS=()
fi

BIN="$OUTDIR/$PKG"
# The Vulkan/Skia session streamer the shell execs for a connect — shipped alongside the shell
# (the shell resolves it as its /usr/bin sibling), or desktop streaming breaks.
SESSION_BIN="$OUTDIR/punktfunk-session"
# The headless CLI (design/client-architecture-split.md §4) ships with every client.
CLI_BIN="$OUTDIR/punktfunk"
# pf-update: the client's own copy of the root helper (`punktfunk-client --apply-update`).
UPDATE_BIN="$OUTDIR/pf-update"
# Builds only when a binary is missing, so it tests every binary installed below. A caller with
# a restored target must build first: an existing binary is packaged as-is, however old.
if [ ! -x "$BIN" ] || [ ! -x "$SESSION_BIN" ] || [ ! -x "$CLI_BIN" ] || [ ! -x "$UPDATE_BIN" ]; then
  echo "==> building $CRATE + punktfunk-client-session + punktfunk-cli + pf-update (release${TARGET:+ for $TARGET})"
  cargo build --release --locked "${CARGO_TARGET_ARGS[@]}" -p "$CRATE" -p punktfunk-client-session \
    -p punktfunk-cli -p pf-update
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"

# --- file layout --------------------------------------------------------------
install -Dm0755 "$BIN"                                   "$STAGE/usr/bin/$PKG"
install -Dm0755 "$SESSION_BIN"                           "$STAGE/usr/bin/punktfunk-session"
install -Dm0755 "$CLI_BIN"                               "$STAGE/usr/bin/punktfunk"
install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
                "$STAGE/usr/share/applications/io.unom.Punktfunk.desktop"
# Second launcher, straight into the gamepad console (`--browse`): the couch entry point a
# TV/HTPC user picks from the app grid, and what gets added to Steam as a non-Steam game.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Console.desktop \
                "$STAGE/usr/share/applications/io.unom.Punktfunk.Console.desktop"
# The app icon the desktop entry (and the About dialog) name. Without it the launcher falls
# back to a generic monitor glyph, which is what shipped until now.
install -Dm0644 packaging/linux/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg \
                "$STAGE/usr/share/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg"
# DualSense hidraw access (full pad fidelity through SDL's HIDAPI driver).
install -Dm0644 scripts/70-punktfunk-client.rules \
                "$STAGE/usr/lib/udev/rules.d/70-punktfunk-client.rules"
# UDP receive-buffer tuning (32 MB) — without it the kernel clamps the client's SO_RCVBUF and
# high-bitrate streams overflow it at the receiver (measured: 4 MB cap = 31.6% loss at 2 Gbps,
# 32 MB = 0%). systemd-sysctl applies it at boot; the postinst applies it on install.
install -Dm0644 scripts/99-punktfunk-client-net.conf \
                "$STAGE/usr/lib/sysctl.d/99-punktfunk-client-net.conf"
# One-tap client updates (`punktfunk-client --apply-update`, which is what the Decky plugin
# runs): the same root helper the host package ships, under the CLIENT's own paths + its own
# unit and polkit rule. Separate paths because dpkg refuses two packages owning one file, and a
# client-only box must be able to install this without punktfunk-host. Opt-in = joining the
# (shipped-empty) punktfunk-update group; postinst creates it.
install -Dm0755 "$UPDATE_BIN"                      "$STAGE/usr/libexec/punktfunk/pf-update-client"
install -Dm0644 packaging/linux/punktfunk-client-update.service \
                "$STAGE/usr/lib/systemd/system/punktfunk-client-update.service"
sed -i 's#/usr/libexec/punktfunk/pf-update#/usr/libexec/punktfunk/pf-update-client#' \
       "$STAGE/usr/lib/systemd/system/punktfunk-client-update.service"
install -Dm0644 packaging/linux/49-punktfunk-client-update.rules \
                "$STAGE/usr/share/polkit-1/rules.d/49-punktfunk-client-update.rules"
# Install-kind + channel marker for the CLIENT, read by `punktfunk-client --check-update`
# (planning: host-update-from-web-console.md §4.1). Its own directory, matching the RPM. A
# canary build's version carries `~ciN`; anything else is stable.
case "$VERSION" in
  *~ci*) _pf_client_channel=canary ;;
  *)     _pf_client_channel=stable ;;
esac
printf 'apt %s\n' "$_pf_client_channel" | \
    install -Dm0644 /dev/stdin "$STAGE/usr/share/punktfunk-client/install-kind"
install -Dm0644 LICENSE-MIT                              "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                           "$DOCDIR/LICENSE-APACHE"
install -Dm0644 README.md                                "$DOCDIR/README.md"
# Third-party crate attributions (regenerate with scripts/gen-third-party-notices.sh).
#
# The CLIENT-scoped copy, not the workspace-wide one at the repo root: the root file covers the
# whole workspace, of which this package is a subset. It is the same file the GTK shell shows on
# its About → Legal page, so the
# installed doc and the running app say the same thing. Falls back to the root file only if the
# generated copy is missing (an old checkout), because shipping no attribution at all is worse.
if [ -f clients/linux/THIRD-PARTY-NOTICES.txt ]; then
    install -Dm0644 clients/linux/THIRD-PARTY-NOTICES.txt "$DOCDIR/THIRD-PARTY-NOTICES.txt"
elif [ -f THIRD-PARTY-NOTICES.txt ]; then
    echo "warning: clients/linux/THIRD-PARTY-NOTICES.txt missing — shipping the workspace-wide" \
         "file, which attributes host-only dependencies to the client" >&2
    install -Dm0644 THIRD-PARTY-NOTICES.txt "$DOCDIR/THIRD-PARTY-NOTICES.txt"
fi

cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: punktfunk
Source: https://git.unom.io/unom/punktfunk

Files: *
Copyright: unom and the punktfunk contributors
License: MIT or Apache-2.0
 Dual-licensed. Full texts in /usr/share/doc/$PKG/LICENSE-MIT and
 /usr/share/doc/$PKG/LICENSE-APACHE.
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <packages@unom.io>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

# --- dependencies --------------------------------------------------------------
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
cat > "$SHLIB_TMP/debian/control" <<EOF
Source: $PKG

Package: $PKG
Architecture: any
Depends: \${shlibs:Depends}
EOF
# Resolve DT_NEEDED for BOTH the shell and the session streamer (dpkg-shlibdeps merges).
SHDEPS="$(cd "$SHLIB_TMP" && dpkg-shlibdeps -O --ignore-missing-info \
            "$ROOTDIR/$BIN" "$ROOTDIR/$SESSION_BIN" 2>/dev/null \
          | sed -n 's/^shlibs:Depends=//p')"
rm -rf "$SHLIB_TMP"
[ -n "$SHDEPS" ] || { echo "dpkg-shlibdeps produced no deps — is dpkg-dev installed?" >&2; exit 1; }

# Manual addition shlibdeps can't see: the session streamer loads libvulkan at runtime (ash,
# dlopen — not a DT_NEEDED), and streaming can't start without it, so it's a hard Depends.
SHDEPS="$SHDEPS, libvulkan1"

# Manual additions shlibdeps can't see: the PipeWire daemon + session manager are runtime
# services (audio playback / mic capture degrade gracefully without them — Recommends).
# NOT pipewire-pulse: the client speaks native PipeWire (audio.rs → libpipewire-0.3) and never
# opens a Pulse socket, and the shim Conflicts with pulseaudio — so recommending it only nags
# users who run real PulseAudio, in exchange for nothing the client can use.
RECOMMENDS="pipewire, wireplumber"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: unom <packages@unom.io>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Homepage: https://git.unom.io/unom/punktfunk
Depends: $SHDEPS
Recommends: $RECOMMENDS
Description: Low-latency desktop/game streaming client (punktfunk/1, GTK4)
 The native Linux client for punktfunk, a Linux-first low-latency desktop and
 game streaming stack. Discovers hosts on the LAN (mDNS), trusts them via
 certificate pinning with a SPAKE2 PIN pairing ceremony, and streams HEVC video
 (GF(2^16) Leopard FEC + AES-GCM over UDP, QUIC control plane) with Opus audio,
 microphone passthrough, and full gamepad support including DualSense touchpad,
 motion, adaptive triggers and lightbar through SDL3.
 .
 The host creates a virtual output at exactly this client's resolution and
 refresh rate — no scaling. See the punktfunk-host package for the host side.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # The (empty) opt-in group for one-tap client updates — nobody is auto-added. The host
    # package's postinst creates the same group; addgroup is idempotent, so whichever is
    # configured first wins and the other is a no-op.
    getent group punktfunk-update >/dev/null 2>&1 || addgroup --system punktfunk-update 2>/dev/null || true
    # Pick up the DualSense hidraw rule without a reboot (best-effort, no-op in containers).
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=hidraw 2>/dev/null || true
    update-desktop-database /usr/share/applications 2>/dev/null || true
    # Apply the UDP recv-buffer tuning now (also auto-applied at boot by systemd-sysctl).
    sysctl -p /usr/lib/sysctl.d/99-punktfunk-client-net.conf >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
echo "  Depends: $SHDEPS"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size' || true
