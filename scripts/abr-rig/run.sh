#!/usr/bin/env bash
# One command for the real-stream ABR rig: a privileged Linux container, two
# network namespaces joined by a `tc netem` path, the real host in one and the
# real client pump in the other.
#
#   scripts/abr-rig/run.sh <profile> [seconds]
#   scripts/abr-rig/run.sh wan_wg_12 120
#
# Profiles are named after the simulator's scenarios (profiles.sh). Trajectories
# and logs land in scripts/abr-rig/out/. `tc` and network namespaces need
# NET_ADMIN, so this is a local/on-demand rig and never a CI gate.
#
# PUNKTFUNK_ABR_PROBE=0 and the other ABR environment knobs pass through, which
# is how the startup-burst control run is taken. PF_RIG_SKIP_BUILD=1 reuses the
# binaries from the last run; PF_RIG_RECOVERY_MS sets how long the host takes to
# answer a keyframe request.
set -euo pipefail

PROFILE=${1:?usage: run.sh <profile> [seconds]}
SECONDS_RUN=${2:-60}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
OUT=$HERE/out
IMAGE=pf-abr-rig
VOLUME=pf-abr-rig-target

mkdir -p "$OUT"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "[run] building the $IMAGE image"
  docker build -t "$IMAGE" -f "$HERE/Dockerfile" "$HERE"
fi

# Passed through so a control run is one environment variable, not a second script.
env_args=(-e PF_RIG_OUT=/out)
for k in PUNKTFUNK_ABR_PROBE PUNKTFUNK_ABR_PROBE_KBPS PUNKTFUNK_ABR_MAX_MBPS \
         PUNKTFUNK_PACE_FACTOR PUNKTFUNK_PACE_BURST_KB PF_RIG_RECOVERY_MS \
         PF_RIG_SKIP_BUILD PF_RIG_KEYFRAME_ANSWER PF_RIG_DECODER_HOLD RUST_LOG; do
  if [ -n "${!k:-}" ]; then env_args+=(-e "$k=${!k}"); fi
done

exec docker run --rm --privileged \
  -v "$REPO":/w \
  -v "$VOLUME":/target \
  -v pf-bookworm-cargo:/cargohome \
  -v "$OUT":/out \
  -e CARGO_HOME=/cargohome \
  "${env_args[@]}" \
  "$IMAGE" bash /w/scripts/abr-rig/rig.sh "$PROFILE" "$SECONDS_RUN"
