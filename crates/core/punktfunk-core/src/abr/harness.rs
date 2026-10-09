//! Fixtures the controller tests share: a 750 ms clock, and the window runs
//! that put a controller in a known state (climbed, choked, stood down).
//!
//! Every helper drives the real [`BitrateController`] through
//! [`on_window`](BitrateController::on_window), so a test reads as the
//! session it describes rather than as a list of windows.

use super::controller::{BitrateController, DECODE_CAP_SIMILAR_DIV};
use super::sample::{WindowActivity, WindowSample};
use super::verdict::{BASELINE_MIN_WINDOWS, RECOVERY_KF_SEVERE};
use std::time::{Duration, Instant};

/// Pump's 750 ms tick; 5× is past [`CHANGE_COOLDOWN`].
pub(super) const TICK: Duration = Duration::from_millis(750);

pub(super) fn ticks(start: Instant, n: u32) -> Instant {
    start + TICK * n
}

/// `n` clean fully-loaded windows (1 Gb/s) so utilization and proven never bind.
pub(super) fn run_clean(
    c: &mut BitrateController,
    start: Instant,
    from: u32,
    n: u32,
) -> Option<u32> {
    let mut out = None;
    for i in from..from + n {
        out = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            actual_kbps: 1_000_000,
            ..WindowSample::at(ticks(start, i))
        });
        if out.is_some() {
            return out;
        }
    }
    out
}

/// Re-seed the baseline `on_ack` cleared, then present `level`. Four seed
/// windows stay under [`CLEAN_WINDOWS_TO_INCREASE`].
pub(super) fn encode_choke(
    c: &mut BitrateController,
    start: Instant,
    tick: &mut u32,
    level: i64,
) -> Option<u32> {
    for _ in 0..BASELINE_MIN_WINDOWS {
        let at = ticks(start, *tick);
        *tick += 1;
        // Ack a climb if taken so tests with headroom still work.
        if let Some(k) = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            encode_mean_us: Some(7_000),
            actual_kbps: 1_000_000,
            ..WindowSample::at(at)
        }) {
            c.on_ack(k, None);
        }
    }
    let at = ticks(start, *tick);
    *tick += 1;
    c.on_window(&WindowSample {
        owd_mean_us: Some(10_000),
        encode_mean_us: Some(level),
        actual_kbps: 1_000_000,
        ..WindowSample::at(at)
    })
}

/// `n` clean windows with no encode sample; ack any climb.
pub(super) fn clean_run(c: &mut BitrateController, start: Instant, tick: &mut u32, n: u32) {
    for _ in 0..n {
        let at = ticks(start, *tick);
        *tick += 1;
        if let Some(k) = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            actual_kbps: 1_000_000,
            ..WindowSample::at(at)
        }) {
            c.on_ack(k, None);
        }
    }
}

/// One notch at a level the rate does not move, then the windows that tell
/// the driver so: the rate comes back and the driver stands down.
pub(super) fn disarm_encode(c: &mut BitrateController, start: Instant, tick: &mut u32) {
    let notch = encode_choke(c, start, tick, 20_000).expect("an encode rise must cost a notch");
    c.on_ack(notch, None);
    let restore = encode_windows(c, start, tick, 20_000, 8).expect("the notch must be judged");
    assert!(
        restore > notch,
        "the rate the encoder never answered comes back"
    );
    c.on_ack(restore, None);
    assert!(c.encode.down.disarmed());
}

/// Windows carrying `level` until one asks for a rate, at most `n`. Contention
/// that holds its level whatever the rate looks exactly like this.
pub(super) fn encode_windows(
    c: &mut BitrateController,
    start: Instant,
    tick: &mut u32,
    level: i64,
    n: u32,
) -> Option<u32> {
    for _ in 0..n {
        let at = ticks(start, *tick);
        *tick += 1;
        if let Some(k) = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            encode_mean_us: Some(level),
            actual_kbps: 1_000_000,
            ..WindowSample::at(at)
        }) {
            return Some(k);
        }
    }
    None
}

