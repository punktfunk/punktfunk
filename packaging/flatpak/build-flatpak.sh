#!/usr/bin/env bash
# Build the punktfunk Linux client as a single-file `.flatpak` bundle.
#
# Works on the Steam Deck (org.flatpak.Builder from Flathub, user-scope, NO root) and on any
# Linux box with flatpak + flatpak-builder. The CI does the same steps (.gitea/workflows/flatpak.yml).
#
# On the Deck (one-time):
#   flatpak install --user -y flathub org.flatpak.Builder
# Then run this script from the repo root:
#   bash packaging/flatpak/build-flatpak.sh
# Output: dist/punktfunk-client-<version>-<arch>.flatpak  (install: `flatpak install --user <file>`)
#
# Env knobs:
#   VERSION=...        version string for the bundle name (default: git describe / 0.0.1-dev)
#   ONLINE=1           skip offline cargo-sources.json; build with --share=network (fast local
#                      iteration, non-reproducible). Default: offline (cargo-sources.json is
#                      reused when present, else generated; FORCE_GEN=1 regenerates it).
#   BUILDER=...        override the flatpak-builder invocation (default: auto-detect host
#                      flatpak-builder, else `flatpak run org.flatpak.Builder`).
#   BRANCH=...         the flatpak branch to build and bundle (default: stable). A Deck tracking
#                      the hosted `canary` takes a test build as BRANCH=canary, replacing it in
#                      place — a second branch beside it would win the plain `flatpak run`.
#   ARCH=...           target architecture (default: this machine's). `aarch64` builds the arm64
#                      client — the manifest carries the per-arch PKG_CONFIG_PATH and the matching
#                      prebuilt Skia archive. NOTE this is not a cross-compile: flatpak-builder
#                      runs the build inside a sandbox for that arch, so building aarch64 on an
#                      x86_64 box needs qemu binfmt registered and is very slow. Run it on an
#                      arm64 machine.
set -euo pipefail

ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

APP_ID="io.unom.punktfunk"
MANIFEST="packaging/flatpak/io.unom.punktfunk.yml"
VERSION="${VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo 0.0.1-dev)}"
VERSION="${VERSION#v}"
# `flatpak --default-arch` reports flatpak's own name for this machine (x86_64 / aarch64).
ARCH="${ARCH:-$(flatpak --default-arch 2>/dev/null || uname -m)}"
BRANCH="${BRANCH:-stable}"
# Arch-suffixed so an x86_64 and an aarch64 bundle can sit in dist/ together instead of the
# second silently overwriting the first. (CI composes its own published filename.)
BUNDLE="dist/punktfunk-client-${VERSION}-${ARCH}.flatpak"

# --- pick a flatpak-builder (host binary, or the org.flatpak.Builder flatpak on the Deck) ---
if [ -n "${BUILDER:-}" ]; then
  FPB=($BUILDER)
elif command -v flatpak-builder >/dev/null 2>&1; then
  FPB=(flatpak-builder)
elif flatpak info org.flatpak.Builder >/dev/null 2>&1; then
  FPB=(flatpak run org.flatpak.Builder)
else
  echo "error: need flatpak-builder. On the Deck: flatpak install --user -y flathub org.flatpak.Builder" >&2
  exit 1
fi

# --- ensure Flathub is available for the runtime/SDK/extensions ---
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo

# --- offline crate cache (skip with ONLINE=1) -------------------------------------------
EXTRA_ARGS=()
if [ "${ONLINE:-0}" = "1" ]; then
  echo "==> ONLINE build (cargo fetches from crates.io; non-reproducible)"
  EXTRA_ARGS+=(--build-args=--share=network)
  # The manifest references cargo-sources.json; provide an empty list so it stays valid.
  [ -f packaging/flatpak/cargo-sources.json ] || echo '[]' > packaging/flatpak/cargo-sources.json
