#!/usr/bin/env bash
# Link profiles, named after the simulator's scenarios and carrying its numbers
# (crates/punktfunk-core/src/abr/sim/scenarios.rs). Sourced by rig.sh.
#
# Unmeasured in both tiers, and worth knowing before you read a row: a keyframe
# here is IDR_PCT (10x) of an ordinary frame and the simulator's is 4x. Nobody
# has measured a real encoder's; under roughly one-frame VBV it may be closer to
# 1-2x, which would make both tiers over-price every recovery.
#
# RATE_KBIT/DELAY_MS/BUFFER_MS/LOSS_PCT shape the veth pair in both directions.
# TRACE is "<at_s>:<kbit> …" applied while the run goes on; WANDER_PCT/WANDER_S
# re-draw the rate around it. MODE and ACHIEVABLE_KBPS are what the client asks
# for and what the summary scores against.

profile() {
  # Defaults; a profile overrides what it cares about.
  RATE_KBIT=1000000
  DELAY_MS=1
  BUFFER_MS=20
  LOSS_PCT=0
  TRACE=""
  WANDER_PCT=0
  WANDER_S=0
  MODE=1920x1080x60
  ACHIEVABLE_KBPS=0
  CONTENT=steady
  FILL=100
  PROBES=1
  # A host that answers a keyframe ask on the next frame, with an IDR. The
  # simulator's C6/C7 scenarios hinge on a slower, partial answer: `host173`
  # 09-17 09:53 logged keyframe_req=9 idr=2 rfi=8 in one minute.
  RECOVERY_MS=${PF_RIG_RECOVERY_MS:-0}
  KEYFRAME_ANSWER=${PF_RIG_KEYFRAME_ANSWER:-idr}
  # Hold the picture after a lost frame until one re-anchors it, as a real
  # decoder must. Off by default: it is a model, not a decoder.
  DECODER_HOLD=${PF_RIG_DECODER_HOLD:-0}

  case "$1" in
    lan_1g)
      RATE_KBIT=1000000; DELAY_MS=1; BUFFER_MS=20
      MODE=3840x2160x120; ACHIEVABLE_KBPS=150000
      ;;
    # C1: the G5's clean start. 60 ms is what a switch holds.
    wifi_tv)
      RATE_KBIT=245000; DELAY_MS=3; BUFFER_MS=60
      MODE=3840x2160x165; ACHIEVABLE_KBPS=171294
      FILL=78
      ;;
    # C6: the same link behind a consumer AP's 250 ms aggregation queue — where
    # the startup burst does its damage. Holds the picture, because the damage
    # C6 describes is a decoder freeze, not a wire condition.
    wifi_tv_probe_damage)
      RATE_KBIT=245000; DELAY_MS=3; BUFFER_MS=250
      MODE=3840x2160x165; ACHIEVABLE_KBPS=168000
      FILL=78
      DECODER_HOLD=${PF_RIG_DECODER_HOLD:-1}
      ;;
    # C5, Klos54's tunnel: 12.5 Mbps behind a bloated queue, 0.7 % loss.
    wan_wg_12)
      RATE_KBIT=12500; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.7
      WANDER_PCT=30; WANDER_S=180
      MODE=1920x1080x30; ACHIEVABLE_KBPS=12000
      ;;
    lte_variable)
      RATE_KBIT=30000; DELAY_MS=30; BUFFER_MS=250; LOSS_PCT=0.3
      TRACE="40:8000 75:50000 110:2500 140:18000"
      WANDER_PCT=20; WANDER_S=20
      MODE=1920x1080x30; ACHIEVABLE_KBPS=18000
      ;;
    # F: two Automatic sessions over one tunnel, one host.
    shared_two_auto)
      RATE_KBIT=12500; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.7
      MODE=1920x1080x30; ACHIEVABLE_KBPS=12000
      PROBES=2
      ;;
    *)
      echo "unknown profile '$1' (lan_1g wifi_tv wifi_tv_probe_damage wan_wg_12 \
lte_variable shared_two_auto)" >&2
      return 1
      ;;
  esac
}
