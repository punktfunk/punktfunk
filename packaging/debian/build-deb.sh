#!/usr/bin/env bash
# Build a punktfunk-host .deb for Ubuntu/Debian hosts.
#
# Mirrors the Fedora RPM (../rpm/punktfunk.spec): the host binary + the uinput udev rule
# + the systemd *user* unit + headless session helpers + example config + the OpenAPI doc.
#
# Runtime Depends are computed by `dpkg-shlibdeps` from the binaries' actual DT_NEEDED, NOT
# hand-listed: the exact soname package names (libpipewire-0.3-0t64, …) drift across distro
# releases — shlibdeps tracks them automatically and pins them to whatever the BUILD distro
# ships. Build this inside the Ubuntu 26.04 rust-ci image so those names match the target
# boxes exactly. `--ignore-missing-info` drops libcuda.so.1 (the NVIDIA driver lib, linked via
# FFI): on a GPU-less builder it resolves to no package, and we must never hard-depend on a
# specific libnvidia-compute-<ver> anyway — NVENC/EGL come from the driver, out of band.
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [ARCH=amd64] bash packaging/debian/build-deb.sh
# Output: dist/punktfunk-host_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
ARCH="${ARCH:-amd64}"
PKG="punktfunk-host"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

BIN="target/release/$PKG"
if [ ! -x "$BIN" ]; then
  echo "==> building $PKG (release)"
  PUNKTFUNK_BUILD_VERSION="$VERSION" cargo build --release -p "$PKG" --locked   # stamp --version (build.rs)
fi
# The PyroWave encode worker — the capability-carrying half. A SEPARATE executable, never a
# hardlink or a host subcommand: a shared inode would share the file capability and make the host
# unidentifiable to KWin all over again (see the postinst note below). It ships in this same .deb
# because host and worker version-check each other over their socket and fall back to the
# in-process encoder on any mismatch, so they must move in lockstep.
WORKER_BIN="target/release/punktfunk-encode-worker"
if [ ! -x "$WORKER_BIN" ]; then
  echo "==> building punktfunk-encode-worker (release)"
  PUNKTFUNK_BUILD_VERSION="$VERSION" cargo build --release -p punktfunk-encode-worker --locked
fi
TRAY_BIN="target/release/punktfunk-tray"
# ALWAYS built here, in its OWN cargo invocation — load-bearing, not tidiness, and deliberately not
# skipped when the artifact already exists. Cargo unifies features across everything in one build,
# so a caller that co-built the tray with the host (the .deb workflow used to) leaves behind a
# binary whose zbus took the host's ashpd -> zbus/tokio while the tray runs ksni's async-io
# executor with no tokio runtime by design — it then panics at every launch with "there is no
# reactor running, must be called from the context of a Tokio 1.x runtime". Skipping the rebuild is
# exactly how that binary shipped. Building it alone keeps its zbus on async-io; cargo no-ops this
# when the existing artifact was already resolved that way, and rebuilds it when it wasn't.
echo "==> building punktfunk-tray (release, own invocation — see comment above)"
cargo build --release -p punktfunk-tray --locked
# The web-console-update root helper (dep-free; see crates/pf-update).
echo "==> building pf-update (release)"
cargo build --release -p pf-update --locked

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"
SHAREDIR="$STAGE/usr/share/$PKG"

# --- file layout (matches the RPM %install) ----------------------------------
install -Dm0755 "$BIN"                              "$STAGE/usr/bin/$PKG"
# Next to the host in the SAME bindir — the host resolves the worker as a sibling of
# /proc/self/exe. postinst grants this one (and only this one) cap_sys_nice=ep.
install -Dm0755 "$WORKER_BIN"                       "$STAGE/usr/bin/punktfunk-encode-worker"
# Web-console-triggered updates (host-update-from-web-console.md §7): root helper + its
# oneshot unit + the polkit rule scoping `systemctl start punktfunk-update.service` to the
# (shipped-empty) punktfunk-update group. Opt-in = joining the group; postinst creates it.
install -Dm0755 target/release/pf-update           "$STAGE/usr/libexec/punktfunk/pf-update"
install -Dm0644 packaging/linux/punktfunk-update.service \
                                                   "$STAGE/usr/lib/systemd/system/punktfunk-update.service"
