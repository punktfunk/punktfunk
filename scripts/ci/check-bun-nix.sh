#!/bin/sh
# Drift gate for the generated bun2nix lockfile expressions (web/bun.nix, sdk/bun.nix).
#
# `bun.nix` is a DERIVED file: bun2nix is a pure function of `bun.lock` (it reads the lockfile text
# and emits one `fetchurl` per package, keyed by the lockfile's own integrity hashes — see
# packaging/nix/README.md). Nothing but the lockfile goes in, so any disagreement between the two
# committed files is drift, and it is always mechanically fixable.
#
# Why this exists: `bun.nix` regenerates only from a local `bun install` that runs lifecycle
# scripts (web's `postinstall`, the SDK's `prepare`). It does NOT regenerate on:
#
#   * `bun install --ignore-scripts` — which is what EVERY bun install in CI uses (ci.yml,
#     web-screenshots.yml, windows-host.yml, sdk-publish.yml), because web's `postinstall` shells
#     out to a `bun` on PATH that CI's portable bun isn't;
#   * a merge or rebase — git merges `bun.lock` and `bun.nix` as two unrelated files, so a branch
#     that generated `bun.nix` before picking up someone else's lockfile change silently commits
#     the pair out of step;
#   * a lockfile edited or re-resolved by hand.
#
# A stale pair breaks the Nix build: it fetches node_modules strictly from `bun.nix`, so the
# offline `bun install` inside the derivation is asked for a tarball the store cache does not
# contain and `punktfunk-web` fails to build — with a "package not found" that names npm, not the
# lockfile that actually drifted.
#
# The gate also enforces the version pin the flake and README only *state*: `bun.nix` has no schema
# stability across bun2nix releases, so the flake input ref and BOTH npm devDependencies must name
# the same exact version. A half-moved pin regenerates the file with a generator the flake does
# not use.
#
# It also holds every other bun pin (the oven/bun images, flake.nix's overlay, the Windows portable
# bun, the winget server) to the release in ci/bun.env, which the Linux builders install.
#
# The list of packages to check is read out of packaging/nix/packages.nix (its `bunNix = src + …`
# lines) rather than hardcoded here, so a third bun package is covered the day it is added — and an
# empty list is a hard error, because a gate that checks nothing passes exactly like a clean tree.
#
# Usage:
#   scripts/ci/check-bun-nix.sh          # verify; non-zero on drift (CI)
#   scripts/ci/check-bun-nix.sh --fix    # regenerate the committed files in place
set -eu

FIX=0
if [ $# -gt 0 ]; then
    case "$1" in
        --fix) FIX=1 ;;
        *) echo "usage: $0 [--fix]" >&2; exit 2 ;;
    esac
fi

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
PACKAGES_NIX="$ROOT/packaging/nix/packages.nix"
FLAKE="$ROOT/flake.nix"

