#!/bin/sh
# Ensure `sccache` is on PATH, self-healing on a runner/image that does not already carry it.
#
# WHY THIS EXISTS: one copy per platform, here, instead of one in every job that needs sccache,
# so a version bump or a mirror change is one edit. The two dialects differ: the Linux branch
# passes `--wildcards` to GNU tar, the macOS one must NOT (bsdtar globs by default and rejects
# the flag).
#
# The builder images (ci/*.Dockerfile) BAKE sccache, so on Linux this is a no-op in the normal case;
# it stays because a job runs against the image from the PREVIOUS push (docker.yml's bootstrap lag),
# and the macOS runner is a persistent host with no image at all.
#
# POSIX sh on purpose: Gitea's act_runner executes a step's `run:` under `sh -e` (dash) inside the
# Linux job containers — no bashisms, no process substitution (see the shader-gate note in ci.yml).
#
# Usage:  sh scripts/ci/ensure-sccache.sh
set -e

# Keep in step with the ARG SCCACHE_VERSION in ci/*.Dockerfile — the images bake this same version,
# and a job that heals to a DIFFERENT one would quietly split the shared cache's key universe in two
# (sccache's cache keys are not versioned across incompatible releases).
SCCACHE_VERSION="${SCCACHE_VERSION:-0.10.0}"

# Fork PRs do not receive SCCACHE_* secrets. CMake stores the launcher name
# `sccache` in the build dir (and in a restored target/ cache), so the name has
# to keep resolving — shadow it with a passthrough shim earlier on PATH. Never
# overwrite the installed binary: the macOS runner is a persistent host, and a
# clobbered binary stays broken for every job that follows this one.
disable_sccache_wrappers() {
    shim_dir="${RUNNER_TEMP:-$(mktemp -d)}/sccache-passthrough"
    mkdir -p "$shim_dir"
    # bash, not sh: dash drops environment names it cannot parse, and cargo's
    # `CARGO_BIN_EXE_punktfunk-setup` has a hyphen, so `env!` in that test fails.
    printf '%s\n' \
        '#!/bin/bash' \
        'case "$1" in' \
        '--version|--show-stats|--start-server|--stop-server) exit 0 ;;' \
        'esac' \
        'exec "$@"' > "$shim_dir/sccache"
    chmod 0755 "$shim_dir/sccache"
    PATH="$shim_dir:$PATH"
    export PATH
    if [ -n "${GITHUB_PATH:-}" ]; then
        echo "$shim_dir" >> "$GITHUB_PATH"
    fi
    echo "sccache shimmed to a compiler passthrough at $shim_dir"
}

probe_sccache() {
    [ "${RUSTC_WRAPPER:-}" = "sccache" ] || return 0
    if [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
        echo "sccache secrets absent; compiler passthrough"
        disable_sccache_wrappers
        return 0
    fi
    # --show-stats reaches a running server or starts one. --start-server fails
    # on the server a previous job left behind (persistent macOS runner) and
    # would read a healthy cache as broken storage.
    probe_log=$(mktemp)
    if sccache --show-stats >"$probe_log" 2>&1; then
        rm -f "$probe_log"
        return 0
    fi
    echo "sccache storage is not usable; compiler passthrough"
    cat "$probe_log" >&2 || true
    rm -f "$probe_log"
    disable_sccache_wrappers
}

if command -v sccache >/dev/null 2>&1; then
    sccache --version
    probe_sccache
    exit 0
fi

BASE="https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}"

# Download ONE release asset and refuse to unpack it unless its bytes match the pinned SHA-256.
# This binary becomes RUSTC_WRAPPER — every compiler invocation in the release builds, including the
# jobs that hold the RPM/Flatpak signing keys, runs through it — and a GitHub release asset is
# MUTABLE at a fixed URL, so the version in the path vouches for nothing on its own.
# Usage: fetch <arch-triple> <sha256>; leaves the verified tarball in $TARBALL.
fetch() {
    TARBALL="$(mktemp)"
    curl -fsSL "$BASE/sccache-v${SCCACHE_VERSION}-$1.tar.gz" -o "$TARBALL"
    # sha256sum where present, else shasum, which macOS ships.
    got="$(sha256sum "$TARBALL" 2>/dev/null || shasum -a 256 "$TARBALL")"
    got="${got%% *}"
    if [ "$got" != "$2" ]; then
        echo "sccache $1 sha256 mismatch: got $got, pinned $2" >&2
        echo "(bumping SCCACHE_VERSION? update the pins below it from the release's .sha256 files)" >&2
        exit 1
    fi
}

case "$(uname -s)" in
    Darwin)
        # The macOS runner is a LaunchDaemon run as the installing user, not root
        # (setup-macos-runner.sh) — install into the user prefix. ~/.local/bin is already on the
        # daemon's PATH; GITHUB_PATH is belt-and-braces for the steps that follow in THIS job.
        DEST="$HOME/.local/bin"
        mkdir -p "$DEST"
        case "$(uname -m)" in
            arm64|aarch64) ARCH=aarch64-apple-darwin
                           SHA=5aba39252e2efa26bd76144f87ac59787d60fe567ab785e27e2a8c8190892eac ;;
            *)             ARCH=x86_64-apple-darwin
                           SHA=6d4a77802ec83607478df7b6338be28171e65e58a38a49497ebec1fbb300fce4 ;;
        esac
        fetch "$ARCH" "$SHA"
        # bsdtar globs by default and does not accept --wildcards.
        tar -xz --strip-components=1 -C "$DEST" -f "$TARBALL" '*/sccache'
        rm -f "$TARBALL"
        chmod 0755 "$DEST/sccache"
        PATH="$DEST:$PATH"
        export PATH
        if [ -n "${GITHUB_PATH:-}" ]; then
            echo "$DEST" >> "$GITHUB_PATH"
        fi
        ;;
    *)
        # Linux job containers run as root; /usr/local/bin is on PATH already, so no GITHUB_PATH
        # dance is needed. The musl build is static — one binary serves the Ubuntu, Fedora and Arch
        # images alike.
        DEST=/usr/local/bin
        fetch x86_64-unknown-linux-musl \
            1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b
        tar -xz --wildcards --strip-components=1 -C "$DEST" -f "$TARBALL" '*/sccache'
        rm -f "$TARBALL"
        chmod 0755 "$DEST/sccache"
        ;;
esac

sccache --version
probe_sccache