install -Dm0644 packaging/linux/49-punktfunk-update.rules \
                                                   "$STAGE/usr/share/polkit-1/rules.d/49-punktfunk-update.rules"
# postinst runs this on configure and on the file trigger below (web, runner, bun).
install -Dm0755 packaging/linux/restart-user-units.sh "$STAGE/usr/libexec/punktfunk/restart-user-units"
install -Dm0644 scripts/60-punktfunk.rules         "$STAGE/usr/lib/udev/rules.d/60-punktfunk.rules"
install -Dm0644 scripts/60-punktfunk-dualsense.conf "$STAGE/usr/share/wireplumber/wireplumber.conf.d/60-punktfunk-dualsense.conf"
# ALSA UCM for the DualSense's own sound card — the `SpeakerHaptic` device alsa-ucm-conf has
# never carried. Without it the card's only playback route is a 1-channel `Speaker` split and a
# game that opens GE-Proton's "Sony controller speaker" endpoint overruns it. The conf.d files
# are keyed by USB vid:pid and only redefine which profile the pad resolves to, so this REPLACES
# NOTHING alsa-ucm-conf owns — no diversion, no Conflicts. Complements the WirePlumber rules
# above rather than overlapping them: those hold the device open and keep it off the graph-driver
# election, this decides which sinks the card offers in the first place. See scripts/alsa-ucm2/.
for f in USB-Audio/conf.d/054c-0ce6.conf USB-Audio/conf.d/054c-0df2.conf \
         USB-Audio/Punktfunk/DualSense-PS5-Haptic.conf \
         USB-Audio/Punktfunk/DualSense-PS5-Haptic-HiFi.conf; do
  install -Dm0644 "scripts/alsa-ucm2/$f" "$STAGE/usr/share/alsa/ucm2/$f"
done
# Managed gamescope takeover on DM-autologin boxes: root helper + polkit action so the host can
# stop/restore the display manager for the stream (the helper derives the DM unit itself).
install -Dm0755 scripts/pf-dm-helper               "$STAGE/usr/libexec/punktfunk/pf-dm-helper"
install -Dm0644 scripts/io.unom.punktfunk.dm-helper.policy \
                                                   "$STAGE/usr/share/polkit-1/actions/io.unom.punktfunk.dm-helper.policy"
# ...and the other half of stopping one: with the DM stopped the box has no active local session,
# so logind's power actions fall to auth_admin_keep and Steam's power menu goes quiet mid-stream.
install -Dm0644 packaging/linux/49-punktfunk-power.rules \
                                                   "$STAGE/usr/share/polkit-1/rules.d/49-punktfunk-power.rules"
# vhci-hcd autoload — usbip transport for the virtual Steam Deck pad (Steam only adopts USB pads).
install -Dm0644 scripts/punktfunk-modules.conf     "$STAGE/usr/lib/modules-load.d/punktfunk.conf"
# UDP socket-buffer tuning (32 MB) — without it the kernel clamps the host's SO_SNDBUF to ~416 KB
# and high-bitrate frames overflow it (send-side packet loss). systemd-sysctl applies it at boot.
install -Dm0644 scripts/99-punktfunk-net.conf      "$STAGE/usr/lib/sysctl.d/99-punktfunk-net.conf"
# Nice-limit headroom for the host's data-plane threads: raises the user-session RLIMIT_NICE so
# pf-frame's setpriority() works on boxes without RealtimeKit (with rtkit the host never needs
# it). A limit, not a grant, and never a file capability on the host binary (KWin identification).
install -Dm0644 packaging/linux/50-punktfunk-nice.conf \
                                                   "$STAGE/usr/lib/systemd/system/user@.service.d/50-punktfunk-nice.conf"
install -Dm0644 scripts/punktfunk-host.service     "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# The source unit's ExecStart points at the dev source tree; a packaged install has the binary at
# /usr/bin. Rewrite it so a fresh apt install (no hand-rolled unit) starts the installed binary.
sed -i 's#%h/punktfunk/target/release/punktfunk-host#/usr/bin/punktfunk-host#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# Optional drop-in for a DESKTOP-LOGIN host: binds the host to graphical-session.target so a
# Plasma/GNOME restart restarts it instead of leaving it on a dead compositor connection. Shipped
# under /usr/share (NOT as an active drop-in) because it is wrong for the appliance route — the
# operator copies it into ~/.config/systemd/user/punktfunk-host.service.d/ when they want it.
install -Dm0644 scripts/punktfunk-host-desktop-session.conf \
    "$STAGE/usr/share/punktfunk-host/punktfunk-host-desktop-session.conf"
