#!/usr/bin/env bash
# The rig, inside a privileged Linux container: two network namespaces joined by
# a shaped veth pair, the real host in one, the real client pump in the other.
#
#   rig.sh <profile> [seconds]
#
# Everything it needs is in the container; run.sh is the wrapper that starts it.
set -euo pipefail

PROFILE=${1:?usage: rig.sh <profile> [seconds]}
SECONDS_RUN=${2:-60}
OUT=${PF_RIG_OUT:-/out}
HERE=$(dirname "$0")
# shellcheck source=profiles.sh
. "$HERE/profiles.sh"
profile "$PROFILE"

export CARGO_TARGET_DIR=/target
export CARGO_HOME=${CARGO_HOME:-/cargohome}
BIN=$CARGO_TARGET_DIR/release
HOST_IP=10.77.0.1
CLIENT_IP=10.77.0.2
PORT=9777
# A fresh PIN per run: nothing to check in, and the ceremony is the real one.
PIN=$(shuf -i 1000-9999 -n 1)

say() { echo "[rig] $*"; }

cleanup() {
  set +e
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "${WANDER_PID:-}" ] && kill "$WANDER_PID" 2>/dev/null
  ip netns pids h 2>/dev/null | xargs -r kill 2>/dev/null
  ip netns pids c 2>/dev/null | xargs -r kill 2>/dev/null
  ip netns del h 2>/dev/null
  ip netns del c 2>/dev/null
}
trap cleanup EXIT

# ---- build ------------------------------------------------------------------
say "building"
cd /w
cargo build --release -p punktfunk-host --bin punktfunk-host
cargo build --release -p punktfunk-probe --bin punktfunk-probe

# ---- the shaped link --------------------------------------------------------
# A veth pair is lossless and unqueued; netem with an explicit rate is what
# makes it a link. `limit` is packets, so the queue in bytes (rate × buffer_ms)
# is divided by the MTU, plus what the delay holds in flight.
shape() {
  local ns=$1 dev=$2 kbit=$3 verb=${4:-add}
  local queue_pkts=$(( kbit * BUFFER_MS / 8 / 1500 + kbit * DELAY_MS / 8 / 1500 + 16 ))
  ip netns exec "$ns" tc qdisc "$verb" dev "$dev" root netem \
    rate "${kbit}kbit" delay "${DELAY_MS}ms" limit "$queue_pkts" loss "${LOSS_PCT}%"
}

say "namespaces + veth"
cleanup
ip netns add h
ip netns add c
ip link add vh type veth peer name vc
ip link set vh netns h
ip link set vc netns c
ip -n h addr add $HOST_IP/24 dev vh
ip -n c addr add $CLIENT_IP/24 dev vc
ip -n h link set vh up; ip -n h link set lo up
ip -n c link set vc up; ip -n c link set lo up

say "shaping ${RATE_KBIT}kbit, ${DELAY_MS}ms, ${BUFFER_MS}ms queue, ${LOSS_PCT}% loss"
shape h vh "$RATE_KBIT"
shape c vc "$RATE_KBIT"

# ---- prove the link before streaming over it --------------------------------
# A profile that did not take is hours of misread trajectory. Two commands.
say "checking the shaped link"
ip netns exec c ping -c 3 -i 0.2 -q $HOST_IP | tail -2
ip netns exec h iperf3 -s -1 -B $HOST_IP >/dev/null 2>&1 &
sleep 0.5
ip netns exec c iperf3 -c $HOST_IP -u -b "${RATE_KBIT}k" -t 3 -f k 2>&1 | tail -4
wait %2 2>/dev/null || true

# ---- the session ------------------------------------------------------------
say "host: --source synthetic-abr --content $CONTENT --fill $FILL --recovery-ms $RECOVERY_MS"
ip netns exec h "$BIN/punktfunk-host" punktfunk1-host \
  --port $PORT --source synthetic-abr --content "$CONTENT" --fill "$FILL" \
  --recovery-ms "$RECOVERY_MS" \
  --seconds $(( SECONDS_RUN + 30 )) --pairing-pin "$PIN" --no-mdns \
  > "$OUT/$PROFILE-host.log" 2>&1 &
