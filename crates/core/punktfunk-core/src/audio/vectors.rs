//! `clients/shared/jitter-vectors.json`: scripted runs of [`JitterPolicy`] (CoreAudio tuning),
//! [`AvSync`] and [`DroughtConceal`] with what each one answered. The Apple client keeps Swift
//! twins of all three and replays this file against them.
//!
//! The scripts live here; the file is their output. After a deliberate policy change, rewrite it
//! with `cargo test -p punktfunk-core --lib write_jitter_vectors -- --ignored` and review the diff.

use super::*;
use serde_json::{json, Value};

const FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../clients/shared/jitter-vectors.json"
);

/// One scripted policy input.
enum Op {
    /// `times` callbacks of `step(depth, want)` then `note_read(short)`.
    Run {
        depth: usize,
        want: usize,
        short: bool,
        times: u32,
    },
    Sync(Option<usize>),
    FrameUs(u32),
}

fn run(depth: usize, want: usize, short: bool, times: u32) -> Op {
    Op::Run {
        depth,
        want,
        short,
        times,
    }
}

fn jitter_case(
    name: &str,
    rate_hz: u32,
    channels: u8,
    ops: impl FnOnce(usize) -> Vec<Op>,
) -> Value {
    let mut p = JitterPolicy::new_at_rate(JitterTuning::COREAUDIO, channels, rate_hz);
    // Samples per ms, exact on every rung these cases use.
    let ms = rate_hz as usize * channels as usize / 1000;
    let mut script = Vec::new();
    for op in ops(ms) {
        match op {
            Op::Sync(s) => {
                p.set_sync_target(s);
                script.push(json!({ "sync": s }));
            }
            Op::FrameUs(us) => {
                p.set_frame_us(us);
                script.push(json!({ "frame_us": us }));
            }
            Op::Run {
                depth,
                want,
                short,
                times,
            } => {
                let (mut drop, mut insert, mut crossfade, mut trims, mut silent) = (0, 0, 0, 0, 0);
                for _ in 0..times {
                    let s = p.step(depth, want);
                    drop += s.drop_front;
                    insert += s.insert_front;
                    crossfade += s.crossfade;
                    trims += u32::from(s.hard_trim);
                    silent += u32::from(s.silence);
                    p.note_read(short);
                }
                script.push(json!({
                    "run": { "depth": depth, "want": want, "short": short, "times": times },
                    "expect": {
                        "drop": drop,
                        "insert": insert,
                        "crossfade": crossfade,
                        "trims": trims,
                        "silent": silent,
                        "target_ms": p.target_ms(),
                        "avg_depth_ms": p.avg_depth_ms(),
                        "primed": p.is_primed(),
                    },
                }));
            }
        }
    }
    json!({ "name": name, "rate_hz": rate_hz, "channels": channels, "script": script })
}

