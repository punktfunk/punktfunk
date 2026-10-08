#!/bin/sh
# Mutation sweep over the Automatic-bitrate controller's constants.
#
# For each constant: change it, run the `abr::` tests, restore. A constant
# whose mutation leaves every test GREEN is a decision nothing holds, so a
# later package could change it silently. Exits non-zero on the first such
# constant; the list at the bottom of the output is what to cover.
#
# The run is the whole module, not only `abr::sim`: a rule the scenario table
# cannot reach on any modelled link still has to be pinned somewhere, and a
# unit test that pins it is coverage.
#
# `ENCODE_RISE_US` and `ENCODE_SEVERE_US` only apply when the session's
# refresh is unknown, and `LOW_RATE_WARN_KBPS` only logs, so neither is in
# the table. `PROVEN_BUCKET_WINDOWS` is in it and expected green: see
# `KNOWN_GREEN`. `PROBE_AFTERMATH_WINDOWS` is mutated to 0 rather than to
# another count: its budget binds only for a client that never gets its
# picture back, and no scenario models one. `SHARD_WIRE_OVERHEAD` is the
# packet header's size and `NO_SHARE_KBPS` a wire value, not decisions, so
# neither is in the table.
set -u
cd "$(dirname "$0")/.." || exit 2

# One constant per concern file. The table below names declarations without
# their visibility, so a constant another module reads still matches.
DIR=crates/punktfunk-core/src/abr
TEST="cargo test -p punktfunk-core --features quic --lib abr::"
BACKUP=$(mktemp -d)
cp "$DIR"/*.rs "$BACKUP/"
restore() { cp "$BACKUP"/*.rs "$DIR/"; }
trap 'restore; rm -rf "$BACKUP"' EXIT INT TERM

# Constants whose mutation the simulator cannot see, with the reason.
# `proven_cur_kbps` is refreshed from the deciding window before the climb
# gate reads it, and the utilisation gate guarantees that refreshed value
# already licenses a step, so the bucket period only changes a step's size
# in a window that is clean enough to climb and delivering less than the
# buckets remember. No scenario produces one.
KNOWN_GREEN="PROVEN_BUCKET_WINDOWS"

fail=0

# name | line as it stands | line as it becomes
MUTATIONS=$(cat <<'EOF'
FLOOR_KBPS|const FLOOR_KBPS: u32 = 2_000;|const FLOOR_KBPS: u32 = 500;
IDLE_WINDOWS_TO_REARM|const IDLE_WINDOWS_TO_REARM: u32 = 4;|const IDLE_WINDOWS_TO_REARM: u32 = 40;
CLEAN_WINDOWS_TO_REARM|const CLEAN_WINDOWS_TO_REARM: u32 = 8;|const CLEAN_WINDOWS_TO_REARM: u32 = 40;
MIN_ACTIVE_FRAMES_TO_CLIMB|const MIN_ACTIVE_FRAMES_TO_CLIMB: u32 = 4;|const MIN_ACTIVE_FRAMES_TO_CLIMB: u32 = 40;
PROVEN_BUCKET_WINDOWS|const PROVEN_BUCKET_WINDOWS: u32 = 40;|const PROVEN_BUCKET_WINDOWS: u32 = 10;
BAD_WINDOWS_TO_DECREASE|const BAD_WINDOWS_TO_DECREASE: u32 = 2;|const BAD_WINDOWS_TO_DECREASE: u32 = 4;
SEVERE_LOSS_PPM|const SEVERE_LOSS_PPM: u32 = 60_000;|const SEVERE_LOSS_PPM: u32 = 200_000;
CLEAN_WINDOWS_TO_INCREASE|const CLEAN_WINDOWS_TO_INCREASE: u32 = 6;|const CLEAN_WINDOWS_TO_INCREASE: u32 = 3;
CHANGE_COOLDOWN|const CHANGE_COOLDOWN: Duration = Duration::from_millis(1500);|const CHANGE_COOLDOWN: Duration = Duration::from_millis(3000);
HEAVY_LOSS_PPM|const HEAVY_LOSS_PPM: u32 = 20_000;|const HEAVY_LOSS_PPM: u32 = 55_000;
PROBE_AFTERMATH_WINDOWS|const PROBE_AFTERMATH_WINDOWS: u32 = 8;|const PROBE_AFTERMATH_WINDOWS: u32 = 0;
BLIP_CLEAN_WINDOWS|const BLIP_CLEAN_WINDOWS: u32 = 8;|const BLIP_CLEAN_WINDOWS: u32 = 40;
RECOVERY_KF_BAD|const RECOVERY_KF_BAD: u32 = 2;|const RECOVERY_KF_BAD: u32 = 7;
RECOVERY_KF_SEVERE|const RECOVERY_KF_SEVERE: u32 = 4;|const RECOVERY_KF_SEVERE: u32 = 9;
OWD_RISE_US|const OWD_RISE_US: i64 = 25_000;|const OWD_RISE_US: i64 = 50_000;
DECODE_RISE_US|const DECODE_RISE_US: i64 = 15_000;|const DECODE_RISE_US: i64 = 60_000;
DECODE_SEVERE_US|const DECODE_SEVERE_US: i64 = 45_000;|const DECODE_SEVERE_US: i64 = 180_000;
DECODE_HOLD_PCT|const DECODE_HOLD_PCT: i64 = 80;|const DECODE_HOLD_PCT: i64 = 99;
DECODE_RETREAT_PCT|const DECODE_RETREAT_PCT: i64 = 90;|const DECODE_RETREAT_PCT: i64 = 45;
ANSWER_PCT|const ANSWER_PCT: i64 = 5;|const ANSWER_PCT: i64 = 50;
RETREAT_DIV|const RETREAT_DIV: u32 = 8;|const RETREAT_DIV: u32 = 3;
VERDICT_WINDOWS|const VERDICT_WINDOWS: u32 = 2;|const VERDICT_WINDOWS: u32 = 4;
PROBE_MAX_AGE|const PROBE_MAX_AGE: u32 = 16;|const PROBE_MAX_AGE: u32 = 2;
DECODE_FULL_RATE_NUM|const DECODE_FULL_RATE_NUM: i64 = 3;|const DECODE_FULL_RATE_NUM: i64 = 6;
UTILIZATION_NUM|const UTILIZATION_NUM: u64 = 3;|const UTILIZATION_NUM: u64 = 6;
PROVEN_HEADROOM_NUM|const PROVEN_HEADROOM_NUM: u32 = 3;|const PROVEN_HEADROOM_NUM: u32 = 6;
CAP_REPROBE_WINDOWS_MIN|const CAP_REPROBE_WINDOWS_MIN: u32 = 16;|const CAP_REPROBE_WINDOWS_MIN: u32 = 4;
CAP_REPROBE_WINDOWS_MAX|const CAP_REPROBE_WINDOWS_MAX: u32 = 128;|const CAP_REPROBE_WINDOWS_MAX: u32 = 32;
DECODE_CAP_SIMILAR_DIV|const DECODE_CAP_SIMILAR_DIV: u32 = 8;|const DECODE_CAP_SIMILAR_DIV: u32 = 16;
STARVED_DELIVERY_DIV|const STARVED_DELIVERY_DIV: u32 = 4;|const STARVED_DELIVERY_DIV: u32 = 40;
BASELINE_WINDOWS|const BASELINE_WINDOWS: usize = 40;|const BASELINE_WINDOWS: usize = 10;
BASELINE_MIN_WINDOWS|const BASELINE_MIN_WINDOWS: usize = 4;|const BASELINE_MIN_WINDOWS: usize = 12;
MAX_UNACKED|const MAX_UNACKED: u32 = 3;|const MAX_UNACKED: u32 = 6;
RAMP_STEP_MS|const RAMP_STEP_MS: u32 = 25;|const RAMP_STEP_MS: u32 = 200;
RAMP_START_KBPS|const RAMP_START_KBPS: u32 = 5_000;|const RAMP_START_KBPS: u32 = 40_000;
RAMP_STEP_BYTES|const RAMP_STEP_BYTES: u64 = 16_000_000;|const RAMP_STEP_BYTES: u64 = 300_000;
RAMP_WALL_PCT|const RAMP_WALL_PCT: u64 = 90;|const RAMP_WALL_PCT: u64 = 50;
RAMP_LOSS_SLACK_PCT|const RAMP_LOSS_SLACK_PCT: u64 = 5;|const RAMP_LOSS_SLACK_PCT: u64 = 50;
RAMP_CEILING_PCT|const RAMP_CEILING_PCT: u32 = 70;|const RAMP_CEILING_PCT: u32 = 95;
RAMP_START_PCT|const RAMP_START_PCT: u32 = 50;|const RAMP_START_PCT: u32 = 25;
MODE_RATE_DIV|const MODE_RATE_DIV: u32 = 5;|const MODE_RATE_DIV: u32 = 2;
RAMP_DRAIN_MS|const RAMP_DRAIN_MS: u64 = 20;|const RAMP_DRAIN_MS: u64 = 200;
RAMP_STEP_TIMEOUT|const RAMP_STEP_TIMEOUT: Duration = Duration::from_millis(1_500);|const RAMP_STEP_TIMEOUT: Duration = Duration::from_millis(100);
LINK_CUT_PCT|const LINK_CUT_PCT: u32 = 85;|const LINK_CUT_PCT: u32 = 40;
LINK_CUT_FLOOR_PCT|const LINK_CUT_FLOOR_PCT: u32 = 50;|const LINK_CUT_FLOOR_PCT: u32 = 10;
LINK_DRAIN_WINDOWS|const LINK_DRAIN_WINDOWS: u32 = 6;|const LINK_DRAIN_WINDOWS: u32 = 0;
DRAIN_FALL_US|const DRAIN_FALL_US: i64 = 5_000;|const DRAIN_FALL_US: i64 = 500_000;
LINK_HOLD_DIV|const LINK_HOLD_DIV: u32 = 10;|const LINK_HOLD_DIV: u32 = 3;
DELIVERY_REF_WINDOWS|const DELIVERY_REF_WINDOWS: u32 = 4;|const DELIVERY_REF_WINDOWS: u32 = 40;
DELIVERY_SHORT_PCT|const DELIVERY_SHORT_PCT: u32 = 90;|const DELIVERY_SHORT_PCT: u32 = 40;
LIFT_BAR_US|const LIFT_BAR_US: i64 = 15_000;|const LIFT_BAR_US: i64 = 200_000;
LIFT_BAR_DIV|const LIFT_BAR_DIV: i64 = 4;|const LIFT_BAR_DIV: i64 = 16;
LIFT_OVER_WINDOWS|const LIFT_OVER_WINDOWS: u32 = 2;|const LIFT_OVER_WINDOWS: u32 = 8;
LIFT_RISING_WINDOWS|const LIFT_RISING_WINDOWS: u32 = 2;|const LIFT_RISING_WINDOWS: u32 = 8;
LIFT_PROBE_WINDOWS|const LIFT_PROBE_WINDOWS: u32 = 8;|const LIFT_PROBE_WINDOWS: u32 = 40;
LIFT_PROBE_MAX_AGE|const LIFT_PROBE_MAX_AGE: u32 = 16;|const LIFT_PROBE_MAX_AGE: u32 = 2;
LINK_MARK_SIMILAR_DIV|const LINK_MARK_SIMILAR_DIV: u32 = 5;|const LINK_MARK_SIMILAR_DIV: u32 = 25;
SHARE_BAND_DIV|const SHARE_BAND_DIV: u32 = 10;|const SHARE_BAND_DIV: u32 = 4;
SHORT_DIV|const SHORT_DIV: u32 = 16;|const SHORT_DIV: u32 = 4;
SHARE_CLOCK|const SHARE_CLOCK: Duration = Duration::from_secs(5);|const SHARE_CLOCK: Duration = Duration::from_secs(60);
SHARE_LIFT_CLOCK|const SHARE_LIFT_CLOCK: Duration = Duration::from_secs(60);|const SHARE_LIFT_CLOCK: Duration = Duration::from_secs(5);
LOSS_HORIZON_WINDOWS|const LOSS_HORIZON_WINDOWS: u32 = 32;|const LOSS_HORIZON_WINDOWS: u32 = 4;
LOSS_BUDGET_SECS|const LOSS_BUDGET_SECS: u64 = 600;|const LOSS_BUDGET_SECS: u64 = 60;
LOSS_RELEASE_PCT|const LOSS_RELEASE_PCT: u32 = 125;|const LOSS_RELEASE_PCT: u32 = 100;
LOSS_PCT_MAX|const LOSS_PCT_MAX: u8 = 25;|const LOSS_PCT_MAX: u8 = 50;
MIN_BITRATE_KBPS|const MIN_BITRATE_KBPS: u32 = 500;|const MIN_BITRATE_KBPS: u32 = 5_000;
FEC_MIN|const FEC_MIN: u8 = 5;|const FEC_MIN: u8 = 20;
FEC_MAX|const FEC_MAX: u8 = 50;|const FEC_MAX: u8 = 15;
FEC_ADAPTIVE_START|const FEC_ADAPTIVE_START: u8 = 10;|const FEC_ADAPTIVE_START: u8 = 40;
FEC_STEP|const FEC_STEP: u8 = 3;|const FEC_STEP: u8 = 20;
FEC_STEP_WINDOWS|const FEC_STEP_WINDOWS: u32 = 4;|const FEC_STEP_WINDOWS: u32 = 40;
PROBE_MS|const PROBE_MS: u32 = 800;|const PROBE_MS: u32 = 100;
PROBE_DELAY|const PROBE_DELAY: Duration = Duration::from_secs(2);|const PROBE_DELAY: Duration = Duration::from_secs(30);
PROBE_TIMEOUT|const PROBE_TIMEOUT: Duration = Duration::from_secs(15);|const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
STILL_FRAMES_DIV|const STILL_FRAMES_DIV: u64 = 4;|const STILL_FRAMES_DIV: u64 = 40;
ACK_GIVE_UP|const ACK_GIVE_UP: Duration = Duration::from_secs(10);|const ACK_GIVE_UP: Duration = Duration::from_secs(300);
HEAD_MIN|const HEAD_MIN: u32 = 4;|const HEAD_MIN: u32 = 40;
HEAD_RATIO|const HEAD_RATIO: u32 = 3;|const HEAD_RATIO: u32 = 10;
WAKE_WINDOWS|const WAKE_WINDOWS: u8 = 2;|const WAKE_WINDOWS: u8 = 5;
WAKE_OFF_SECS|const WAKE_OFF_SECS: u64 = 60;|const WAKE_OFF_SECS: u64 = 5;
TAIL_MIN|const TAIL_MIN: u32 = 4;|const TAIL_MIN: u32 = 2;
TAIL_RATIO|const TAIL_RATIO: u32 = 3;|const TAIL_RATIO: u32 = 1;
LINK_FLOOR_KBPS|const LINK_FLOOR_KBPS: u32 = 800_000;|const LINK_FLOOR_KBPS: u32 = 500_000;
NOTCH_DIV|const NOTCH_DIV: u32 = 8;|const NOTCH_DIV: u32 = 3;
NOTCH_MAX|const NOTCH_MAX: u8 = 8;|const NOTCH_MAX: u8 = 2;
NOTCH_CALM_WINDOWS|const NOTCH_CALM_WINDOWS: u32 = 40;|const NOTCH_CALM_WINDOWS: u32 = 4;
LONE_DEAD_PER_MIN_PCT|const LONE_DEAD_PER_MIN_PCT: u64 = 1;|const LONE_DEAD_PER_MIN_PCT: u64 = 2;
EOF
)

printf '%s\n' "$MUTATIONS" | while IFS='|' read -r name from to; do
    [ -n "$name" ] || continue
    restore
    # The declaration is a substring of its line (the visibility prefix is
    # not in the table), and carries no sed metacharacter.
    file=$(grep -lF "$from" "$DIR"/*.rs | head -1)
    [ -n "$file" ] && sed "s|$from|$to|" "$BACKUP/$(basename "$file")" > "$file"
    if [ -z "$file" ] || ! grep -qF "$to" "$file"; then
        printf '::error::%-31s the mutation did not apply — the constant moved\n' "$name"
        echo "APPLY-FAILED $name" >> "$BACKUP.green"
        continue
    fi
    out=$($TEST 2>&1)
    failed=$(printf '%s\n' "$out" | sed -n 's/^test \(abr::[^ ]*\) \.\.\. FAILED$/\1/p' | tr '\n' ' ')
    if [ -n "$failed" ]; then
        printf 'RED   %-31s %s\n' "$name" "$failed"
    elif echo " $KNOWN_GREEN " | grep -q " $name "; then
        printf 'GREEN %-31s nothing noticed (known, see KNOWN_GREEN)\n' "$name"
    else
        printf 'GREEN %-31s nothing noticed\n' "$name"
        echo "$name" >> "$BACKUP.green"
    fi
done

restore
if [ -s "$BACKUP.green" ]; then
    echo
    echo "::error::these constants are invisible to the abr tests:"
    sed 's/^/  /' "$BACKUP.green"
    rm -f "$BACKUP.green"
    fail=1
fi
rm -f "$BACKUP.green"
exit $fail
