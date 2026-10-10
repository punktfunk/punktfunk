#!/usr/bin/env bash
# The rig, inside a privileged Linux container: two network namespaces joined by
# a shaped veth pair, the real host in one, the real client pump in the other.
#
#   rig.sh <profile> [seconds]
#
# Everything it needs is in the container; run.sh is the wrapper that starts it.
#
# netem sits before segmentation and drops a GSO train whole, which no wire does,
# so the host sends one packet per skb here unless PUNKTFUNK_GSO says otherwise.
set -euo pipefail

PROFILE=${1:?usage: rig.sh <profile> [seconds]}
SECONDS_RUN=${2:-60}
OUT=${PF_RIG_OUT:-/out}
HERE=$(dirname "$0")
# shellcheck source=profiles.sh
. "$HERE/profiles.sh"
profile "$PROFILE"

export PUNKTFUNK_GSO=${PUNKTFUNK_GSO:-0}
export CARGO_TARGET_DIR=/target
export CARGO_HOME=${CARGO_HOME:-/cargohome}
BIN=$CARGO_TARGET_DIR/release
HOST_IP=10.77.0.1
CLIENT_IP=10.77.0.2
PORT=9777
# A fresh PIN per run: nothing to check in, and the ceremony is the real one.
PIN=$(shuf -i 1000-9999 -n 1)
# One client identity per probe lives here; see the pairing block.
PROBE_HOME=/tmp/pf-rig-home

say() { echo "[rig] $*"; }

cleanup() {
  set +e
  [ -n "${HOST_PID:-}" ] && kill "$HOST_PID" 2>/dev/null
  [ -n "${WANDER_PID:-}" ] && kill "$WANDER_PID" 2>/dev/null
  ip netns pids h 2>/dev/null | xargs -r kill 2>/dev/null
  ip netns pids c 2>/dev/null | xargs -r kill 2>/dev/null
  ip netns del h 2>/dev/null
  ip netns del c 2>/dev/null
  rm -rf "$PROBE_HOME"
}
trap cleanup EXIT

# ---- build ------------------------------------------------------------------
cd /w
say "building"
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

