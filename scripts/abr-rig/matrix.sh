#!/usr/bin/env bash
# The C6 question, run as a matrix: does the startup capacity burst damage a
# real stream the way the simulator's `wifi_tv_probe_damage` says?
#
#   scripts/abr-rig/matrix.sh [seconds] [repeats]
#
# Four cells — burst target {the webOS client's 320 Mbps, the 2 Gbps default} ×
# keyframe answer {an IDR 700 ms later, every fourth ask 300 ms later} — plus
# one that holds the picture far longer than the field did, to find the freeze
# length at which the aftermath reaches a judged window at all. Each runs
# `repeats` times: real time is noisy and one run of anything here is an
# anecdote.
#
# `read-matrix.sh` prints the table, from the `out/<tag>-<n>.jsonl` each run
# leaves behind — so a finished matrix can be re-read without re-running it.
set -euo pipefail

SECONDS_RUN=${1:-60}
REPEATS=${2:-3}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out
PROFILE=wifi_tv_probe_damage

cell() {
  local tag=$1 kbps=$2 answer=$3 recovery=$4
  for r in $(seq "$REPEATS"); do
    PUNKTFUNK_ABR_PROBE_KBPS=$kbps PF_RIG_KEYFRAME_ANSWER=$answer \
      PF_RIG_RECOVERY_MS=$recovery PF_RIG_SKIP_BUILD=${PF_RIG_SKIP_BUILD:-1} \
      "$HERE/run.sh" "$PROFILE" "$SECONDS_RUN" > "$OUT/$tag-$r.run" 2>&1 ||
      { echo "$tag run $r FAILED — see $OUT/$tag-$r.run"; continue; }
    cp "$OUT/$PROFILE-1.jsonl" "$OUT/$tag-$r.jsonl"
    cp "$OUT/$PROFILE-host.log" "$OUT/$tag-$r-host.log"
    echo "  $tag run $r done"
  done
}

# Build once, up front; every run then measures the same binaries.
PF_RIG_SKIP_BUILD=0 "$HERE/run.sh" "$PROFILE" 1 >/dev/null 2>&1 || true

cell webos-idr 320000 idr 700
cell webos-wave 320000 wave:4 300
cell default-idr 2000000 idr 700
cell default-wave 2000000 wave:4 300
# Sensitivity, not a field shape: how long must the freeze be before its asks
# reach a judged window at all? Sixteen asks at one per 100 ms plus the answer
# is ~1.6 s, against a ~700 ms freeze in the cells above.
cell default-slow 2000000 wave:16 300

"$HERE/read-matrix.sh" "$OUT"

echo
echo "baseline row for the same scenario:"
head -1 "$HERE/../../crates/punktfunk-core/src/abr/sim/baseline.tsv"
grep -h "^$PROFILE	" "$HERE/../../crates/punktfunk-core/src/abr/sim/baseline.tsv" | sed 's/^/sim  /'