fn jitter_cases() -> Vec<Value> {
    vec![
        jitter_case("primes_at_the_target_then_plays", 48_000, 2, |ms| {
            vec![
                run(0, 10 * ms, true, 5),
                run(19 * ms, 10 * ms, false, 3),
                run(20 * ms, 10 * ms, false, 1),
                run(25 * ms, 10 * ms, false, 200),
            ]
        }),
        jitter_case("a_large_quantum_lifts_the_priming_floor", 48_000, 2, |ms| {
            vec![
                run(44 * ms, 40 * ms, false, 2),
                run(45 * ms, 40 * ms, false, 20),
            ]
        }),
        jitter_case("one_late_packet_does_not_deprime", 48_000, 2, |ms| {
            vec![
                run(30 * ms, 5 * ms, false, 20),
                run(2 * ms, 5 * ms, true, 1),
                run(30 * ms, 5 * ms, false, 10),
            ]
        }),
        jitter_case(
            "starvation_grows_the_target_then_deprimes",
            48_000,
            2,
            |ms| {
                vec![
                    run(30 * ms, 5 * ms, false, 20),
                    run(0, 5 * ms, true, 11),
                    run(0, 5 * ms, true, 1),
                    run(10 * ms, 5 * ms, false, 2),
                ]
            },
        ),
        jitter_case("the_hard_cap_trims_on_sight", 48_000, 2, |ms| {
            vec![
                run(30 * ms, 5 * ms, false, 20),
                run(200 * ms, 5 * ms, false, 1),
            ]
        }),
        jitter_case(
            "a_clump_is_kept_until_the_average_follows",
            48_000,
            2,
            |ms| {
                vec![
                    run(25 * ms, 5 * ms, false, 20),
                    run(60 * ms, 5 * ms, false, 300),
                ]
            },
        ),
        jitter_case("a_sustained_excess_sheds_one_frame", 48_000, 2, |ms| {
            vec![
                run(40 * ms, 5 * ms, false, 399),
                run(40 * ms, 5 * ms, false, 1),
                run(40 * ms, 5 * ms, false, 400),
            ]
        }),
        jitter_case(
            "underruns_grow_the_target_and_quiet_relaxes_it",
            48_000,
            2,
            |ms| {
                vec![
                    run(25 * ms, 10 * ms, false, 50),
                    run(5 * ms, 10 * ms, true, 1),
                    run(25 * ms, 10 * ms, false, 50),
                    run(5 * ms, 10 * ms, true, 1),
                    run(25 * ms, 10 * ms, false, 50),
                    run(5 * ms, 10 * ms, true, 1),
                    run(35 * ms, 10 * ms, false, 3000),
                    run(35 * ms, 10 * ms, false, 600),
                ]
            },
        ),
        jitter_case("a_near_miss_grows_once_per_window", 48_000, 2, |ms| {
            vec![
                run(25 * ms, 10 * ms, false, 20),
                run(12 * ms, 10 * ms, false, 1),
                run(12 * ms, 10 * ms, false, 1),
                run(30 * ms, 10 * ms, false, 500),
                run(12 * ms, 10 * ms, false, 1),
            ]
        }),
        jitter_case(
            "a_hollow_ring_reprimes_on_its_first_click",
            48_000,
            2,
            |ms| {
                vec![
                    run(20 * ms, 5 * ms, false, 5),
                    run(6 * ms, 5 * ms, false, 1),
                    run(12 * ms, 5 * ms, false, 400),
                    run(3 * ms, 5 * ms, true, 1),
                    run(12 * ms, 5 * ms, false, 2),
                ]
            },
        ),
        jitter_case("sync_asks_deeper_and_the_ring_inserts", 48_000, 2, |ms| {
            vec![
                run(20 * ms, 5 * ms, false, 5),
                Op::Sync(Some(40 * ms)),
                run(20 * ms, 5 * ms, false, 400),
                run(20 * ms, 5 * ms, false, 10),
                Op::Sync(None),
                run(20 * ms, 5 * ms, false, 10),
            ]
        }),
        jitter_case(
            "sync_asks_shallower_and_a_failed_probe_restores",
            48_000,
            2,
            |ms| {
                vec![
                    run(25 * ms, 5 * ms, false, 5),
                    run(6 * ms, 5 * ms, false, 1),
                    Op::Sync(Some(10 * ms)),
                    run(30 * ms, 5 * ms, false, 1000),
                    run(3 * ms, 5 * ms, true, 1),
                    run(30 * ms, 5 * ms, false, 1000),
                ]
            },
        ),
        jitter_case("a_lossless_frame_sheds_its_own_length", 96_000, 2, |ms| {
            vec![
                Op::FrameUs(2_000),
                run(20 * ms, 2 * ms, false, 5),
                run(40 * ms, 2 * ms, false, 2000),
                run(3 * ms, 2 * ms, false, 1),
            ]
        }),
        jitter_case("the_44k1_rung_sheds_the_wire_frame", 44_100, 2, |_| {
            // 88.2 samples per ms: every depth here is written out in samples.
            vec![
                run(1764, 441, false, 5),
                run(3528, 441, false, 900),
                run(1000, 441, true, 1),
            ]
        }),
        jitter_case("surround_trims_whole_frames", 48_000, 6, |ms| {
            vec![
                run(30 * ms, 5 * ms, false, 5),
                run(200 * ms + 5, 5 * ms, false, 1),
            ]
        }),
        jitter_case(
            "a_quantum_past_the_hard_cap_keeps_the_floor",
            48_000,
            2,
            |ms| {
                vec![
                    Op::Sync(Some(1_000 * ms)),
                    run(210 * ms, 200 * ms, false, 3),
                    Op::Sync(Some(ms)),
                    run(210 * ms, 200 * ms, false, 3),
                ]
            },
        ),
    ]
}