pub(super) fn calm_window(c: &mut BitrateController, at: Instant) {
    // Calm, unutilized: seed baselines, decide nothing.
    assert_eq!(
        c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(8_000),
            actual_kbps: 2_000,
            ..WindowSample::at(at)
        }),
        None
    );
}

/// Climb to `target` on fully-utilized windows, acking each step. 600-window bound.
pub(super) fn climb_to(c: &mut BitrateController, start: Instant, tick: &mut u32, target: u32) {
    for _ in 0..600 {
        if c.current_kbps >= target {
            return;
        }
        if let Some(k) = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(8_000),
            actual_kbps: 1_000_000,
            ..WindowSample::at(ticks(start, *tick))
        }) {
            c.on_ack(k, None);
        }
        *tick += 1;
    }
    panic!(
        "no climb to {target} within 600 windows (stuck at {})",
        c.current_kbps
    );
}

/// One decode-severe window at the current rate. Steps past cooldown first.
pub(super) fn choke(c: &mut BitrateController, start: Instant, tick: &mut u32) -> Option<u32> {
    *tick += 2;
    let r = c.on_window(&WindowSample {
        owd_mean_us: Some(10_000),
        decode_mean_us: Some(60_000),
        actual_kbps: c.current_kbps,
        ..WindowSample::at(ticks(start, *tick))
    });
    *tick += 1;
    r
}

/// Choke, ack the ×0.7, re-climb, choke inside ±1/8. Returns the latched cap.
pub(super) fn latch_knee(c: &mut BitrateController, start: Instant, tick: &mut u32) -> u32 {
    for _ in 0..4 {
        calm_window(c, ticks(start, *tick));
        *tick += 1;
    }
    let knee = c.current_kbps;
    let r1 = choke(c, start, tick).expect("first choke must back off");
    assert!(c.decode.cap.kbps().is_none(), "one event must not latch");
    c.on_ack(r1, None);
    climb_to(c, start, tick, knee - knee / DECODE_CAP_SIMILAR_DIV);
    let rate = c.current_kbps;
    let r2 = choke(c, start, tick).expect("re-climb choke must back off");
    assert_eq!(c.decode.cap.kbps(), Some(rate - rate / 16));
    c.on_ack(r2, None);
    rate - rate / 16
}

/// Stall-shaped: current/10 delivered, flush + kf-storm. Severe, but starved.
pub(super) fn stall_choke(
    c: &mut BitrateController,
    start: Instant,
    tick: &mut u32,
) -> Option<u32> {
    *tick += 2;
    let r = c.on_window(&WindowSample {
        actual_kbps: c.current_kbps / 10,
        flushed: true,
        recovery_kf: RECOVERY_KF_SEVERE,
        ..WindowSample::at(ticks(start, *tick))
    });
    *tick += 1;
    r
}

/// One clean, utilized, full-rate 120 Hz window with a given decode mean.
pub(super) fn loaded(c: &mut BitrateController, at: Instant, decode_us: i64) -> Option<u32> {
    c.on_window(&WindowSample {
        owd_mean_us: Some(10_000),
        decode_mean_us: Some(decode_us),
        actual_kbps: c.current_kbps,
        activity: WindowActivity::Active(90),
        ..WindowSample::at(at)
    })
}

/// A 120 Hz controller with room to climb and seeded baselines.
pub(super) fn seeded_120(start_kbps: u32) -> (BitrateController, Instant, u32) {
    let mut c = BitrateController::new(start_kbps, None);
    c.set_ceiling(400_000);
    c.set_frame_budget(120);
    let start = Instant::now();
    let mut t = 0;
    for _ in 0..4 {
        calm_window(&mut c, ticks(start, t));
        t += 1;
    }
    (c, start, t)
}

/// Clean windows at `decode_us` until the controller asks for a rate, acked.
pub(super) fn until_request(
    c: &mut BitrateController,
    start: Instant,
    t: &mut u32,
    decode_us: i64,
    max: u32,
) -> Option<u32> {
    for _ in 0..max {
        let r = loaded(c, ticks(start, *t), decode_us);
        *t += 1;
        if let Some(k) = r {
            c.on_ack(k, None);
            return Some(k);
        }
    }
    None
}
