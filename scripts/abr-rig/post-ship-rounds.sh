#!/usr/bin/env bash
# The post-ship rows (9a, 9b, 9c, 9e) on a real stream, in one unattended pass. Tags are
# `ps-<row>-<run>`; `ALL DONE` is the last line.
#
#   scripts/abr-rig/post-ship-rounds.sh
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out

keep() { # <tag> <profile> [probes]
  local n
  for n in $(seq "${3:-1}"); do
    # `-p<n>` is the spelling read-shared.py reads for a two-session row.
    local sfx=""
    [ "${3:-1}" != 1 ] && sfx="-p$n"
    cp "$OUT/$2-$n.jsonl" "$OUT/$1$sfx.jsonl" 2>/dev/null || true
    cp "$OUT/$2-$n.log" "$OUT/$1$sfx-client.log" 2>/dev/null || true
  done
  cp "$OUT/$2-host.log" "$OUT/$1-host.log" 2>/dev/null || true
}

row() { # <tag> <profile> <seconds> <probes> [env assignments…]
  local tag=$1 profile=$2 secs=$3 probes=$4
  shift 4
  env "$@" "$HERE/run.sh" "$profile" "$secs" > "$OUT/$tag.run" 2>&1
  local rc=$?
  keep "$tag" "$profile" "$probes"
  echo "  $tag rc=$rc"
}

# Build up front; every later run's cargo build is a no-op.
echo "== build =="
"$HERE/run.sh" nowall_720p 1 > "$OUT/ps-build.run" 2>&1
echo "  build rc=$?"
# The ramp's per-step lines and the drain guard's are debug.
export RUST_LOG=${RUST_LOG:-info,punktfunk_core::abr=debug}

echo "== 9e: loss-free LAN, ramp steps whole =="
for r in 1 2 3; do row "ps-lan-$r" nowall_720p 60 1; done

echo "== 9a: tunnel, ramp cut short by a 100 ms bring-up =="
for r in 1 2 3; do row "ps-short-$r" wan_wg_12 120 1 PF_RIG_BRINGUP_MS=100; done

echo "== 9c: ramp x20 on a roomy link at 0.5 % loss =="
for r in $(seq 20); do row "ps-ramp-$r" ramp_loss 20 1; done

echo "== 9c: still desktop on the tunnel =="
for r in 1 2 3; do row "ps-still-$r" wan_still 300 1; done

echo "== 9c: Wi-Fi =="
for r in 1 2 3; do row "ps-wifi-$r" wifi_tv 300 1; done

echo "== 9c: tunnel =="
for r in 1 2 3; do row "ps-wan-$r" wan_wg_12 600 1; done

echo "== 9b: pinned survivor =="
for r in 1 2 3; do row "ps-fixsurv-$r" shared_fixed_survivor 600 2; done

echo "== 9b: leaver =="
for r in 1 2 3; do row "ps-leaver-$r" shared_leaver 600 2; done

echo "== 9b: two Automatic =="
for r in 1 2 3; do row "ps-two-$r" shared_two_auto 600 2; done

echo "ALL DONE"
