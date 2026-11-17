#!/usr/bin/env bash
# The bring-up ramp's rows: the no-wall profile's ten, the failure paths, and the
# sessions. One script so a long stretch of real time runs unattended.
#
#   scripts/abr-rig/ramp-rounds.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out

keep() { # <tag> <profile>
  cp "$OUT/$2-1.jsonl" "$OUT/$1.jsonl" 2>/dev/null || true
  cp "$OUT/$2-host.log" "$OUT/$1-host.log" 2>/dev/null || true
  cp "$OUT/$2-1.log" "$OUT/$1-client.log" 2>/dev/null || true
}

# Build up front; every later run's cargo build is a no-op.
"$HERE/run.sh" nowall_720p 1 >/dev/null 2>&1 || true

echo "== no-wall profile, ten runs =="
for r in $(seq 10); do
  "$HERE/run.sh" nowall_720p 10 > "$OUT/truth-nowall_720p-$r.run" 2>&1 || { echo "  $r FAILED"; continue; }
  keep "truth-nowall_720p-$r" nowall_720p
  cp "$OUT/nowall_720p-1.log" "$OUT/truth-nowall_720p-$r-client.log"
  echo "  run $r done"
done

echo "== failure path: a bring-up faster than the ramp =="
for p in wifi_tv_probe_damage wan_wg_12; do
  for r in 1 2 3; do
    PF_RIG_BRINGUP_MS=100 PF_RIG_DECODER_HOLD=1 "$HERE/run.sh" "$p" 60 \
      > "$OUT/cutshort-$p-$r.run" 2>&1 || { echo "  $p $r FAILED"; continue; }
    keep "cutshort-$p-$r" "$p"
    echo "  $p run $r done"
  done
done

echo "== failure path: a ramp that finishes must arm no burst =="
for r in 1 2 3; do
  "$HERE/run.sh" nowall_720p 60 > "$OUT/finished-$r.run" 2>&1 || { echo "  $r FAILED"; continue; }
  keep "finished-$r" nowall_720p
  echo "  run $r done"
done

echo "== sessions: wifi_tv_probe_damage 60 s =="
for r in 1 2 3; do
  PF_RIG_DECODER_HOLD=1 "$HERE/run.sh" wifi_tv_probe_damage 60 \
    > "$OUT/sess-wifi-$r.run" 2>&1 || { echo "  $r FAILED"; continue; }
  keep "sess-wifi-$r" wifi_tv_probe_damage
  echo "  run $r done"
done

echo "== sessions: wan_wg_12 600 s =="
for r in 1 2 3; do
  "$HERE/run.sh" wan_wg_12 600 > "$OUT/sess-wan-$r.run" 2>&1 || { echo "  $r FAILED"; continue; }
  keep "sess-wan-$r" wan_wg_12
  echo "  run $r done"
done
echo "ALL DONE"
