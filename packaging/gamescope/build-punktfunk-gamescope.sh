#!/usr/bin/env bash
# Build gamescope + punktfunk's `pipewire-hdr` patches and install it as `punktfunk-gamescope`.
#
# The ONE build recipe every packaging path calls (Arch PKGBUILD, the Bazzite sysext image, an
# operator building by hand); the nix derivation expresses the same thing declaratively. It never
# touches the distro's own `gamescope` — the binary lands under a different name, and the host's
# resolution order (PUNKTFUNK_GAMESCOPE_BIN > punktfunk-gamescope > gamescope) picks it up.
#
# Usage:
#   bash build-punktfunk-gamescope.sh [--rev <git-rev>] [--prefix /usr] [--destdir DIR]
#                                     [--srcdir DIR] [--jobs N] [--no-setcap]
#
#   --rev       gamescope commit/tag to build (default: the pin below)
#   --prefix    install prefix (default /usr)
#   --destdir   staging root for packaging (default: install straight into --prefix)
#   --srcdir    reuse an existing gamescope checkout instead of cloning
#   --no-setcap skip CAP_SYS_NICE (always skipped when --destdir is set — a package sets it)
#
# Needs: git, meson >= 0.58, ninja, a C++20 compiler, and gamescope's own build deps. Those vary
# by distro; the packaging files list them (packaging/gamescope/PKGBUILD for Arch). gamescope
# vendors wlroots / libliftoff / vkroots / libdisplay-info / SPIRV-Headers / reshade as git
# SUBMODULES (plus two meson wraps for glm + stb), so both the clone and the build want network
# access unless the checkout already carries them — hence `--recurse-submodules` below.
set -euo pipefail

# The pinned upstream. Bump together with the patches (they are `git am`-able and rebase cheaply —
# two files, mirroring code that already exists in-tree; see README.md).
GAMESCOPE_REV="5fb8dce4a09d0a68d097b9faf9513782106bc843"
GAMESCOPE_REPO="https://github.com/ValveSoftware/gamescope.git"

REV="$GAMESCOPE_REV" PREFIX=/usr DESTDIR="" SRCDIR="" JOBS="" SETCAP=1 EXTRA_FALLBACK=""
while [ $# -gt 0 ]; do
  case "$1" in
    --rev)       REV="${2:?}"; shift 2 ;;
    --prefix)    PREFIX="${2:?}"; shift 2 ;;
    --destdir)   DESTDIR="${2:?}"; shift 2 ;;
    --srcdir)    SRCDIR="${2:?}"; shift 2 ;;
    --jobs)      JOBS="${2:?}"; shift 2 ;;
    --no-setcap) SETCAP=0; shift ;;
    # Extra `force_fallback_for` entries, comma-separated, appended to the mandatory three below.
    # Exists for ONE package family: the .deb has to install on both Debian 13 and Ubuntu 26.04,
    # and those two disagree on the libdisplay-info SONAME (0.2.0 -> libdisplay-info2 vs 0.3.0 ->
    # libdisplay-info3), so a package built against either one is uninstallable on the other.
    # Vendoring it makes ONE .deb serve both. Opt-in rather than baked in, so the Arch/Fedora/nix
    # packages — which have no such split and are shipping fine — keep producing exactly the binary
    # they produce today. Its only caller is the `build-publish-gamescope` job in
    # .gitea/workflows/deb.yml, which passes `--extra-fallback libdisplay-info`.
    --extra-fallback) EXTRA_FALLBACK="${2:?}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
# A staged install is a package build: the package manager owns file capabilities.
[ -n "$DESTDIR" ] && SETCAP=0

for tool in git meson ninja; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