# Install-kind + channel marker, read by the host's update-check surface (planning:
# host-update-from-web-console.md §4.1). ONE canonical path across all package formats —
# /usr/share/punktfunk/, not this package's punktfunk-host/ data dir. A canary version
# carries `~ciN`; anything else is stable.
case "$VERSION" in
  *~ci*) _pf_update_channel=canary ;;
  *)     _pf_update_channel=stable ;;
esac
printf 'apt %s\n' "$_pf_update_channel" | \
    install -Dm0644 /dev/stdin "$STAGE/usr/share/punktfunk/install-kind"
# Optional headless KWin session unit (the kwin --virtual appliance), as the RPM/Arch ship.
# Repoint its ExecStart from the dev source tree to the packaged script. NOT enabled by default.
install -Dm0644 scripts/punktfunk-kde-session.service "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"
sed -i 's#%h/punktfunk/scripts/headless/run-headless-kde.sh#/usr/share/punktfunk-host/headless/run-headless-kde.sh#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"

# KWin Desktop-mode authorization: non-launcher .desktop whose X-KDE-Wayland-Interfaces lets the
# host bind KWin's restricted zkde_screencast (virtual output) + fake_input globals on an
# interactive Plasma session. Must ship with the host — KWin caches the per-exe grant on first
# connect, so it has to be present before the host ever connects. See the file's header comment.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
    "$STAGE/usr/share/applications/io.unom.Punktfunk.Host.desktop"
# Status tray: the per-user SNI icon + its XDG autostart entry (self-gating: --autostart exits
# silently for users who don't run a host) + the hicolor status icons it names.
install -Dm0755 "$TRAY_BIN"                        "$STAGE/usr/bin/punktfunk-tray"
install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
    "$STAGE/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop"
for sz in 22x22 48x48; do
  for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
    install -Dm0644 "$png" "$STAGE/usr/share/icons/hicolor/$sz/apps/$(basename "$png")"
  done
done
install -Dm0755 scripts/headless/run-headless-kde.sh   "$SHAREDIR/headless/run-headless-kde.sh"
install -Dm0755 scripts/headless/run-headless-sway.sh  "$SHAREDIR/headless/run-headless-sway.sh"
install -Dm0644 scripts/headless/kde-authorized        "$SHAREDIR/headless/kde-authorized"
install -Dm0644 scripts/headless/punktfunk-sink.conf   "$SHAREDIR/headless/punktfunk-sink.conf"
install -Dm0644 scripts/host.env.example           "$SHAREDIR/host.env.example"
install -Dm0644 packaging/bazzite/host.env         "$SHAREDIR/host.env.bazzite"
install -Dm0644 packaging/kde/host.env             "$SHAREDIR/host.env.kde"
install -Dm0644 api/openapi.json              "$SHAREDIR/openapi.json"
# Firewall openers (shared across all Linux packaging), NOT auto-enabled — the postinst prints the
# enable command for whichever firewall is present. Debian ships none and Ubuntu's ufw is
# installed-but-inactive, so these are a no-op until the admin turns a firewall on.
install -Dm0644 packaging/linux/punktfunk.ufw \
                "$STAGE/etc/ufw/applications.d/punktfunk"
install -Dm0644 packaging/linux/punktfunk-gamestream.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-gamestream.xml"
install -Dm0644 packaging/linux/punktfunk-native.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-native.xml"
# Web console opener (TCP 47992) — only meaningful with the optional punktfunk-web package; opened
# deliberately (see README.md → Firewall). ufw's equivalent is the punktfunk-web profile above.
install -Dm0644 packaging/linux/punktfunk-web.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-web.xml"
install -Dm0644 LICENSE-MIT                         "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                      "$DOCDIR/LICENSE-APACHE"
install -Dm0644 README.md                           "$DOCDIR/README.md"
# Third-party crate attributions (regenerate with scripts/gen-third-party-notices.sh).
if [ -f THIRD-PARTY-NOTICES.txt ]; then
    install -Dm0644 THIRD-PARTY-NOTICES.txt "$DOCDIR/THIRD-PARTY-NOTICES.txt"
fi

