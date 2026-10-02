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
  # Non-zero swaps netem's rate for an ingress policer at this rate: the wall answers an
  # overshoot with dropped packets and no queue, which is the one shape netem cannot make.
  POLICE_KBIT=0
  # The policer's token bucket, KiB. 0 = BUFFER_MS of the policed rate. A real policer's
  # burst allowance varies by an order of magnitude, so it is a knob, not a constant.
  POLICE_BURST_KB=${PF_RIG_POLICE_BURST_KB:-0}
  WANDER_PCT=0
  WANDER_S=0
  MODE=1920x1080x60
  ACHIEVABLE_KBPS=0
  CONTENT=steady
  FILL=100
  PROBES=1
  # Per-probe overrides, one word per probe, so a shared-path profile can say what only
  # the second session does. Absent or 0 = the profile's default.
  # JOIN_S: seconds this probe waits before it connects. BITRATE: kbps it pins (0 =
  # Automatic). SECONDS: its own --seconds (0 = to the end of the run).
  PROBE_JOIN_S=""
  PROBE_BITRATE=""
  PROBE_SECONDS=""
  # A host that answers a keyframe ask on the next frame, with an IDR. The
  # simulator's C6/C7 scenarios hinge on a slower, partial answer: `host173`
  # 09-17 09:53 logged keyframe_req=9 idr=2 rfi=8 in one minute.
  RECOVERY_MS=${PF_RIG_RECOVERY_MS:-0}
  KEYFRAME_ANSWER=${PF_RIG_KEYFRAME_ANSWER:-idr}
  # A keyframe's size against an ordinary frame, percent. 1000 is what rounds 4-7 ran;
  # a hardware encoder holds VBV at one frame, so a faithful row is far nearer 100.
  IDR_PCT=${PF_RIG_IDR_PCT:-1000}
  # Hold the picture after a lost frame until one re-anchors it, as a real
  # decoder must. Off by default: it is a model, not a decoder.
  DECODER_HOLD=${PF_RIG_DECODER_HOLD:-0}
  # How long the host holds its first frame back, as a pipeline build does.
  # The client's bring-up ramp runs inside this window.
  BRINGUP_MS=${PF_RIG_BRINGUP_MS:-2500}
  # 1 = do not offer the ramp, so the client takes the legacy in-session burst.
  NO_RAMP=${PF_RIG_NO_RAMP:-0}

  case "$1" in
    lan_1g)
      RATE_KBIT=1000000; DELAY_MS=1; BUFFER_MS=20
      MODE=3840x2160x120; ACHIEVABLE_KBPS=150000
      ;;
    # The 2.5 G host → 1 G client hop: a 1 Gbit/s link whose queue is one switch's SRAM
    # (1 ms ≈ 125 KB). A line-rate burst overruns it on IDRs; `PUNKTFUNK_DELIVERY=capped`
    # does not.
    lan_1g_switch)
      RATE_KBIT=1000000; DELAY_MS=1; BUFFER_MS=1
      MODE=3840x2160x120; ACHIEVABLE_KBPS=150000
      ;;
    # An untuned receiver on 1 Gbit/s: a queue the size of Linux's default socket buffer
    # (2 ms ≈ 250 KB). `PUNKTFUNK_DELIVERY=smooth` keeps a frame under it.
    lan_1g_smallbuf)
      RATE_KBIT=1000000; DELAY_MS=1; BUFFER_MS=2
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
    # The tunnel with a desktop that goes still a minute in: two new frames a second
    # among repeats, 0.5 % loss. No wander, so a cut is the content's doing or the loss's.
    wan_still)
      RATE_KBIT=12500; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=12000
      CONTENT=motion-then-still:2
      ;;
    # A link with room and 0.5 % random loss: a bring-up ramp that walls under 10 Mbps
    # here read one lost packet as the link.
    ramp_loss)
      RATE_KBIT=245000; DELAY_MS=3; BUFFER_MS=60; LOSS_PCT=0.5
      MODE=1920x1080x60; ACHIEVABLE_KBPS=60000
      ;;
    lte_variable)
      RATE_KBIT=30000; DELAY_MS=30; BUFFER_MS=250; LOSS_PCT=0.3
      TRACE="40:8000 75:50000 110:2500 140:18000"
      WANDER_PCT=20; WANDER_S=20
      MODE=1920x1080x30; ACHIEVABLE_KBPS=18000
      ;;
    # The no-wall case: a link netem shapes cleanly (245 Mbps — 1 Gbps it
    # cannot) carrying a mode whose stream cap / 0.7 is far below it, so the
    # bring-up ramp runs out of things to prove before the link refuses
    # anything. `lan_1g` is NOT this case: its cap / 0.7 is above what the VM
    # can shape, and the ramp reads the two hosts' packet paths instead.
    # A wall that drops instead of queueing: 20 Mbit policed, 10 ms, no random loss, no
    # wander. 1080p60's stream cap is ~90 Mbps, so the policer is what the session meets and
    # the ramp has a real wall to find. BUFFER_MS is the policer's single burst, not a queue.
    policer_20)
      RATE_KBIT=20000; POLICE_KBIT=20000; DELAY_MS=10; BUFFER_MS=10; LOSS_PCT=0
      MODE=1920x1080x60; ACHIEVABLE_KBPS=19000
      ;;
    # WP6 (#1279) shared-path rows. The link is the simulator's `shared()` exactly
    # (sim/scenarios.rs): 18 000 kbit, 10 ms, 450 ms queue, 0.5 % loss, 1080p30, two
    # sessions — so a rig row reads against the `shared_*` baseline rows. ACHIEVABLE_KBPS
    # is an equal share on every one of them, lone rows included, so the summary scores
    # each row on the same scale. `shared_two_auto` keeps its own 12 500 kbit: there a
    # share is 6 250 and the "usable = over 5 Mbps" bar sits inside the noise.
    shared_both)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBES=2
      ;;
    # The second session joins a minute in (sim: shared_newcomer).
    shared_newcomer)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBES=2; PROBE_JOIN_S="0 60"
      ;;
    # One session pinned to 8 Mbps beside an Automatic one (sim: shared_fixed_plus_auto).
    # The pinned one records a trajectory whose target column is 0: its controller never
    # arms, and that the rate never moved is the invariant this row tests.
    shared_fixed_plus_auto)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBES=2; PROBE_BITRATE="0 8000"
      ;;
    # The second session leaves halfway; the survivor should take the path.
    shared_leaver)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBES=2; PROBE_SECONDS="0 300"
      ;;
    # A pinned 8 Mbps session outlives an Automatic sibling that leaves halfway. Its
    # target column stays 0 and no Governor ack reaches it, before or after.
    shared_fixed_survivor)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBES=2; PROBE_BITRATE="8000 0"; PROBE_SECONDS="0 300"
      ;;
    # One session on this link: what a lone session holds, for the leaver row.
    shared_lone)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      ;;
    # One pinned session alone on it: what the fixed row is judged against.
    shared_fixed_lone)
      RATE_KBIT=18000; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.5
      MODE=1920x1080x30; ACHIEVABLE_KBPS=9000
      PROBE_BITRATE="8000"
      ;;
    nowall_720p)
      RATE_KBIT=245000; DELAY_MS=3; BUFFER_MS=60
      MODE=1280x720x60; ACHIEVABLE_KBPS=25000
      ;;

    # F: two Automatic sessions over one tunnel, one host. Each probe scores
    # only itself, so both rows read fairness 1000 — the Jain share needs both
    # sessions' windows in one run, which nothing merges yet.
    shared_two_auto)
      RATE_KBIT=12500; DELAY_MS=10; BUFFER_MS=450; LOSS_PCT=0.7
      MODE=1920x1080x30; ACHIEVABLE_KBPS=12000
      PROBES=2
      ;;
    *)
      echo "unknown profile '$1' (lan_1g lan_1g_switch lan_1g_smallbuf wifi_tv wifi_tv_probe_damage wan_wg_12 \
lte_variable wan_still ramp_loss nowall_720p policer_20 shared_two_auto shared_both shared_newcomer \
shared_fixed_plus_auto shared_fixed_survivor shared_leaver shared_lone shared_fixed_lone)" >&2
      return 1
      ;;
  esac
}