HERE="$(cd "$(dirname "$0")" && pwd)"
PATCHES=("$HERE"/patches/*.patch)
[ -e "${PATCHES[0]}" ] || { echo "no patches found in $HERE/patches" >&2; exit 1; }
VERSION_TAG="$(sed -n 's/^+\(version_tag = vcs_tag .*\)/\1/p' "${PATCHES[@]}" | tail -n1)"
PFHDR="$(printf '%s\n' "$VERSION_TAG" | sed -nE "s/^version_tag = vcs_tag \+ '(\+pfhdr[0-9]+)' \+.*/\1/p")"
[ -n "$PFHDR" ] || { echo "patches contain no capability version stamp" >&2; exit 1; }

WORK=""
if [ -z "$SRCDIR" ]; then
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  SRCDIR="$WORK/gamescope"
  echo "==> cloning gamescope @ ${REV:0:12}"
  git clone --recurse-submodules "$GAMESCOPE_REPO" "$SRCDIR"
  git -C "$SRCDIR" checkout --recurse-submodules "$REV"
fi

# Reusing --srcdir requires the current stamp, not just any punktfunk marker.
if grep -q '+pfhdr' "$SRCDIR/src/meson.build"; then
  echo "==> checking existing punktfunk patch level in $SRCDIR"
else
  echo "==> applying punktfunk patches"
  git -C "$SRCDIR" \
    -c user.name=punktfunk -c user.email=packages@unom.io \
    am "${PATCHES[@]}"
fi
grep -Fxq "$VERSION_TAG" "$SRCDIR/src/meson.build" || {
  echo "source does not carry the current $PFHDR stamp; use a fresh gamescope checkout" >&2
  exit 1
}

BUILD="$SRCDIR/build-punktfunk"
echo "==> configuring"
# We ship ONE binary out of this tree, so build only what leads to it:
#   -Denable_tests=false        gamescope's own unit tests want Catch2 **v3**
#                               (`catch2-with-main`); Fedora ships v2, and building someone else's
#                               test suite is not our job either way.
#   -Denable_openvr_support     the VR integration pulls the openvr submodule + its build for a
#                               code path a headless capture session never enters.
#   -Denable_gamescope_wsi_layer ON, and installed under our own name below. This used to be off,
#                               on the grounds that the distro's gamescope package already ships a
#                               layer and that the layer is "version-independent of the compositor
#                               binary". That second half is FALSE: the layer and the compositor
#                               speak `gamescope_swapchain` to each other, and when they disagree
#                               the compositor rejects the client's `swapchain_feedback` and every
#                               Vulkan client dies on a black screen. A compositor we ship needs
#                               the layer we built beside it.
#   -Dinput_emulation=enabled   gamescope's EIS server, the only way keyboard and mouse reach a
#                               session the host spawns. `auto` drops it without libeis headers.
#
# `force_fallback_for` includes **wlroots** on purpose, and it is load-bearing for a binary we
# SHIP: gamescope vendors a wlroots submodule, but meson prefers a system one when the build host
# happens to have `wlroots-devel` — and then links it SHARED. The result starts fine on the build
# host and dies with `libwlroots-0.19.so: cannot open shared object file` anywhere else, which is
# every machine that installs our package. Fedora 43 has no wlroots-devel so it fell back and
# linked static by luck; Fedora 44's `dnf builddep gamescope` pulls one in, and the binary built
# there would not start on the host. Pinning the fallback makes the outcome the same everywhere.
# (gamescope's own meson.build hard-errors if libliftoff/vkroots are missing from this list, so
# all three go together.)
#
# **libdisplay-info is in the list for exactly the wlroots reason**, learned the hard way on the
# SteamOS VM 2026-08-23: it is a vendored submodule too, so a build box that merely HAS
# libdisplay-info-dev makes meson link it SHARED, and the binary then dies on SteamOS with
# `libdisplay-info.so.2: cannot open shared object file` — it builds, it installs, it prints its
# +pfhdr banner in the box, and build-gamescope.sh's on-glass check is the only thing between that
# and a host promising HDR it cannot deliver. Debian trixie has the -dev package, Fedora and Arch
# have it too, and any of them can pull it in transitively, so "don't install it" is not a fix
# that holds. Pinning the fallback makes the outcome the same everywhere, which is the whole
# point of this list.
#
# The C++ runtime goes STATIC for the same reason wlroots does: this binary is built on a ROLLING
# distro and has to start on a FROZEN one. Arch's gcc (16.1.1 when this was written) makes the
# compositor require `GLIBCXX_3.4.35`, and SteamOS 3.8.16 ships libstdc++ 3.4.34 — so the published
# Arch package died on the very platform the gamescope backend matters most on ("version
# GLIBCXX_3.4.35 not found"), while every other soname resolved and glibc was never close to the
# limit (the binary asks for 2.38 at most; SteamOS has 2.41). Safe because
# gamescope links NO shared C++ library (its `NEEDED` list is all C — glslang/SPIRV are build-time
# only), so no C++ ABI ever crosses a shared boundary; it costs ~1 MB.
# Appended to LDFLAGS rather than passed as `-Dcpp_link_args`, because that option would REPLACE
# the value meson derives from the environment and silently drop makepkg's hardening flags
# (`-z relro`, `-z now`, `--as-needed`).
export LDFLAGS="${LDFLAGS:-} -static-libstdc++ -static-libgcc"
meson setup "$BUILD" "$SRCDIR" \
  --prefix="$PREFIX" \
  --buildtype=release \
  -Dforce_fallback_for="libliftoff,vkroots,wlroots,libdisplay-info${EXTRA_FALLBACK:+,$EXTRA_FALLBACK}" \
  -Dpipewire=enabled \
  -Dinput_emulation=enabled \
  -Denable_tests=false \
  -Denable_openvr_support=false \
  -Denable_gamescope_wsi_layer=true

echo "==> building"
ninja -C "$BUILD" ${JOBS:+-j "$JOBS"}

# Our own names only: `ninja install` would put gamescopectl/gamescopereaper/gamescopestream and the
# WSI layer in the prefix, over the distro's gamescope. gamescope `execvp`s `gamescopereaper` for
# every child it starts, so ours goes to /usr/lib/punktfunk/gamescope, first on the host's PATH.
REAPER=$(find "$BUILD" -type f -name gamescopereaper -perm -u+x | head -1)
[ -n "$REAPER" ] || { echo "the build produced no gamescopereaper, so a session would run nothing" >&2; exit 1; }
install -Dm755 "$REAPER" "${DESTDIR}${PREFIX}/lib/punktfunk/gamescope/gamescopereaper"
BIN="$BUILD/src/gamescope"
[ -x "$BIN" ] || { echo "build produced no $BIN" >&2; exit 1; }
BANNER="$("$BIN" --version 2>&1)" || {
  echo "built gamescope cannot report its version: $BANNER" >&2
  exit 1
}
BANNER_RE=" version [^[:space:]]+\\${PFHDR} \\("
[[ "$BANNER" =~ $BANNER_RE ]] || {
  echo "built gamescope must carry exactly $PFHDR: $BANNER" >&2
  exit 1
}
# The static C++ runtime above is invisible in a successful build and only shows up as a binary
# that will not start on an older distro — so assert it here, where a mistake is a build failure
# instead of a package that dies at `--version` on SteamOS. No `libstdc++.so.6` in NEEDED is the
# whole invariant (and it needs no version threshold to check).
if command -v objdump >/dev/null; then
  objdump -p "$BIN" 2>/dev/null | grep -q 'NEEDED.*libstdc++' && {
    echo "built binary links libstdc++ dynamically — the static C++ runtime did not take, and this" >&2
    echo "package would not start on a distro older than the build host" >&2
    exit 1
  }
fi
DEST="${DESTDIR}${PREFIX}/bin/punktfunk-gamescope"
echo "==> installing $DEST"
install -Dm755 "$BIN" "$DEST"

# The WSI layer, under OUR name, at OUR path.
#
# A game nested under gamescope gets an HDR10 swapchain from this layer and from nothing else —
# gamescope advertises no runtime colour-management protocol a Mesa/NVIDIA WSI could negotiate
# through — so a compositor shipped WITHOUT a matching layer simply cannot do HDR for games. Built
# from this same tree at this same rev, so the two can never drift apart; that is the whole point,
# and it is what makes the host's old "compare version triples and hope" check unnecessary.
#
# It must not collide with the distro's layer and must be switchable independently of it, so the
# generated manifest is rewritten to carry our layer name, our library path and our own
# enable/disable variables. The Vulkan loader keys implicit layers on that NAME, so with a distinct
# one both layers can sit installed side by side and the host picks per session.
#
# python3 rather than sed because meson is itself a Python program — it is guaranteed present on any
# host that got this far — and a JSON edit belongs in a JSON parser.
LAYER_SO=$(find "$BUILD" -type f -name 'libVkLayer_*gamescope_wsi*.so' | head -1)
LAYER_SRC_JSON=$(find "$BUILD" -type f -name '*gamescope_wsi*.json' | head -1)
[ -n "$LAYER_SO" ] && [ -n "$LAYER_SRC_JSON" ] || {
  echo "the WSI layer did not build (.so=${LAYER_SO:-none} .json=${LAYER_SRC_JSON:-none}) — without" >&2
  echo "it no game in a punktfunk gamescope session can get an HDR10 swapchain" >&2
  exit 1
}
LAYER_LIB_PATH="${PREFIX}/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
LAYER_DEST_JSON="${DESTDIR}${PREFIX}/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
echo "==> installing ${DESTDIR}${LAYER_LIB_PATH}"
install -Dm755 "$LAYER_SO" "${DESTDIR}${LAYER_LIB_PATH}"
install -d "$(dirname "$LAYER_DEST_JSON")"
python3 "$(dirname "$0")/rewrite-wsi-layer-manifest.py" \
  "$LAYER_SRC_JSON" "$LAYER_DEST_JSON" "$LAYER_LIB_PATH"

if [ "$SETCAP" = 1 ] && command -v setcap >/dev/null; then
  # gamescope raises its own scheduling priority; without CAP_SYS_NICE it still runs, just noisier
  # and with worse frame pacing. Best-effort — needs root, and a package sets it declaratively.
  setcap 'CAP_SYS_NICE=eip' "$DEST" 2>/dev/null \
    || echo "note: could not setcap CAP_SYS_NICE on $DEST (run as root, or let the package do it)"
fi

echo "==> done: ${BANNER%%$'\n'*}"
echo "    verified exact $PFHDR capability marker"