# Debian copyright + changelog (cheap, keeps the package well-formed).
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

# --- dependencies ------------------------------------------------------------
# Auto: the binaries' directly-linked shared libs (libcuda ignored, see header). The encode
# worker is scanned alongside the host: its link set is a subset today, but it is a shipped
# executable in this package and a future divergence must show up as a Depends, not as a worker
# that silently fails to exec on a fresh install.
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
cat > "$SHLIB_TMP/debian/control" <<EOF
Source: $PKG

Package: $PKG
Architecture: any
Depends: \${shlibs:Depends}
Recommends: rtkit
EOF
# Stderr is captured so a future resolution failure is visible instead of swallowed.
SHDEPS_RAW="$(
  cd "$SHLIB_TMP"
  dpkg-shlibdeps -O --ignore-missing-info "$ROOTDIR/$BIN" "$ROOTDIR/$WORKER_BIN" 2>"$SHLIB_TMP/err" \
    | sed -n 's/^shlibs:Depends=//p'
)" || { echo "dpkg-shlibdeps failed (exit $?):" >&2; sed 's/^/  /' "$SHLIB_TMP/err" >&2; rm -rf "$SHLIB_TMP"; exit 1; }
rm -rf "$SHLIB_TMP"
[ -n "$SHDEPS_RAW" ] || { echo "dpkg-shlibdeps produced no deps — is dpkg-dev installed?" >&2; exit 1; }

# Drop the NVIDIA driver lib unconditionally. --ignore-missing-info already skips libcuda on a
# GPU-less builder (stub, no owning package), but on a box WITH the driver shlibdeps resolves
# libcuda.so.1 -> libnvidia-compute-<ver> and would pin that exact driver build. NVENC/EGL are
# provided by whatever driver the host runs, so this must never be a package dependency.
FILTER='^(libnvidia-compute|libcuda)'
SHDEPS="$(printf '%s' "$SHDEPS_RAW" | tr ',' '\n' | sed 's/^ *//; s/ *$//' \
          | grep -ivE "$FILTER" | awk 'NF' | paste -sd ',' - | sed 's/,/, /g')"
[ -n "$SHDEPS" ] || { echo "no deps left after filtering — unexpected" >&2; exit 1; }