elif [ -f packaging/flatpak/cargo-sources.json ] && [ "${FORCE_GEN:-0}" != "1" ]; then
  # Reuse a cargo-sources.json that was generated elsewhere (e.g. on a dev box with network +
  # python aiohttp/tomlkit, then rsynced to a build host that lacks them — like the Deck). The
  # offline crate cache is a pure function of Cargo.lock, so this is reproducible. FORCE_GEN=1
  # to regenerate anyway.
  echo "==> reusing existing packaging/flatpak/cargo-sources.json (FORCE_GEN=1 to regenerate)"
else
  echo "==> generating offline cargo-sources.json from Cargo.lock"
  # PINNED to a commit and checked by SHA-256 — same ref+sum as .gitea/workflows/flatpak.yml, so
  # the local build path and CI vendor crate sources with the identical script. `master` is a
  # mutable ref, and this is third-party python that chooses which crate sources the build (a
  # SIGNED one, in CI) vendors. Bump both together, here and in the workflow:
  #   curl -fsSL .../<new-sha>/cargo/flatpak-cargo-generator.py | sha256sum
  GEN_REF=f03a673abe6ce189cea1c2857e2b44af2dd79d1f
  GEN_SHA=b373c8ab1a05378ec5d8ed0645c7b127bcec7d2f7a1798694fbc627d570d856c
  GEN=/tmp/flatpak-cargo-generator.py
  if [ ! -f "$GEN" ]; then
    curl -fsSL -o "$GEN" \
      "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$GEN_REF/cargo/flatpak-cargo-generator.py"
  fi
  # Verified on EVERY run, not just after a download: the branch above reuses whatever is already
  # at that /tmp path — which on a shared box is a file this script did not write.
  echo "$GEN_SHA  $GEN" | sha256sum -c - \
    || { echo "error: $GEN does not match the pin for $GEN_REF — rm it and re-run" >&2; exit 1; }
  # Needs python3 + aiohttp + tomlkit. On a host that lacks them (e.g. the Deck), generate on the
  # Mac / a dev box instead and rsync the result next to the manifest (reused by the branch above).
  # Prune the microsoft/windows-rs git crates first (punktfunk-client-windows only) — otherwise
  # flatpak-builder full-clones that multi-GB repo and fills the disk. See prune-windows-lock.py.
  python3 packaging/flatpak/prune-windows-lock.py Cargo.lock /tmp/Cargo.flatpak.lock
  python3 "$GEN" /tmp/Cargo.flatpak.lock -o packaging/flatpak/cargo-sources.json
fi

# --- build into a local ostree repo, then export a single-file bundle --------------------
# The manifest's cargo step stamps this into the binaries (PUNKTFUNK_BUILD_VERSION).
printf '%s\n' "$VERSION" > packaging/flatpak/VERSION
echo "==> flatpak-builder ($APP_ID, version $VERSION, arch $ARCH)"
# --default-branch=stable matches CI / the hosted repo ref, so a locally-built install can also
# track flatpak.unom.io. build-bundle must then be told the branch (else it defaults to `master`).
# --disable-updates matches CI (see .gitea/workflows/flatpak.yml for the full why): every git
# source in the manifest is commit-pinned, so re-running this script reuses the mirrors already in
# the .flatpak-builder state dir instead of re-fetching them.
"${FPB[@]}" --user --force-clean --disable-rofiles-fuse \
  --default-branch="$BRANCH" \
  --disable-updates \
  --arch="$ARCH" \
  --install-deps-from=flathub \
  "${EXTRA_ARGS[@]}" \
  --repo="$ROOTDIR/.flatpak-repo" \
  "$ROOTDIR/.flatpak-build" "$MANIFEST"

mkdir -p dist
flatpak build-bundle --arch="$ARCH" "$ROOTDIR/.flatpak-repo" "$BUNDLE" "$APP_ID" "$BRANCH"
echo "built $BUNDLE"
ls -lh "$BUNDLE"
echo
echo "install:  flatpak install --user -y $BUNDLE"
echo "run:      flatpak run $APP_ID            (or:  flatpak run $APP_ID --connect host:port)"
