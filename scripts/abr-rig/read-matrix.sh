#!/usr/bin/env bash
# Read what a matrix run left in out/ and print the C6 answer per run, then the
# spread per cell. Separate from matrix.sh so a finished run can be re-read
# without re-running it — twenty minutes of real time per matrix.
#
#   scripts/abr-rig/read-matrix.sh [out-dir]
set -euo pipefail

OUT=${1:-$(cd "$(dirname "$0")" && pwd)/out}

# One run, out of its trajectory: what the burst cost in the window the driver
# discards, what reached the first three judged windows, whether anything cut,
# and whether slow start survived. Slow start's steps are 1.2-1.5x here (the
# climb is bounded by what the window proved, not a literal doubling); the crawl
# that replaces it is +6 %, so 1.12 separates them.
read_run() {
  tr -d '{}"' < "$2" | awk -F'[:,]' -v tag="$1" '
    !/summary/ {
      delete v; for (i = 1; i <= NF; i += 2) v[$i] = $(i+1)
      if (v["discarded"] == "true") { dkf += v["keyframe_asks"]; dlost += v["lost_frames"]; dheld += v["held_ms"] }
      else {
        judged++
        if (judged <= 3) { jkf += v["keyframe_asks"]; jlost += v["lost_frames"]; jheld += v["held_ms"] }
        if (v["request_kbps"] != "null") {
          if (v["request_kbps"] + 0 < v["target_kbps"] + 0 && cut == "")
            cut = v["reason"] "@" int(v["t_ms"] / 1000) "s"
          if (prev > 0) { r = (v["request_kbps"] + 0) / prev; if (r >= 1.12) big++; else if (r > 1.0) small++ }
          prev = v["request_kbps"] + 0
        }
      }
      if (v["target_kbps"] + 0 > peak) peak = v["target_kbps"] + 0
      held += v["held_ms"]
    }
    /summary/ { delete v; for (i = 1; i <= NF; i += 2) v[$i] = $(i+1); to90 = v["to90_s"] }
    END {
      printf "%-16s disc[kf=%-3d lost=%-2d held=%4dms] judged1-3[kf=%-3d lost=%-2d held=%4dms] cut=%-16s steps[fast=%-2d +6%%=%-2d] peak=%-7d to90=%-6s held_all=%dms\n",
        tag, dkf, dlost, dheld, jkf, jlost, jheld, (cut == "" ? "none" : cut), big, small, peak, to90, held
    }'
}

for cell in webos-idr webos-wave default-idr default-wave default-slow; do
  peaks=""
  for f in "$OUT/$cell"-[0-9].jsonl; do
    [ -e "$f" ] || continue
    read_run "$cell/$(basename "$f" .jsonl | sed "s/$cell-//")" "$f"
    peaks="$peaks $(grep -v summary "$f" | tr -d '{}"' | awk -F'[:,]' '{for(i=1;i<=NF;i+=2)v[$i]=$(i+1); if (v["target_kbps"]+0>m) m=v["target_kbps"]+0} END{print m}')"
  done
  if [ -n "$peaks" ]; then
    echo "$peaks" | awk -v c="$cell" '{ lo=$1; hi=$1; s=0
      for (i=1;i<=NF;i++) { if ($i<lo) lo=$i; if ($i>hi) hi=$i; s+=$i }
      printf "%-16s peak spread %d..%d kbps (mean %d over %d runs, %.0f%% band)\n\n", c"/spread", lo, hi, s/NF, NF, (hi-lo)*100/(s/NF) }'
  fi
done