/// One scripted [`AvSync`] input: `times` identical observations, then one `desired_depth`.
struct Observe {
    offset_ms: i64,
    buffered: usize,
    output_latency_ns: u64,
    video: bool,
    times: u32,
    depth: usize,
}

fn av_case(name: &str, rate_hz: u32, channels: u8, runs: Vec<Observe>) -> Value {
    let mut s = AvSync::new_at_rate(channels, rate_hz);
    let mut script = Vec::new();
    // Small epoch values keep every field inside a double's exact integers.
    let (pts_ns, now_ns, clock_offset_ns): (u64, i64, i64) =
        (1_000_000_000, 5_000_000_000, -250_000);
    for r in runs {
        let buffered_ns = samples_to_ms(rate_hz, channels, r.buffered) as i64 * 1_000_000;
        let audio_e2e =
            now_ns + buffered_ns + r.output_latency_ns as i64 + clock_offset_ns - pts_ns as i64;
        let video_e2e_ns = r
            .video
            .then(|| (audio_e2e - r.offset_ms * 1_000_000) as u64);
        let mut last = None;
        for _ in 0..r.times {
            last = s.observe(AvSyncObservation {
                pts_ns,
                now_local_ns: now_ns as i128,
                clock_offset_ns,
                buffered_ahead: r.buffered,
                output_latency_ns: r.output_latency_ns,
                video_e2e_ns,
            });
        }
        let desired = s.desired_depth(r.depth);
        script.push(json!({
            "observe": {
                "pts_ns": pts_ns,
                "now_ns": now_ns,
                "clock_offset_ns": clock_offset_ns,
                "buffered": r.buffered,
                "output_latency_ns": r.output_latency_ns,
                "video_e2e_ns": video_e2e_ns,
                "times": r.times,
            },
            "depth": r.depth,
            "expect": {
                "observed_ns": last,
                "offset_ms": s.offset_ms(),
                "settled": s.settled(),
                "implausible": s.implausible(),
                "desired": desired,
            },
        }));
    }
    json!({ "name": name, "rate_hz": rate_hz, "channels": channels, "script": script })
}

fn observe(offset_ms: i64, times: u32, depth: usize) -> Observe {
    Observe {
        offset_ms,
        buffered: 2_400,
        output_latency_ns: 0,
        video: true,
        times,
        depth,
    }
}

fn av_cases() -> Vec<Value> {
    vec![
        av_case(
            "settles_then_aims_shallower_when_audio_is_late",
            48_000,
            2,
            vec![
                observe(40, 99, 6_000),
                observe(40, 1, 6_000),
                observe(40, 50, 6_000),
            ],
        ),
        av_case(
            "aims_deeper_when_audio_is_early",
            48_000,
            2,
            vec![observe(-30, 120, 2_000)],
        ),
        av_case(
            "the_deadband_holds_the_last_request",
            48_000,
            2,
            vec![
                observe(25, 150, 2_000),
                observe(4, 400, 2_000),
                observe(0, 3_000, 2_000),
            ],
        ),
        av_case(
            "no_picture_no_opinion",
            48_000,
            2,
            vec![Observe {
                video: false,
                ..observe(40, 200, 2_000)
            }],
        ),
        av_case(
            "an_implausible_offset_is_refused",
            48_000,
            2,
            vec![
                observe(20, 150, 2_000),
                observe(5_000, 3, 2_000),
                observe(20, 1, 2_000),
            ],
        ),
        av_case(
            "the_device_behind_the_ring_counts",
            48_000,
            2,
            vec![Observe {
                output_latency_ns: 30_000_000,
                ..observe(45, 150, 4_800)
            }],
        ),
        av_case(
            "the_44k1_rung_steers_in_its_own_samples",
            44_100,
            2,
            vec![Observe {
                buffered: 1_764,
                ..observe(33, 150, 3_528)
            }],
        ),
    ]
}

