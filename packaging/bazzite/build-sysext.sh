#!/usr/bin/env bash
# Build the punktfunk systemd-sysext image for Bazzite / Fedora Atomic from the built RPMs —
# the no-layering install path (rpm-ostree layering slows every update and can block upgrades;
# a sysext never enters an rpm-ostree transaction). The .raw overlays /usr read-only from
# /var/lib/extensions/, survives OS updates, and is toggled/updated without a reboot.
#
# Counterpart to ../arch/build-sysext.sh (which wraps a pacman package for SteamOS). This one
# wraps the Fedora RPMs (punktfunk, -web, -scripting, -seats, -bun) and additionally:
#   * relocates the RPMs' /etc payload to /usr/share/punktfunk/etc/ (a sysext carries ONLY /usr;
#     punktfunk-sysext(8) copies these into the real /etc on install),
#   * bakes SELinux labels in as squashfs pseudo-xattrs, computed with matchpathcon from the
#     build container's targeted policy. Without them every file is unlabeled_t at runtime:
#     fine for the user session + systemd --user units (unconfined), but system daemons are
#     DENIED — udev couldn't read 60-punktfunk.rules and systemd-sysctl couldn't read the
#     sysctl drop-in (validated live on Bazzite 43, SELinux enforcing, 2026-07-04),
#   * pins compatibility via ID=fedora + VERSION_ID: merges on Bazzite/Silverblue/Aurora of the
#     SAME Fedora major (ID_LIKE matching, systemd >= 256) and is REFUSED after a major rebase
#     instead of running soname-broken binaries (`punktfunk-sysext update` then re-resolves),
#   * embeds the punktfunk-sysext helper so an installed box can update itself.
#
# Build in the matching Fedora container (ci/fedora*-rpm.Dockerfile) — matchpathcon needs the
# Fedora targeted policy (libselinux-utils + selinux-policy-targeted), and the RPMs are
# soname-coupled to their base anyway. Needs: rpm2cpio, cpio, mksquashfs (>= 4.6), matchpathcon.
#
# Usage:
#   bash build-sysext.sh --version-id 43 --out dist/punktfunk-0.7.1-1-x86-64.raw \
#        [--gamescope-stage path/to/gamescope-destdir] \
#        dist/punktfunk-0.7.1-1.fc43.x86_64.rpm dist/punktfunk-web-0.7.1-1.fc43.x86_64.rpm \
#        dist/punktfunk-bun-0.7.1-1.fc43.x86_64.rpm
#
# --gamescope-stage folds in a prebuilt HDR-capable gamescope (packaging/gamescope) as
# /usr/bin/punktfunk-gamescope, which is what lets the gamescope backend stream 10-bit BT.2020 PQ.
# It is NOT built here: it is a C++ meson build with gamescope's whole dependency set, so CI builds
# it in the same Fedora container beforehand (`bash packaging/gamescope/build-punktfunk-gamescope.sh
# --destdir stage --prefix /usr`) and passes that DESTDIR in. Omit it and the image is exactly what
# it was — the host then stays SDR on that backend, by design.
#
# A directory rather than the binary, because the tree also carries the Vulkan WSI layer built beside
# the compositor. That layer is the only route to an HDR10 swapchain for a game nested under
# gamescope, so an image with the compositor and without it would stream HDR while every game in it
# rendered SDR.
#
# The installed image MUST be named punktfunk.raw (the embedded extension-release marker is
# extension-release.punktfunk; systemd-sysext requires marker == image name) — the feed carries
# versioned filenames and punktfunk-sysext installs to the fixed name.
set -euo pipefail

VERSION_ID="" OUT="" GAMESCOPE="" RPMS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version-id) VERSION_ID="${2:?}"; shift 2 ;;
    --out)        OUT="${2:?}"; shift 2 ;;
    --gamescope-stage) GAMESCOPE="${2:?}"; shift 2 ;;
    *)            RPMS+=("$1"); shift ;;
  esac
done
[ -n "$VERSION_ID" ] || { echo "missing --version-id <fedora major, e.g. 43>" >&2; exit 1; }
[ -n "$OUT" ] || { echo "missing --out <image.raw>" >&2; exit 1; }
[ "${#RPMS[@]}" -gt 0 ] || { echo "no RPMs given" >&2; exit 1; }
for tool in rpm2cpio cpio mksquashfs matchpathcon; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../linux/sysext-lib.sh"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# SYSEXT_VERSION_ID from the punktfunk RPM (V-R without the dist tag): what
# `punktfunk-sysext status` reports as the installed version.
PF_VR=""
SEEN_NAMES=" "
for rpm in "${RPMS[@]}"; do
  [ -f "$rpm" ] || { echo "no such RPM: $rpm" >&2; exit 1; }
  name="$(rpm -qp --qf '%{NAME}' "$rpm" 2>/dev/null)"
  # Two RPMs of the same NAME (e.g. a stale noarch next to the current x86_64 from a sloppy
  # download glob) silently shadow each other's files — refuse instead of building a chimera.
  case "$SEEN_NAMES" in *" $name "*) echo "duplicate RPM name '$name' in inputs — pass exactly one RPM per package" >&2; exit 1 ;; esac
  SEEN_NAMES="$SEEN_NAMES$name "
  if [ "$name" = punktfunk ]; then
    PF_VR="$(rpm -qp --qf '%{VERSION}-%{RELEASE}' "$rpm" 2>/dev/null)"
    PF_VR="${PF_VR%.fc*}"
  fi
  rpm2cpio "$rpm" | ( cd "$STAGE" && cpio -idmu --quiet )