HOST_PID=$!
for _ in $(seq 40); do
  ip netns exec c bash -c "</dev/tcp/$HOST_IP/$PORT" 2>/dev/null && break
  sleep 0.25
done

say "pairing"
FP=$(echo "$PIN" | ip netns exec c "$BIN/punktfunk-probe" \
      --connect $HOST_IP:$PORT --pair - --name abr-rig 2>&1 \
      | grep -o 'connect with --pin [0-9a-f]\{64\}' | awk '{print $4}')
[ -n "$FP" ] || { echo "[rig] pairing did not return a fingerprint"; tail -20 "$OUT/$PROFILE-host.log"; exit 1; }

# The rate trace and the wander both run beside the session; a profile with
# neither leaves the shaper alone.
if [ -n "$TRACE" ] || [ "$WANDER_PCT" != 0 ]; then
  (
    at=0
    base=$RATE_KBIT
    live=$RATE_KBIT
    while [ $at -lt "$SECONDS_RUN" ]; do
      sleep 1; at=$(( at + 1 ))
      want=$live
      for t in $TRACE; do
        if [ "$at" = "${t%%:*}" ]; then base=${t##*:}; want=$base; fi
      done
      # A drawn rate holds until the next draw — capacity wanders over minutes,
      # it does not step back a second later.
      if [ "$WANDER_PCT" != 0 ] && [ $(( at % WANDER_S )) = 0 ]; then
        want=$(( base * (100 - WANDER_PCT + RANDOM % (2 * WANDER_PCT + 1)) / 100 ))
      fi
      if [ "$want" != "$live" ]; then
        live=$want
        echo "[rig] capacity now ${live}kbit at ${at}s"
        shape h vh "$live" change 2>/dev/null || true
        shape c vc "$live" change 2>/dev/null || true
      fi
    done
  ) &
  WANDER_PID=$!
fi

say "streaming $SECONDS_RUN s on $PROFILE"
run_probe() {
  local n=$1
  ip netns exec c "$BIN/punktfunk-probe" \
    --connect $HOST_IP:$PORT --pin "$FP" --name "abr-rig-$n" \
    --mode "$MODE" --seconds "$SECONDS_RUN" \
    --trajectory "$OUT/$PROFILE-$n.jsonl" \
    --link "$ACHIEVABLE_KBPS:$RATE_KBIT" --profile "$PROFILE" \
    > "$OUT/$PROFILE-$n.log" 2>&1
}
for n in $(seq "$PROBES"); do
  run_probe "$n" &
done
wait $(jobs -p | grep -v "${HOST_PID}" | grep -v "${WANDER_PID:-x}") 2>/dev/null || true

# ---- the summary, beside the simulator's row --------------------------------
echo
echo "== $PROFILE: ${SECONDS_RUN}s on a ${RATE_KBIT}kbit/${DELAY_MS}ms/${BUFFER_MS}ms path =="
head -1 /w/crates/punktfunk-core/src/abr/sim/baseline.tsv
grep -h "^$PROFILE	" /w/crates/punktfunk-core/src/abr/sim/baseline.tsv \
  | sed 's/^/sim  /' || true
for n in $(seq "$PROBES"); do
  tail -1 "$OUT/$PROFILE-$n.jsonl" | tr -d '{}"' | awk -F'[:,]' -v p="$PROFILE" '
    { for (i = 1; i <= NF; i += 2) v[$i] = $(i+1) }
    END { printf "rig  %s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", p,
          v["under5_pct"], v["to90_s"], v["cuts_per_10min"], v["lost_per_10min"],
          v["queue_p95_ms"], v["over_cap_kb_10s"], v["blip_recover_s"],
          v["fairness_x1000"], v["decisions_fnv1a"] }'
done
echo "trajectories: $OUT/$PROFILE-*.jsonl   host log: $OUT/$PROFILE-host.log"