# Manual additions shlibdeps can't see:
#  - libei1: input injection (libei) is loaded at runtime, not in DT_NEEDED.
#  - pipewire/wireplumber: runtime services (the daemon + session manager), not linked libs.
DEPENDS="$SHDEPS, libei1, pipewire, wireplumber"
# gamescope = a ready compositor backend; pipewire-pulse = desktop audio.
# mesa-va-drivers / intel-media-va-driver = the VAAPI encode drivers for AMD (radeonsi) and Intel
# (iHD) — pulled by default so the auto-selected VAAPI backend works out of the box; NVIDIA boxes
# don't need them (NVENC comes from the driver) and can --no-install-recommends.
# punktfunk-web = the management web console (pairing + status) every user needs — a separate
# Architecture:all .deb; Recommends so `apt install punktfunk-host` pulls it by default, while a
# headless/encoding-only box can opt out with --no-install-recommends.
# punktfunk-scripting = the plugin/script runner (host automation on bun). Recommends so it's pulled
# by default; its systemd --user unit ships disabled (inert until you add scripts/plugins).
RECOMMENDS="gamescope, pipewire-pulse, mesa-va-drivers, intel-media-va-driver, punktfunk-web, punktfunk-scripting"
SUGGESTS="kwin-wayland, mutter"

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
Depends: $DEPENDS
Recommends: $RECOMMENDS
Suggests: $SUGGESTS
Description: Low-latency desktop/game streaming host (Moonlight + punktfunk/1)
 punktfunk is a Linux-first, low-latency desktop and game streaming host. It speaks
 the Moonlight/GameStream protocol (pair a stock Moonlight client) and its own native
 punktfunk/1 protocol (GF(2^16) Leopard FEC + AES-GCM, mid-stream mode renegotiation,
 client microphone passthrough). Each session gets a virtual output at the client's
 exact resolution and refresh via a per-compositor backend (KWin, gamescope, Mutter,
 Sway/wlroots), captured zero-copy (dmabuf -> CUDA -> NVENC). Input (mouse, keyboard,
 gamepads) is injected back into the session.
 .
 NVENC + GPU EGL come from the NVIDIA driver (libnvidia-encode / libEGL_nvidia),
 installed out of band. After install: add yourself to the 'input' group for virtual
 gamepads, then enable the systemd user service punktfunk-host.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # The (empty) opt-in group for web-console-triggered updates — nobody is auto-added.
    getent group punktfunk-update >/dev/null 2>&1 || addgroup --system punktfunk-update 2>/dev/null || true
    # Owns the usbip vhci attach/detach nodes (60-punktfunk.rules). Deliberately NOT 'input':
    # writing 'attach' materialises an arbitrary emulated USB device — a root-only kernel
    # primitive that must not ride on the group users are told to join for gamepads
    # (security-review 2026-08-05 M-4). It is ALSO the group pf-dm-helper authorizes on (its
    # polkit action must stay allow_any, so membership is the real gate), i.e. what a managed
    # gamescope takeover needs to stop the display manager. Creating the group is necessary and
    # NOT sufficient for either use: membership is.
    getent group punktfunk >/dev/null 2>&1 || addgroup --system punktfunk 2>/dev/null || true
    # NO capability on the host binary — and an active removal of the one 0.26.0-1 granted here.
    #
    # 0.26.0-1 ran `setcap cap_sys_nice=ep` at this point for the GPU-priority lever, and that broke
    # desktop streaming on every KDE box. KWin advertises its restricted protocols
    # (zkde_screencast_unstable_v1 for the virtual output, org_kde_kwin_fake_input for input) only
    # to a client it can IDENTIFY, by resolving that client's /proc/<pid>/exe and matching it
    # against an installed .desktop's Exec=. The kernel refuses that readlink to any reader whose
    # effective set is not a superset of the target's PERMITTED set (cap_ptrace_access_check), and
    # KWin holds no capabilities — so a capability here makes the host unidentifiable and the
    # session dies with "KWin does not expose zkde_screencast_unstable_v1 to this client". Full
    # matrix (and why PR_SET_DUMPABLE and AmbientCapabilities= both fail to rescue it) in
    # packaging/arch/punktfunk-host.install.
    #
    # Costs pacing only: pf-zerocopy walks REALTIME -> HIGH -> default when a class is refused.
    # postinst runs on upgrade too, so this heals boxes that installed 0.26.0-1. `setcap -r` exits
    # non-zero on a file that has no capability, hence the redirect and `|| true`.
    setcap -r /usr/bin/punktfunk-host 2>/dev/null || true
    # CAP_SYS_NICE on the ENCODE WORKER — the same grant, on the binary that can carry it.
    #
    # punktfunk-encode-worker is a SEPARATE executable (never a hardlink or a host subcommand: a
    # shared inode shares the capability and re-creates the breakage above). It is spawned per
    # PyroWave session, speaks one socketpair to its parent, and never connects to Wayland, D-Bus
    # or the network — so nothing ever resolves ITS /proc/<pid>/exe and the KWin identification
    # path above stays clear.
    #
    # Why it is worth a capability at all: PyroWave encodes on the GPU shader cores the game
    # saturates, and an elevated VK_KHR_global_priority queue is the preemption lever. Every driver
    # tested (NVIDIA and RADV) refuses EVERY class without CAP_SYS_NICE. Measured on an RTX 5070
    # Ti under load: encode p99 6.4 -> 4.4 ms. Narrow — scheduling priority only, no filesystem,
    # network or user-switching privilege, not setuid.
    #
    # Best-effort, always: an uncapped worker still encodes at default priority, so a box without
    # libcap or a filesystem that cannot store capabilities must not fail this install. postinst
    # runs on upgrade too, which is what re-applies the grant to the replaced (new-inode) file.
    #
    # Debugging the WORKER: a capability makes it AT_SECURE — the loader ignores LD_LIBRARY_PATH
    # and LD_PRELOAD for it, and core dumps are suppressed.
    if [ -x /usr/bin/punktfunk-encode-worker ]; then
        setcap 'cap_sys_nice=ep' /usr/bin/punktfunk-encode-worker 2>/dev/null || true
    fi
    # Pick up the /dev/uinput rule without a reboot (best-effort, no-op in containers).
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=misc 2>/dev/null || true
    # Apply the UDP socket-buffer tuning now (also auto-applied at boot by systemd-sysctl).
    sysctl -p /usr/lib/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || true
    echo "punktfunk-host installed. Add yourself to the 'input' group for virtual gamepads:"
    echo "    sudo usermod -aG input \"\$USER\"   # then re-login"
    # Naming only the usbip pad here is how a Nobara host shipped broken: its owner had no Deck
    # pad, so they correctly skipped this group — and then every managed gamescope takeover
    # degraded silently, because pf-dm-helper (which stops the display manager) gates on membership.
    echo "ALSO join 'punktfunk' if this box streams Steam Gaming Mode (gamescope) or you want the"
    echo "virtual Steam Deck pad: sudo usermod -aG punktfunk \"\$USER\"   # then log out and back in"
    echo "  — it authorizes stopping the display manager for a managed gamescope session, and the"
    echo "    pad's usbip nodes; it can emulate arbitrary USB devices, so join it only on a box you trust."
    echo "Config:  mkdir -p ~/.config/punktfunk && cp /usr/share/punktfunk-host/host.env.example ~/.config/punktfunk/host.env"
    echo "Enable:  systemctl --user enable --now punktfunk-host"
    # Debian ships no active firewall and Ubuntu's ufw is inactive by default; hint whichever is present.
    if command -v ufw >/dev/null 2>&1; then
        echo "Firewall (ufw detected): sudo ufw allow punktfunk-native   (or punktfunk-gamestream for Moonlight)"
    fi
    if command -v firewall-cmd >/dev/null 2>&1; then
        echo "Firewall (firewalld detected): sudo firewall-cmd --reload &&"
        echo "    sudo firewall-cmd --permanent --add-service=punktfunk-native && sudo firewall-cmd --reload"
        echo "    (use punktfunk-gamestream for the Moonlight-compat host)"
    fi
    # An ALREADY-OPEN firewall does not pick up a port we later added to a profile. ufw expands an
    # app profile into concrete rules at `ufw allow` time and keeps those, so editing
    # /etc/ufw/applications.d on upgrade changes nothing; firewalld re-reads its XML, but only on a
    # reload. 47993 (the separate origin plugin UIs are served from) arrived exactly this way, and
    # an unrefreshed rule turns every plugin interface in the console into an empty panel.
    # `ufw status verbose` prints expanded ports, so it can tell "allowed" from "allowed, stale".
    if command -v ufw >/dev/null 2>&1 &&
       ufw status verbose 2>/dev/null | grep -q 'punktfunk-web' &&
       ! ufw status verbose 2>/dev/null | grep -q '47993'; then
        echo ""
        echo "punktfunk: your ufw rule for 'punktfunk-web' predates TCP 47993 (plugin UIs, served"
        echo "  from their own origin). Plugin interfaces will not load in the console until:"
        echo "    sudo ufw app update punktfunk-web && sudo ufw reload"
    fi
    # --info-service answers from the definition the daemon loaded, i.e. the stale one.
    if command -v firewall-cmd >/dev/null 2>&1 &&
       firewall-cmd --state >/dev/null 2>&1 &&
       firewall-cmd --query-service=punktfunk-web >/dev/null 2>&1 &&
       ! firewall-cmd --info-service=punktfunk-web 2>/dev/null | grep -q '47993'; then
        echo ""
        echo "punktfunk: the punktfunk-web firewalld service now also covers TCP 47993 (plugin UIs)."
        echo "  Plugin interfaces will not load in the console until:  sudo firewall-cmd --reload"
    fi
    # Conflicting Moonlight-compatible host (Sunshine/Apollo/...): reuse the host's own detector so
    # the warning lives in one place. Exit 1 = found; never fail the install on it.
    if command -v punktfunk-host >/dev/null 2>&1; then
        if ! conflict="$(punktfunk-host detect-conflicts 2>/dev/null)"; then
            echo ""
            echo "$conflict"
        fi
    fi
fi
# Restart the running services. configure restarts all three: dpkg may fold a pending trigger into it.
case "$1" in configure|triggered) /usr/libexec/punktfunk/restart-user-units ;; esac
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"
cat > "$STAGE/DEBIAN/triggers" <<'EOF'
interest-noawait /usr/share/punktfunk-web
interest-noawait /usr/share/punktfunk-scripting
interest-noawait /usr/lib/punktfunk-bun
EOF

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
echo "  Depends: $DEPENDS"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size' || true