command -v bun >/dev/null 2>&1 || {
    echo "check-bun-nix: bun is not on PATH (needed to run bun2nix and to read package.json)" >&2
    exit 1
}
[ -f "$PACKAGES_NIX" ] || { echo "check-bun-nix: no $PACKAGES_NIX" >&2; exit 1; }
[ -f "$FLAKE" ] || { echo "check-bun-nix: no $FLAKE" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- the pinned bun2nix version -------------------------------------------------------------------
# flake.nix:  url = "github:nix-community/bun2nix?ref=<version>";
PINNED=$(sed -n 's/.*github:nix-community\/bun2nix?ref=\([^"]*\)".*/\1/p' "$FLAKE" | head -1)
[ -n "$PINNED" ] || {
    echo "check-bun-nix: could not read the bun2nix input ref out of $FLAKE." >&2
    echo "Expected a line like: url = \"github:nix-community/bun2nix?ref=<version>\";" >&2
    exit 1
}

# --- which packages carry a generated bun.nix -----------------------------------------------------
# packages.nix:  bunDeps = bun2nix.fetchBunDeps { bunNix = src + "/web/bun.nix"; };
sed -n 's/.*bunNix *= *src *+ *"\/\(.*\)\/bun\.nix".*/\1/p' "$PACKAGES_NIX" | sort -u > "$TMP/roots"
if [ ! -s "$TMP/roots" ]; then
    echo "check-bun-nix: found no \`bunNix = src + \"/<dir>/bun.nix\"\` in $PACKAGES_NIX." >&2
    echo "Either the bun packages were removed (delete this gate) or the expression changed shape" >&2
    echo "and the gate silently stopped checking anything. Not passing vacuously." >&2
    exit 1
fi

fail=0
checked=0

# --- version pin agreement ------------------------------------------------------------------------
# `bun.nix` has no schema stability across bun2nix versions, so the generator the flake builds with
# and the generator `bun install` runs must be the SAME exact version (packaging/nix/README.md).
while read -r dir; do
    pkgjson="$ROOT/$dir/package.json"
    [ -f "$pkgjson" ] || { echo "check-bun-nix: no $pkgjson" >&2; fail=1; continue; }
    dev=$(bun -e "const d=require(process.argv[1]).devDependencies||{};console.log(d.bun2nix??'')" \
        "$pkgjson")
    if [ "$dev" != "$PINNED" ]; then
        echo "check-bun-nix: bun2nix version pin disagrees." >&2
        echo "  flake.nix input ref            : $PINNED" >&2
        echo "  $dir/package.json devDependency : ${dev:-<absent>}" >&2
        echo "These must be the same exact version — bun.nix has no schema stability across" >&2
        echo "bun2nix releases. Move both together, then rerun this script with --fix." >&2
        fail=1
    fi
done < "$TMP/roots"

# --- the bun runtime pin ---------------------------------------------------------------------------
# ci/bun.env names the one bun release; every image tag and download URL elsewhere must match it,
# and flake.nix's x86_64 hash must be the same asset's sum. Release notes and the changelog are
# history and are not checked.
BUN_PIN=$(sed -n 's/^BUN_VERSION=//p' "$ROOT/ci/bun.env")
BUN_SHA=$(sed -n 's/^BUN_SHA=//p' "$ROOT/ci/bun.env")
[ -n "$BUN_PIN" ] && [ -n "$BUN_SHA" ] || { echo "check-bun-nix: no BUN_VERSION/BUN_SHA in ci/bun.env" >&2; exit 1; }
git -C "$ROOT" grep -n -o -E 'oven/bun:[0-9][0-9.]*[0-9]|bun-v[0-9][0-9.]*[0-9]' -- \
    ':!CHANGELOG.md' ':!docs/releases' > "$TMP/bunpins" || true
[ -s "$TMP/bunpins" ] || { echo "check-bun-nix: found no bun pin outside ci/bun.env" >&2; exit 1; }
while IFS= read -r hit; do
    case "$hit" in
        *"oven/bun:$BUN_PIN" | *"bun-v$BUN_PIN") ;;
        *) echo "check-bun-nix: $hit disagrees with ci/bun.env (bun $BUN_PIN)" >&2; fail=1 ;;
    esac
done < "$TMP/bunpins"
grep -q "version = \"$BUN_PIN\";" "$FLAKE" || {
    echo "check-bun-nix: flake.nix's bun overlay does not set version = \"$BUN_PIN\"" >&2; fail=1; }
BUN_SRI=$(bun -e "console.log('sha256-'+Buffer.from(process.argv[1],'hex').toString('base64'))" "$BUN_SHA")
grep -q "hash = \"$BUN_SRI\";" "$FLAKE" || {
    echo "check-bun-nix: flake.nix has no x86_64 bun hash $BUN_SRI (BUN_SHA in ci/bun.env)" >&2; fail=1; }

