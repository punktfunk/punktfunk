#!/bin/sh
# Bound the published Nix binary cache on unom-1.
#
# WHY THIS EXISTS UP FRONT, rather than being added after the box fills: the flatpak repo next
# door taught this exact lesson the expensive way. It publishes with rsync WITHOUT --delete (so
# a client mid-download is never broken), nothing ever removed the superseded objects, and it
# reached 3.84 GB on a box that had already run out of disk once. This cache has the same
# publish model and the same growth shape — every build whose inputs moved adds a fresh set of
# store paths and keeps the old ones — so it gets the sweep from day one.
#
# The order below is the whole correctness argument:
#
#   1. Delete narinfos older than $DAYS, except those named in the keep file. rsync -a carries
#      the CI-side mtime over, and `nix copy` rewrites every narinfo it publishes on every run,
#      so age means "no publish has referenced this in $DAYS". The keep file lists the latest
#      stable release, which can go longer than $DAYS without a publish.
#   2. THEN delete NARs no surviving narinfo points at, once they are a day old. A publish
#      uploads NARs before narinfos, so a younger orphan may belong to a publish in flight.
#
# Doing it the other way round — or aging the NARs independently — can strand a live narinfo
# pointing at a deleted NAR, and that is strictly worse than a cache miss: nix reports a missing
# NAR as a hard download failure, not as "not cached, build it yourself".
#
# POSIX sh: this runs over ssh on unom-1 (Debian), invoked by .gitea/workflows/nix.yml.
#
# Usage:  sh prune.sh <cache-dir> [max-age-days] [keep-file]     (default 180, keep nothing)
#         sh prune.sh --self-test
set -eu

self_test() {
	# Smallest thing that fails if the ordering or the reference sweep breaks.
	t="$(mktemp -d)"
	trap 'rm -rf "$t"' EXIT
	mkdir -p "$t/nar"
	printf 'StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 41\n' >"$t/nix-cache-info"

	# A live path, a stale one, and a NAR nothing ever pointed at.
	printf 'StorePath: /nix/store/aaa-live\nURL: nar/live.nar.xz\n' >"$t/aaa.narinfo"
	printf 'StorePath: /nix/store/bbb-stale\nURL: nar/stale.nar.xz\n' >"$t/bbb.narinfo"
	: >"$t/nar/live.nar.xz"
	: >"$t/nar/stale.nar.xz"
	: >"$t/nar/orphan.nar.xz"
	# A NAR uploaded by a publish whose narinfos have not landed yet.
	: >"$t/nar/inflight.nar.xz"
	# Two paths that SHARE a NAR, one stale and one live: the shared NAR must survive. A sweep
	# that deleted NARs per-evicted-narinfo instead of by surviving references would drop it.
	printf 'StorePath: /nix/store/ccc-live\nURL: nar/shared.nar.xz\n' >"$t/ccc.narinfo"
	printf 'StorePath: /nix/store/ddd-stale\nURL: nar/shared.nar.xz\n' >"$t/ddd.narinfo"
	: >"$t/nar/shared.nar.xz"
	# The stable release: old, but named in the keep file.
	printf 'StorePath: /nix/store/eee-stable\nURL: nar/release.nar.xz\n' >"$t/eee.narinfo"
	: >"$t/nar/release.nar.xz"
	printf 'eee.narinfo\n' >"$t/stable.narinfos"

	# Age the stale ones well past any plausible threshold (portable -t form: YYYYMMDDhhmm).
	touch -t 200001010000 "$t/bbb.narinfo" "$t/ddd.narinfo" "$t/eee.narinfo" \
		"$t/nar/stale.nar.xz" "$t/nar/orphan.nar.xz" "$t/nar/release.nar.xz"

	prune "$t" 180 "$t/stable.narinfos"

	fail() { echo "SELF-TEST FAILED: $1" >&2; exit 1; }
	[ -f "$t/aaa.narinfo" ] || fail "evicted a fresh narinfo"
	[ -f "$t/nar/live.nar.xz" ] || fail "evicted a referenced NAR"
	[ -f "$t/nix-cache-info" ] || fail "deleted nix-cache-info"
	[ ! -f "$t/bbb.narinfo" ] || fail "kept a stale narinfo"
	[ ! -f "$t/nar/stale.nar.xz" ] || fail "kept a NAR nothing references any more"
	[ ! -f "$t/nar/orphan.nar.xz" ] || fail "kept an orphan NAR"
	[ -f "$t/nar/inflight.nar.xz" ] || fail "deleted a NAR a publish in flight just uploaded"
	[ -f "$t/nar/shared.nar.xz" ] || fail "deleted a NAR a surviving narinfo still references"
	[ -f "$t/eee.narinfo" ] || fail "evicted a narinfo the keep file names"
	[ -f "$t/nar/release.nar.xz" ] || fail "deleted the stable release's NAR"
	echo "prune.sh self-test OK"
}

prune() {
	root="$1"
	days="$2"
	# Read before the cd, so a relative path still resolves. Missing means keep nothing.
	exempt="$(mktemp)"
	cat "${3:-/dev/null}" >"$exempt" 2>/dev/null || true
	cd "$root"

	before="$(du -sh . 2>/dev/null | cut -f1)"

	# 1. Age out the narinfos, sparing the keep file's.
	find . -maxdepth 1 -name '*.narinfo' -mtime "+$days" | sed 's|^\./||' |
		grep -vxF -f "$exempt" | tr '\n' '\0' | xargs -0 -r rm -f

	# 2. Sweep NARs nothing points at any more. Both lists are relative to $root and spelled the
	#    same way ("nar/<file>") so `comm` can diff them.
	keep="$(mktemp)"
	have="$(mktemp)"
	# `|| true`: an empty cache (or one whose narinfos were all just evicted) makes the glob
	# match nothing, and an empty keep-list is the correct answer there, not an error.
	cat ./*.narinfo 2>/dev/null | sed -n 's|^URL: ||p' | sort -u >"$keep" || true
	find nar -type f -mtime +0 2>/dev/null | sed 's|^\./||' | sort >"$have" || true
	comm -13 "$keep" "$have" | tr '\n' '\0' | xargs -0 -r rm -f
	rm -f "$keep" "$have" "$exempt"

	# Leave the numbers in the deploy log — this is the only place the published size is visible.
	echo "cache pruned (narinfos older than ${days}d): ${before:-?} -> $(du -sh . 2>/dev/null | cut -f1) in $(pwd)"
}

case "${1:-}" in
--self-test) self_test ;;
"") echo "usage: prune.sh <cache-dir> [max-age-days] [keep-file] | --self-test" >&2; exit 2 ;;
*) prune "$1" "${2:-180}" "${3:-}" ;;
esac
