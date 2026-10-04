#!/usr/bin/env bash
# Regenerate THIRD-PARTY-NOTICES.txt for the Rust workspace.
#
# Goes through the dependency-free offline generator (scripts/gen-third-party-notices.py, reads
# `cargo metadata` plus the cargo registry cache) — see the ⚠ note below for why NOT `cargo about`.
# Run it when the dependency tree changes. Nothing regenerates this during packaging: the deb, rpm
# and Windows installer scripts all ship the committed file verbatim. What holds it honest is the
# THIRD-PARTY-NOTICES drift gate in ci.yml's rust job, which runs this and diffs the result.
#
# Usage: scripts/gen-third-party-notices.sh [output-file]
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-THIRD-PARTY-NOTICES.txt}"

# ⚠ The root file goes through the PYTHON generator, NOT `cargo about` — deliberately, and this
# is not a fallback. `cargo about` only ever sees CARGO dependencies, so it silently omits the
# VENDORED_TREES below: pyrowave, the Granite subset, volk, Vulkan-Headers, the Font Awesome brand
# icons and Simple Icons. Those are third-party sources shipped INSIDE first-party crates, each
# under its own licence, and dropping them from an attribution file is a legal regression rather
# than an untidiness. Measured 2026-08-13: `cargo about` produced 7,274 lines / ~514 crates with
# zero mentions of volk, Vulkan-Headers or Font Awesome, against the python generator's 17,324
# lines / 575 crates with all of them. This script used to prefer cargo-about whenever it was
# installed, so simply HAVING it on your PATH silently degraded the file.
#
# `cargo about` is still what the CI licence GATE runs (.gitea/workflows/audit.yml) — that job
# checks every licence is in the about.toml allowlist and writes to /dev/null, which is a
# different question from what this file must contain. If about.hbs ever learns to emit the
# vendored trees, preferring cargo-about here again would be reasonable.
echo "==> gen-third-party-notices.py -> $OUT" >&2
# The root file also names the seat keeper's crates: a separate workspace the host installer ships.
# No host build fetches them, and the generator reads metadata offline.
cargo fetch --locked --manifest-path crates/pf-seat-keeper/Cargo.toml >&2
python3 scripts/gen-third-party-notices.py --out "$OUT" --manifest Cargo.toml \
    --manifest crates/pf-seat-keeper/Cargo.toml
echo "==> wrote $OUT" >&2

# Regenerate the per-client in-tree copies. EVERY client has one now, because every client SHOWS
# it: the mobile apps bundle theirs as a resource/asset for their Acknowledgements screen, and the
# two desktop shells `include_str!` theirs onto their Licenses page (the MSIX and the client .deb
# ship the file as well).
#
# These are GENERATED, not copied. They used to be the workspace-wide file, which attributed to
# every client every crate anything in this repo links: FFmpeg, the NVENC SDK, GTK4, windows-rs.
# The Apple app links ONE Rust crate (punktfunk-ffi, through PunktfunkCore.xcframework — see
# scripts/build-xcframework.sh) and Android links the JNI bridge over it; everything else in those
# apps is Swift/Kotlin and platform frameworks.
#
# M10 — the client's FFmpeg excision — is what turned the same untidiness on the DESKTOP copies
# into a false statement a user can see: the shells print an `ffmpeg-next 8.1.0 — WTFPL` line and
# the full FFmpeg licence text three screens under a card saying no FFmpeg is bundled. So they are
# scoped too, each to the binaries its package actually installs — the shell, the session streamer,
# the headless CLI, and on Linux the update helper (`pf-update` ships as pf-update-client).
#
# The ROOT file stays workspace-wide on purpose: the HOST ships out of it, and the host does still
# link FFmpeg.
#
# Only the offline generator can scope a file (cargo-about renders the whole workspace), so these
# always go through it, and so does the root file — see the ⚠ note above.
if [ "$OUT" = "THIRD-PARTY-NOTICES.txt" ]; then
    # <in-tree path> <workspace members whose closure it must state>
    while read -r dest packages; do
        [ -n "$dest" ] || continue
        [ -d "$(dirname "$dest")" ] || continue
        python3 scripts/gen-third-party-notices.py --packages "$packages" --out "$dest"
        echo "==> generated $dest ($packages closure)" >&2
    done <<'CLIENTS'
clients/apple/Sources/PunktfunkKit/Resources/THIRD-PARTY-NOTICES.txt punktfunk-ffi
clients/android/app/src/main/assets/THIRD-PARTY-NOTICES.txt punktfunk-client-android
clients/linux/THIRD-PARTY-NOTICES.txt punktfunk-client-linux,punktfunk-client-session,punktfunk-cli,pf-update
clients/windows/THIRD-PARTY-NOTICES.txt punktfunk-client-windows,punktfunk-client-session,punktfunk-cli
CLIENTS
fi