done
[ -n "$PF_VR" ] || { echo "the punktfunk (host) RPM must be among the inputs" >&2; exit 1; }
# An image resolves no dependencies, and both launchers exec punktfunk-bun's bun.
if [ -e "$STAGE/usr/bin/punktfunk-web-server" ] || [ -e "$STAGE/usr/bin/punktfunk-scripting" ]; then
  [ -x "$STAGE/usr/libexec/punktfunk-bun/bun" ] || {
    echo "the punktfunk-bun RPM must be among the inputs — web and scripting run on it" >&2; exit 1; }
fi

# A sysext carries only /usr. Relocate the RPMs' /etc payload (gamescope-session drop-in, tray
# autostart entry) under /usr/share/punktfunk/etc/ — punktfunk-sysext copies it into /etc.
if [ -d "$STAGE/etc" ]; then
  mkdir -p "$STAGE/usr/share/punktfunk/etc"
  cp -a "$STAGE/etc/." "$STAGE/usr/share/punktfunk/etc/"
  rm -rf "${STAGE:?}/etc"
fi
rm -rf "${STAGE:?}/var"   # rpm ghosts etc. — nothing outside /usr may remain

# The HDR-capable gamescope, when one was built (see --gamescope-stage in the header), verified by
# its banner and its WSI layer rather than trusted by filename.
if [ -n "$GAMESCOPE" ]; then
  GS_BIN="$GAMESCOPE/usr/bin/punktfunk-gamescope"
  GS_LAYER_SO="$GAMESCOPE/usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
  GS_LAYER_JSON="$GAMESCOPE/usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
  pf_verify_gamescope "$GAMESCOPE" "the gamescope stage $GAMESCOPE"
  install -Dm0755 "$GS_BIN" "$STAGE/usr/bin/punktfunk-gamescope"
  install -Dm0755 "$GS_LAYER_SO" \
    "$STAGE/usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
  install -Dm0644 "$GS_LAYER_JSON" \
    "$STAGE/usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
fi

# Enable the plugin/script runner for every user, by baking its `[Install] WantedBy=default.target`
# symlink straight into the image.
#
# A sysext carries only /usr, and RPM scriptlets never run from one — so the `systemctl --global
# enable` the .rpm/.deb do at install time has no equivalent here, and without this the runner would
# ship present-but-off on exactly the platform (Bazzite / Fedora Atomic) where an operator is least
# likely to go hunting for it. The game-library scanners are plugins now (design D9), so an
# unenabled runner means an empty library.
#
# Opt-out is unchanged and still wins: `systemctl --user mask punktfunk-scripting` in the user's own
# ~/.config/systemd/user takes precedence over anything under /usr.
if [ -f "$STAGE/usr/lib/systemd/user/punktfunk-scripting.service" ]; then
  install -d "$STAGE/usr/lib/systemd/user/default.target.wants"
  ln -sf ../punktfunk-scripting.service \
    "$STAGE/usr/lib/systemd/user/default.target.wants/punktfunk-scripting.service"
fi

# Self-update: the helper rides inside the image.
install -Dm0755 "$HERE/punktfunk-sysext.sh" "$STAGE/usr/bin/punktfunk-sysext"

# Compatibility marker. ID=fedora matches Bazzite & friends through os-release ID_LIKE;
# VERSION_ID makes a major-rebased host refuse the old ABI instead of merging it.
install -d "$STAGE/usr/lib/extension-release.d"
cat > "$STAGE/usr/lib/extension-release.d/extension-release.punktfunk" <<EOF
ID=fedora
VERSION_ID=$VERSION_ID
ARCHITECTURE=x86-64
SYSEXT_ID=punktfunk
SYSEXT_VERSION_ID=$PF_VR
EXTENSION_RELOAD_MANAGER=1
EOF

# CAP_SYS_NICE on the encode worker, never the host, then both halves of the matrix
# (packaging/linux/sysext-lib.sh). The spec's %caps cannot reach the image: rpm keeps capabilities
# in its header and `rpm2cpio | cpio` carries only the payload. mksquashfs keeps security.capability
# (only security.selinux is excluded below). A plain-user build ships the worker uncapped.
pf_seal_caps "$STAGE" "rpm keeps capabilities in its own header and 'rpm2cpio | cpio' carries only the payload."

# SELinux labels as pseudo-xattrs (see header). matchpathcon resolves each target path against
# the targeted policy's file_contexts; <<none>> means "no specific entry" — skip those (the
# handful of matches all resolve to real contexts for our payload).
PSEUDO="$STAGE.pseudo"
( cd "$STAGE" && find . -mindepth 1 \( -type f -o -type d \) -printf '/%P\n' ) | sort \
  | while IFS= read -r path; do
      ctx="$(matchpathcon -n "$path" 2>/dev/null || true)"
      case "$ctx" in ''|'<<none>>') continue ;; esac
      printf '%s x security.selinux=%s\n' "$path" "$ctx"
    done > "$PSEUDO"
[ -s "$PSEUDO" ] || { echo "matchpathcon produced no labels — refusing to build an unlabeled image" >&2; exit 1; }

rm -f "$OUT"; mkdir -p "$(dirname "$OUT")"
# -xattrs-exclude drops any security.selinux the staging fs already had (would collide with the
# pseudo defs when building on an SELinux host); -all-root because cpio extracted as the CI uid.
mksquashfs "$STAGE" "$OUT" -all-root -noappend -quiet \
  -xattrs-exclude '^security.selinux' -pf "$PSEUDO"
rm -f "$PSEUDO"
echo "built $OUT (punktfunk $PF_VR, fedora $VERSION_ID, $(du -h "$OUT" | cut -f1))"
echo "  install on the box:  punktfunk-sysext install   (or --from-file $OUT)"