fn drought_case(name: &str, max_ms: u32, frame_us: u32, ops: Vec<(u32, u32)>) -> Value {
    let mut d = DroughtConceal::new_at_frame_us(max_ms, frame_us);
    let mut script = Vec::new();
    // `(u32::MAX, _)` is a packet; anything else is `conceal(since_ms, depth_ms)`.
    for (since_ms, depth_ms) in ops {
        if since_ms == u32::MAX {
            script.push(json!({ "packet": true, "expect": d.packet() }));
        } else {
            let yes = d.conceal(Duration::from_millis(since_ms as u64), depth_ms);
            script.push(json!({ "since_ms": since_ms, "depth_ms": depth_ms, "expect": yes }));
        }
    }
    json!({
        "name": name,
        "max_ms": max_ms,
        "frame_us": frame_us,
        "script": script,
        "total_ms": d.total_ms(),
    })
}

fn drought_cases() -> Vec<Value> {
    const PACKET: (u32, u32) = (u32::MAX, 0);
    let plc_max = JitterTuning::COREAUDIO.plc_max_ms();
    let mut spent = vec![(20, 0); 30];
    spent.push(PACKET);
    spent.push((20, 0));
    vec![
        drought_case(
            "conceals_only_a_real_drought_on_a_draining_ring",
            plc_max,
            5_000,
            vec![(9, 0), (10, 0), (10, 11), (10, 10), (50, 0), PACKET, (5, 0)],
        ),
        drought_case("the_budget_is_twice_the_fuse", plc_max, 5_000, spent),
        drought_case(
            "a_lossless_frame_moves_both_thresholds",
            plc_max,
            2_000,
            vec![(3, 0), (4, 0), (4, 4), (4, 5), PACKET, (4, 0)],
        ),
        drought_case(
            "a_half_millisecond_rung_rounds_the_floor_up",
            plc_max,
            2_500,
            vec![(4, 0), (5, 0), (5, 5), (5, 6)],
        ),
    ]
}

fn vectors() -> Value {
    json!({
        "$comment": "Written by punktfunk-core's audio::vectors (see its module doc); the Apple client's JitterVectorTests replays it. jitter: each run is `times` callbacks of step(depth, want) then note_read(short), all in interleaved samples, under JitterTuning::COREAUDIO; expect sums the steps and reads the state after. av_sync: `times` identical observations, then one desired_depth(depth). drought: conceal(since_ms, depth_ms) or packet().",
        "version": 1,
        "jitter": jitter_cases(),
        "av_sync": av_cases(),
        "drought": drought_cases(),
    })
}

#[test]
fn the_checked_in_vectors_are_what_the_policy_answers() {
    let raw = include_str!("../../../../../clients/shared/jitter-vectors.json");
    let file: Value = serde_json::from_str(raw).expect("vector file parses");
    assert!(
        file == vectors(),
        "clients/shared/jitter-vectors.json is stale: rewrite it (module doc) and review the diff"
    );
}

#[test]
#[ignore = "rewrites clients/shared/jitter-vectors.json"]
fn write_jitter_vectors() {
    let text = serde_json::to_string_pretty(&vectors()).expect("serialize") + "\n";
    std::fs::write(FILE, text).expect("write the vector file");
}