# A wall that answers with loss instead of delay: netem carries the delay only, and an
# ingress policer drops everything over the rate. `burst` is the single bucket — BUFFER_MS
# of it — so an overshoot costs packets within a few ms, never a queue that grows.
police() {
  local ns=$1 dev=$2 kbit=$3
  local burst=$(( POLICE_BURST_KB * 1024 ))
  if [ "$burst" = 0 ]; then burst=$(( kbit * BUFFER_MS / 8 )); fi
  ip netns exec "$ns" tc qdisc add dev "$dev" root netem \
    delay "${DELAY_MS}ms" limit 10000
  ip netns exec "$ns" tc qdisc add dev "$dev" handle ffff: ingress
  ip netns exec "$ns" tc filter add dev "$dev" parent ffff: protocol ip prio 1 u32 \
    match u32 0 0 police rate "${kbit}kbit" burst "${burst}b" conform-exceed drop
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

if [ "$POLICE_KBIT" != 0 ]; then
  say "policing ${POLICE_KBIT}kbit, ${DELAY_MS}ms, burst ${POLICE_BURST_KB}KiB (0 = ${BUFFER_MS}ms), drop over (no queue)"
  police h vh "$POLICE_KBIT"
  police c vc "$POLICE_KBIT"
else
  say "shaping ${RATE_KBIT}kbit, ${DELAY_MS}ms, ${BUFFER_MS}ms queue, ${LOSS_PCT}% loss"
  shape h vh "$RATE_KBIT"
  shape c vc "$RATE_KBIT"
fi

# ---- prove the link before streaming over it --------------------------------
# A profile that did not take is hours of misread trajectory. Two commands.
say "checking the shaped link"
ip netns exec c ping -c 3 -i 0.2 -q $HOST_IP | tail -2
ip netns exec h iperf3 -s -1 -B $HOST_IP >/dev/null 2>&1 &
sleep 0.5
# A policer is proved by over-offering: the excess must come back as loss, not as delay.
OFFER_KBIT=$RATE_KBIT
if [ "$POLICE_KBIT" != 0 ]; then OFFER_KBIT=$(( POLICE_KBIT * 3 / 2 )); fi
ip netns exec c iperf3 -c $HOST_IP -u -b "${OFFER_KBIT}k" -t 3 -f k 2>&1 | tail -4
wait %2 2>/dev/null || true

# ---- the session ------------------------------------------------------------
NO_RAMP_ARG=""
if [ "$NO_RAMP" = 1 ]; then NO_RAMP_ARG="--no-ramp"; fi
# A PIN pairs one device, so a run with more than one probe trusts on first use instead
# (see the pairing block).
TOFU_ARG=""
if [ "$PROBES" != 1 ]; then TOFU_ARG="--allow-tofu"; fi
say "host: --content $CONTENT --fill $FILL --recovery-ms $RECOVERY_MS --keyframe-answer $KEYFRAME_ANSWER --idr-pct $IDR_PCT --bringup-ms $BRINGUP_MS $NO_RAMP_ARG"
ip netns exec h "$BIN/punktfunk-host" punktfunk1-host \
  --port $PORT --source synthetic-abr --content "$CONTENT" --fill "$FILL" \
  --recovery-ms "$RECOVERY_MS" --keyframe-answer "$KEYFRAME_ANSWER" \
  --idr-pct "$IDR_PCT" \
  --bringup-ms "$BRINGUP_MS" $NO_RAMP_ARG \
  --seconds $(( SECONDS_RUN + 30 )) --pairing-pin "$PIN" --no-mdns $TOFU_ARG \
  > "$OUT/$PROFILE-host.log" 2>&1 &
HOST_PID=$!
for _ in $(seq 40); do
  ip netns exec c bash -c "</dev/tcp/$HOST_IP/$PORT" 2>/dev/null && break
  sleep 0.25
done

# `cleanup` above already removed the last run's; one directory per probe.
for n in $(seq "$PROBES"); do mkdir -p "$PROBE_HOME/$n"; done
if [ "$PROBES" = 1 ]; then
  say "pairing"
  FP=$(echo "$PIN" | HOME="$PROBE_HOME/1" ip netns exec c "$BIN/punktfunk-probe" \
        --connect $HOST_IP:$PORT --pair - --name abr-rig 2>&1 \
        | grep -o 'connect with --pin [0-9a-f]\{64\}' | awk '{print $4}')
  [ -n "$FP" ] || { echo "[rig] pairing did not return a fingerprint"; tail -20 "$OUT/$PROFILE-host.log"; exit 1; }
else
  # Two sessions on a path are two clients. The host preempts a client's own earlier
  # session by certificate fingerprint and the probe keeps its certificate in
  # $HOME/.config/punktfunk, so each probe needs its own HOME — and one PIN pairs one
  # device. `--allow-tofu` admits both on first use; each still pins the host, which
  # prints its fingerprint on the line it starts listening on.
  say "reading the host's fingerprint (--allow-tofu: one PIN cannot pair two clients)"
  FP=""
  for _ in $(seq 60); do
    FP=$(sed 's/\x1b\[[0-9;]*m//g' "$OUT/$PROFILE-host.log" \
          | grep 'clients pin this fingerprint' \
          | grep -o 'fingerprint=[0-9a-f]\{64\}' | head -1 | cut -d= -f2)
    [ -n "$FP" ] && break
    sleep 0.25
  done
  [ -n "$FP" ] || { echo "[rig] the host printed no fingerprint"; tail -20 "$OUT/$PROFILE-host.log"; exit 1; }
fi

# The rate trace and the wander both run beside the session; a profile with
# neither leaves the shaper alone.
if [ -n "$TRACE" ] || [ "$WANDER_PCT" != 0 ]; then
  (
    # A subshell reseeds RANDOM, so the seed is set here: one PF_RIG_SEED, one capacity trace.
    if [ -n "${PF_RIG_SEED:-}" ]; then RANDOM=$PF_RIG_SEED; fi
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

say "streaming $SECONDS_RUN s on $PROFILE (decoder-hold=$DECODER_HOLD)"
HOLD_ARG=""
if [ "$DECODER_HOLD" = 1 ]; then HOLD_ARG="--decoder-hold"; fi
run_probe() {
  local n=$1
  # What only this probe does (profiles.sh): join late, pin a rate, leave early.
  local join secs rate rate_arg
  join=$(echo "$PROBE_JOIN_S" | awk -v n="$n" '{print $n+0}')
  secs=$(echo "$PROBE_SECONDS" | awk -v n="$n" '{print $n+0}')
  rate=$(echo "$PROBE_BITRATE" | awk -v n="$n" '{print $n+0}')
  if [ "$join" != 0 ]; then sleep "$join"; fi
  if [ "$secs" = 0 ] || [ "$secs" -gt $(( SECONDS_RUN - join )) ]; then
    secs=$(( SECONDS_RUN - join ))
  fi
  # A pinned probe's controller never arms, so its trajectory's target column is 0 and
  # what the row is read on is its delivered rate.
  rate_arg=""
  if [ "$rate" != 0 ]; then rate_arg="--bitrate $rate"; fi
  HOME="$PROBE_HOME/$n" ip netns exec c "$BIN/punktfunk-probe" \
    --connect $HOST_IP:$PORT --pin "$FP" --name "abr-rig-$n" \
    --mode "$MODE" --seconds "$secs" $HOLD_ARG $rate_arg \
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
# The host's columns sum its whole-minute `link health` lines for that probe's session.
printf '%s\tunrec_10min\trfi_10min\tidr_10min\tkf_req_10min\n' \
  "$(head -1 /w/crates/core/punktfunk-core/src/abr/sim/baseline.tsv)"
grep -h "^$PROFILE	" /w/crates/core/punktfunk-core/src/abr/sim/baseline.tsv \
  | sed 's/^/sim  /' || true
clean() { sed 's/\x1b\[[0-9;]*m//g' "$@"; }
# Host session ids rise in open order, so the k-th probe to open owns the k-th id.
mapfile -t SIDS < <(clean "$OUT/$PROFILE-host.log" \
  | grep -o 'link health session=[0-9]*' | cut -d= -f2 | sort -nu)
declare -A SID
k=0
for n in $(for n in $(seq "$PROBES"); do
             echo "$(clean "$OUT/$PROFILE-$n.log" | grep -m1 'trajectory session open' | cut -c1-27) $n"
           done | sort | awk '{print $NF}'); do
  SID[$n]=${SIDS[$k]:-none}; k=$(( k + 1 ))
done
for n in $(seq "$PROBES"); do
  host=$(clean "$OUT/$PROFILE-host.log" | awk -v sid="${SID[$n]}" '
    /link health/ && / secs=60 / && index($0, " session=" sid " ") {
      for (i = 1; i <= NF; i++) { split($i, kv, "="); s[kv[1]] += kv[2] } }
    END { print s["unrecovered"] + 0, s["rfi"] + 0, s["idr"] + 0, s["keyframe_req"] + 0 }')
  tail -1 "$OUT/$PROFILE-$n.jsonl" | tr -d '{}"' | awk -F'[:,]' -v p="$PROFILE" -v h="$host" '
    { for (i = 1; i <= NF; i += 2) v[$i] = $(i+1) }
    END { split(h, d, " "); f = 600000 / (v["duration_ms"] > 0 ? v["duration_ms"] : 1)
          printf "rig  %s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%d\t%d\t%d\t%d\n", p,
          v["under5_pct"], v["to90_s"], v["cuts_per_10min"], v["lost_per_10min"],
          v["queue_p95_ms"], v["over_cap_kb_10s"], v["blip_recover_s"],
          v["fairness_x1000"], v["decisions_fnv1a"],
          d[1] * f, d[2] * f, d[3] * f, d[4] * f }'
done
echo "trajectories: $OUT/$PROFILE-*.jsonl   host log: $OUT/$PROFILE-host.log"

# ---- the ramp guard ---------------------------------------------------------
# A ramp step the HOST could not fill reads as a floor under the link, never a wall, and the
# session then spends 600 s below a link it never measured. It is a rig fault whatever the
# profile — `nowall_720p` ends `wall=false` with no such step and passes. Exit 3 so the
# driver repeats the run instead of reporting it.
if grep -qs "ramp step limited by the sender" "$OUT/$PROFILE-1.log"; then
  echo "[rig] FAILED: the bring-up ramp hit a sender-limited step — the wall was never measured"
  grep -h "ramp step limited by the sender\|bring-up ramp done" "$OUT/$PROFILE-1.log" | tail -2
  exit 3
fi
