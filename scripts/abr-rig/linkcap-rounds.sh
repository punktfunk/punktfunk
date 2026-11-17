#!/usr/bin/env bash
# The link cap on a real stream: every link-cap row, in one unattended pass.
#
#   scripts/abr-rig/linkcap-rounds.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out

keep() { # <tag> <profile>
  cp "$OUT/$2-1.jsonl" "$OUT/$1.jsonl" 2>/dev/null || true
  cp "$OUT/$2-1.log"   "$OUT/$1-client.log" 2>/dev/null || true
  cp "$OUT/$2-host.log" "$OUT/$1-host.log" 2>/dev/null || true
}

"$HERE/run.sh" nowall_720p 1 >/dev/null 2>&1 || true
# The drain guard's per-window line is debug; without this the reader can count
# guard endings but not the windows each one covered.
export RUST_LOG=${RUST_LOG:-info,punktfunk_core::abr=debug}

echo "== 5: nowall_720p 120 s, the control =="
"$HERE/run.sh" nowall_720p 120 > "$OUT/lc-nowall-1.run" 2>&1 && keep lc-nowall-1 nowall_720p
echo "  done"

# Three runs, not two: a split between two has no majority. PUNKTFUNK_PERF puts the
# host's send-loop percentiles in its log, which is how a standing delay at a low
# rate is read.
echo "== 4: wifi_tv_probe_damage 300 s x3, decoder hold, host perf =="
for r in 1 2 3; do
  PF_RIG_DECODER_HOLD=1 PUNKTFUNK_PERF=1 "$HERE/run.sh" wifi_tv_probe_damage 300 \
    > "$OUT/lc-wifi-$r.run" 2>&1 && keep "lc-wifi-$r" wifi_tv_probe_damage
  echo "  run $r done"
done

echo "== 3: lte_variable 600 s x2 =="
for r in 1 2; do
  "$HERE/run.sh" lte_variable 600 > "$OUT/lc-lte-$r.run" 2>&1 && keep "lc-lte-$r" lte_variable
  echo "  run $r done"
done

echo "== 2: wan_wg_12 600 s, no ramp =="
PF_RIG_NO_RAMP=1 "$HERE/run.sh" wan_wg_12 600 > "$OUT/lc-wan-noramp.run" 2>&1 \
  && keep lc-wan-noramp wan_wg_12
echo "  done"

echo "== 1: wan_wg_12 600 s x3 =="
for r in 1 2 3; do
  "$HERE/run.sh" wan_wg_12 600 > "$OUT/lc-wan-$r.run" 2>&1 && keep "lc-wan-$r" wan_wg_12
  echo "  run $r done"
done
echo "ALL DONE"