# --- the generator ---------------------------------------------------------------------------------
# Prefer an already-installed bun2nix at the pinned version (fast, offline — the dev case); otherwise
# fetch exactly the pinned one, once, into $TMP. Never a floating `bunx bun2nix`: that would generate
# with whatever is newest, and `bun.nix` has no schema stability across releases.
BUN2NIX=""
while read -r dir; do
    cand="$ROOT/$dir/node_modules/bun2nix/index.ts"
    [ -f "$cand" ] || continue
    have=$(bun -e "console.log(require(process.argv[1]).version??'')" \
        "$ROOT/$dir/node_modules/bun2nix/package.json" 2>/dev/null || echo '')
    if [ "$have" = "$PINNED" ]; then BUN2NIX="$cand"; break; fi
done < "$TMP/roots"

if [ -z "$BUN2NIX" ]; then
    # Installed in its own scratch dir, so this never touches the repo's lockfiles or .npmrc.
    mkdir -p "$TMP/gen"
    if ! ( cd "$TMP/gen" && bun add --exact "bun2nix@$PINNED" ) > "$TMP/geninstall.log" 2>&1; then
        echo "check-bun-nix: could not install bun2nix@$PINNED" >&2
        cat "$TMP/geninstall.log" >&2
        exit 1
    fi
    BUN2NIX="$TMP/gen/node_modules/bun2nix/index.ts"
    [ -f "$BUN2NIX" ] || { echo "check-bun-nix: bun2nix@$PINNED installed but $BUN2NIX is absent" >&2; exit 1; }
fi

run_bun2nix() { # <lockfile> <outfile>
    bun "$BUN2NIX" --lock-file "$1" --output-file "$2"
}

# --- regenerate + compare ---------------------------------------------------------------------------
while read -r dir; do
    lock="$ROOT/$dir/bun.lock"
    nix="$ROOT/$dir/bun.nix"
    [ -f "$lock" ] || { echo "check-bun-nix: no $lock (packages.nix expects $dir/bun.nix)" >&2; fail=1; continue; }

    out="$TMP/$(echo "$dir" | tr '/' '_').bun.nix"
    run_bun2nix "$lock" "$out" >/dev/null

    if [ "$FIX" -eq 1 ]; then
        if [ ! -f "$nix" ] || ! cmp -s "$nix" "$out"; then
            cp "$out" "$nix"
            echo "check-bun-nix: regenerated $dir/bun.nix from $dir/bun.lock"
        else
            echo "check-bun-nix: $dir/bun.nix already in sync"
        fi
        checked=$((checked + 1))
        continue
    fi

    if [ ! -f "$nix" ]; then
        echo "check-bun-nix: $dir/bun.nix is MISSING — packages.nix fetches node_modules from it." >&2
        fail=1
        continue
    fi
    # Plain files, not `diff <(…) <(…)`: ci.yml runs `sh scripts/ci/check-bun-nix.sh`, and
    # POSIX sh has no process substitution — it would be rejected at parse time and the gate
    # would never compare anything.
    if cmp -s "$nix" "$out"; then
        echo "check-bun-nix: $dir/bun.nix matches $dir/bun.lock"
    else
        echo "check-bun-nix: $dir/bun.nix is STALE — it does not match $dir/bun.lock." >&2
        echo "The Nix build fetches node_modules only from bun.nix, so punktfunk's bun packages" >&2
        echo "would build against the wrong dependency set (or fail to fetch it at all)." >&2
        echo "Regenerate and commit it:  scripts/ci/check-bun-nix.sh --fix" >&2
        echo "--- diff (committed -> regenerated from bun.lock) ---" >&2
        diff -u "$nix" "$out" >&2 || true
        fail=1
    fi
    checked=$((checked + 1))
done < "$TMP/roots"

[ "$checked" -gt 0 ] || { echo "check-bun-nix: checked nothing — refusing to report success" >&2; exit 1; }
if [ "$fail" -eq 0 ] && [ "$FIX" -eq 0 ]; then
    echo "check-bun-nix: $checked bun package(s) in sync, bun2nix pinned at $PINNED, bun $BUN_PIN everywhere"
fi
exit "$fail"
