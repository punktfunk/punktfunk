//! `--trajectory`: a session on the shipped client pump, writing down every
//! Automatic-bitrate window.
//!
//! The rest of the probe drives the wire by hand. This path does not: it opens
//! a [`NativeClient`], which runs the same `DataPump` and `abr::Driver` the
//! desktop and TV clients run, and records what the controller decided. A
//! trajectory assembled here rather than read off the driver would measure the
//! recorder, and the glue between pump and controller is exactly what tier 1
//! cannot see.

use anyhow::{Context, Result};
use punktfunk_core::abr::metrics;
use punktfunk_core::abr::WindowRecord;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::{CompositorPref, Mode};
use std::io::Write;

/// What the shaped link is, for the metrics the client cannot measure from
/// inside the session.
#[derive(Clone, Copy, Debug, Default)]
pub struct Link {
    /// Rate this profile could hold if nothing went wrong. `0` = the best the
    /// session actually reached, which reads "time to 90 % of its own best".
    pub achievable_kbps: u32,
    /// The profile's shaped rate, for the bytes-over-capacity metric. `0` =
    /// unknown, and the metric reports zero rather than a guess.
    pub capacity_kbps: u32,
}

impl Link {
    /// `<achievable_kbps>[:<capacity_kbps>]`.
    pub fn parse(spec: &str) -> Option<Link> {
        let (a, c) = spec.split_once(':').unwrap_or((spec, "0"));
        Some(Link {
            achievable_kbps: a.parse().ok()?,
            capacity_kbps: c.parse().ok()?,
        })
    }
}

/// One ask per this, while the picture is held. `punktfunk-webos`
/// `KEYFRAME_REQUEST_MIN_INTERVAL`.
const KEYFRAME_REQUEST_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Resume without a clean re-anchor after this. `punktfunk-webos` `HOLD_GIVE_UP`:
/// a permanent freeze is worse than a broken picture.
const HOLD_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(2);

/// The picture a real decoder cannot show, modelled on the shipped webOS client
/// (`src/session.rs`, the `holding` state).
///
/// A P-frame whose reference is gone is undecodable, so contiguous frame
/// indexes do not end a freeze — only a frame carrying the re-anchor does, or
/// [`HOLD_GIVE_UP`]. The rig has no decoder; without this it resumes the
/// instant indexes line up again and the burst's cost disappears from the
/// window the controller judges.
#[derive(Default)]
struct Hold {
    holding: bool,
    since: Option<std::time::Instant>,
    last_ask: Option<std::time::Instant>,
}

impl Hold {
    /// One arrived AU at `now`. Returns the freeze this frame ended, zero
    /// otherwise, and `true` in `ask` when a keyframe request is owed.
    fn step(
        &mut self,
        gap: bool,
        lost: bool,
        flags: u32,
        now: std::time::Instant,
    ) -> (std::time::Duration, bool) {
        if (gap || lost) && !self.holding {
            self.holding = true;
            self.since = Some(now);
        }
        let ask = self.holding
            && self
                .last_ask
                .is_none_or(|t| now.duration_since(t) >= KEYFRAME_REQUEST_MIN_INTERVAL);
        if ask {
            self.last_ask = Some(now);
        }
        let re_anchor = flags & u32::from(punktfunk_core::packet::FLAG_SOF) != 0
            || flags & punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR != 0;
        let gave_up = self
            .since
            .is_some_and(|t| now.duration_since(t) >= HOLD_GIVE_UP);
        if !self.holding || re_anchor || gave_up {
            let was = self
                .since
                .map_or(std::time::Duration::ZERO, |t| now.duration_since(t));
            self.holding = false;
            self.since = None;
            return (was, ask);
        }
        (std::time::Duration::ZERO, ask)
    }

    /// [`Hold::step`] against the clock, sending what it asks for.
    fn on_frame(
        &mut self,
        client: &NativeClient,
        gap: bool,
        lost: bool,
        flags: u32,
    ) -> std::time::Duration {
        let (held, ask) = self.step(gap, lost, flags, std::time::Instant::now());
        if ask {
            let _ = client.request_keyframe();
        }
        held
    }
}

/// One window as a JSON object. Hand-written: one line of output does not earn
/// a serialization dependency.
fn window_json(w: &WindowRecord, held_ms: u64) -> String {
    let opt = |v: Option<i64>| v.map_or("null".to_string(), |v| v.to_string());
    format!(
        concat!(
            r#"{{"t_ms":{},"target_kbps":{},"request_kbps":{},"delivered_kbps":{},"#,
            r#""loss_ppm":{},"lost_frames":{},"owd_mean_us":{},"decode_mean_us":{},"#,
            r#""encode_mean_us":{},"keyframe_asks":{},"held_ms":{},"flushed":{},"#,
            r#""discarded":{},"reason":"{:?}"}}"#
        ),
        w.t_ms,
        w.rate_kbps,
        w.request_kbps.map_or("null".to_string(), |k| k.to_string()),
        w.sample.actual_kbps,
        w.sample.loss_ppm,
        w.sample.dropped,
        opt(w.sample.owd_mean_us),
        opt(w.sample.decode_mean_us),
        opt(w.sample.encode_mean_us),
        w.sample.recovery_kf,
        held_ms,
        w.sample.flushed,
        w.discarded,
        w.reason,
    )
}

/// Score a finished run the way `abr/sim` scores a modelled one.
///
/// Two inputs the session cannot see are approximated and named here rather
/// than in the arithmetic: the path's own delay is the smallest window mean
/// observed, and the bytes offered in the first ten seconds are what the
/// session was running at over them — the host spends its budget, so the rate
/// it was told to run at is what it offered.
pub fn summary(
    name: &str,
    windows: &[WindowRecord],
    duration_ms: u64,
    link: Link,
) -> (metrics::Metrics, String) {
    let per: Vec<metrics::MetricWindow> = windows.iter().map(WindowRecord::metric).collect();
    let owd_ms: Vec<u32> = windows
        .iter()
        .filter_map(|w| w.sample.owd_mean_us)
        .map(|us| (us / 1_000).max(0) as u32)
        .collect();
    let base_delay_ms = owd_ms.iter().copied().min().unwrap_or(0);
    let achievable = if link.achievable_kbps > 0 {
        link.achievable_kbps
    } else {
        windows.iter().map(|w| w.rate_kbps).max().unwrap_or(0)
    };
    let offered_10s: u64 = windows
        .iter()
        .filter(|w| w.t_ms < 10_000)
        .map(|w| u64::from(w.rate_kbps) * 1_000 / 8 * 750 / 1_000)
        .sum();
    let capacity_10s = u64::from(link.capacity_kbps) * 1_000 / 8 * 10;
    let m = metrics::measure(&metrics::Run {
        sessions: &[&per],
        owd_ms: &owd_ms,
        base_delay_ms,
        duration_ms,
        achievable_kbps: achievable,
        offered_10s,
        capacity_10s,
        blip_at_ms: None,
    });
    (m, m.row(name))
}

/// Stream for `seconds`, recording every closed window, then write the file.
#[allow(clippy::too_many_arguments)]
pub fn run(
    connect: &str,
    mode: Mode,
    pin: Option<[u8; 32]>,
    identity: Option<(String, String)>,
    client_name: &str,
    seconds: u64,
    path: &str,
    link: Link,
    profile: &str,
    decoder_hold: bool,
) -> Result<()> {
    let (host, port) = connect
        .rsplit_once(':')
        .context("--connect wants HOST:PORT")?;
    let port: u16 = port.parse().context("--connect port")?;
    let client = NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        GamepadPref::Auto,
        // Automatic: the only case the controller arms.
        0,
        0,
        2,
        punktfunk_core::quic::CODEC_H264
            | punktfunk_core::quic::CODEC_HEVC
            | punktfunk_core::quic::CODEC_AV1,
        0,
        None,
        0,
        false,
        None,
        Some(client_name.to_string()),
        pin,
        identity,
        std::time::Duration::from_secs(15),
    )
    .map_err(|e| anyhow::anyhow!("connect to the host: {e:?}"))?;
    tracing::info!(
        start_kbps = client.current_bitrate_kbps(),
        mode = ?client.mode(),
        "trajectory session open"
    );

    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(seconds);
    let mut windows: Vec<WindowRecord> = Vec::new();
    let mut held: Vec<u64> = Vec::new();
    let mut frames = 0u64;
    let mut dropped = client.frames_dropped();
    let mut hold = Hold::default();
    let mut held_since_window = std::time::Duration::ZERO;
    while std::time::Instant::now() < deadline && !client.is_session_ended() {
        // Pull at the wire's pace: a client that lets the frame channel back up
        // makes the pump drop frames, which would read as a damaged link.
        if let Ok(f) = client.next_frame(std::time::Duration::from_millis(20)) {
            frames += 1;
            // What a decoder loop does with each AU: a forward gap asks for an
            // intra refresh or a keyframe. Those asks are half of what a report
            // window is judged on, so a recorder that skipped them would show a
            // damaged link as a clean one.
            let gap = client.note_frame_index(f.frame_index) > 0;
            let now_dropped = client.frames_dropped();
            let lost = now_dropped > dropped;
            dropped = now_dropped;
            if decoder_hold {
                held_since_window += hold.on_frame(&client, gap, lost, f.flags);
            } else if lost {
                // Backstop for an AU parity could not repair: infinite GOP
                // conceals a reference-missing frame, so nothing else asks.
                let _ = client.request_keyframe();
            }
        }
        let batch = client.take_abr_windows();
        if !batch.is_empty() {
            held.resize(held.len() + batch.len(), 0);
            // The drain runs every ≤20 ms and a window closes every 750 ms, so a
            // batch is one window in all but a stall; the held time goes to its
            // last member.
            if let Some(last) = held.last_mut() {
                *last = held_since_window.as_millis() as u64;
            }
            held_since_window = std::time::Duration::ZERO;
            windows.extend(batch);
        }
    }
    let tail = client.take_abr_windows();
    held.resize(held.len() + tail.len(), 0);
    windows.extend(tail);
    held.resize(windows.len(), 0);
    let duration_ms = started.elapsed().as_millis() as u64;

    let out = std::fs::File::create(path).with_context(|| format!("create {path}"))?;
    let row = write_trajectory(out, &windows, &held, profile, duration_ms, frames, link)?;
    println!("{}", metrics::HEADER);
    println!("{row}");
    Ok(())
}

/// One JSON line per window, then the summary. Returns the summary's
/// [`metrics::HEADER`] row.
fn write_trajectory(
    mut out: impl Write,
    windows: &[WindowRecord],
    held: &[u64],
    profile: &str,
    duration_ms: u64,
    frames: u64,
    link: Link,
) -> Result<String> {
    for (w, held_ms) in windows.iter().zip(held.iter().chain(std::iter::repeat(&0))) {
        writeln!(out, "{}", window_json(w, *held_ms)).context("write a window")?;
    }
    let (m, row) = summary(profile, windows, duration_ms, link);
    writeln!(
        out,
        concat!(
            r#"{{"summary":"{}","windows":{},"frames":{},"duration_ms":{},"#,
            r#""under5_pct":{},"to90_s":{},"cuts_per_10min":{},"lost_per_10min":{},"#,
            r#""queue_p95_ms":{},"over_cap_kb_10s":{},"blip_recover_s":{},"#,
            r#""fairness_x1000":{},"decisions_fnv1a":"{:08x}"}}"#
        ),
        profile,
        windows.len(),
        frames,
        duration_ms,
        m.under5_pct,
        m.to90_s,
        m.cuts_per_10min,
        m.lost_per_10min,
        m.queue_p95_ms,
        m.over_cap_kb_10s,
        m.blip_recover_s,
        m.fairness_x1000,
        m.decisions_fnv1a,
    )
    .context("write the summary")?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::abr::{Reason, WindowSample};

    fn rec(t_ms: u64, rate_kbps: u32, request_kbps: Option<u32>, dropped: u64) -> WindowRecord {
        let now = std::time::Instant::now();
        WindowRecord {
            t_ms,
            rate_kbps,
            request_kbps,
            sample: WindowSample {
                now,
                dropped,
                loss_ppm: 0,
                owd_mean_us: Some(12_000),
                decode_mean_us: None,
                encode_mean_us: None,
                actual_kbps: rate_kbps * 9 / 10,
                flushed: false,
                recovery_kf: 0,
                activity: punktfunk_core::abr::WindowActivity::Active(30),
            },
            discarded: false,
            reason: Reason::Clean,
        }
    }

    /// `"key":value` out of one JSON line. The recorded file is the rig's
    /// output format, so a test that re-reads it proves the format too.
    fn field<'a>(line: &'a str, key: &str) -> &'a str {
        let at = line
            .find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("{key} missing from {line}"))
            + key.len()
            + 3;
        let rest = &line[at..];
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        rest[..end].trim_matches('"')
    }

    /// The summary is what the window lines add up to. Written to a file,
    /// read back, re-scored from the lines alone: the two must agree, or the
    /// trajectory and its summary describe different sessions.
    #[test]
    fn a_recorded_files_summary_is_what_its_window_lines_add_up_to() {
        let mut ws: Vec<WindowRecord> = (0..8)
            .map(|i| rec(750 * (i + 1), if i < 4 { 4_000 } else { 18_000 }, None, 0))
            .collect();
        ws[2].request_kbps = Some(3_000);
        ws[2].sample.dropped = 5;
        let link = Link {
            achievable_kbps: 20_000,
            capacity_kbps: 0,
        };
        let path = std::env::temp_dir().join("pf-abr-rig-trajectory-test.jsonl");
        let file = std::fs::File::create(&path).expect("create the trajectory");
        write_trajectory(file, &ws, &[], "wan_wg_12", 6_000, 400, link).expect("write it");
        let text = std::fs::read_to_string(&path).expect("read it back");
        let _ = std::fs::remove_file(&path);

        let mut lines: Vec<&str> = text.lines().collect();
        let summary_line = lines.pop().expect("the summary is the last line");
        let reread: Vec<metrics::MetricWindow> = lines
            .iter()
            .map(|l| metrics::MetricWindow {
                t_ms: field(l, "t_ms").parse().unwrap(),
                rate_kbps: field(l, "target_kbps").parse().unwrap(),
                request_kbps: field(l, "request_kbps").parse().ok(),
                dropped: field(l, "lost_frames").parse().unwrap(),
                discarded: field(l, "discarded") == "true",
            })
            .collect();
        let owd: Vec<u32> = lines
            .iter()
            .map(|l| field(l, "owd_mean_us").parse::<u32>().unwrap() / 1_000)
            .collect();
        let scored = metrics::measure(&metrics::Run {
            sessions: &[&reread],
            owd_ms: &owd,
            base_delay_ms: owd.iter().copied().min().unwrap_or(0),
            duration_ms: 6_000,
            achievable_kbps: 20_000,
            offered_10s: 0,
            capacity_10s: 0,
            blip_at_ms: None,
        });
        for (key, got) in [
            ("under5_pct", scored.under5_pct),
            ("to90_s", scored.to90_s),
            ("cuts_per_10min", scored.cuts_per_10min),
            ("lost_per_10min", scored.lost_per_10min),
            ("queue_p95_ms", scored.queue_p95_ms),
        ] {
            assert_eq!(
                field(summary_line, key),
                got.to_string(),
                "{key} in the summary against the window lines"
            );
        }
        // And the numbers are the ones this run actually produced: half the
        // windows under 5 Mbps, one cut, five lost frames, and 18 000 is 90 %
        // of 20 000 — reached by the window that closed at 3 750 ms.
        assert_eq!(scored.under5_pct, 50);
        assert_eq!(scored.to90_s, 3);
        assert_eq!(scored.cuts_per_10min, 100, "one cut in six seconds");
        assert_eq!(scored.lost_per_10min, 500);
        assert_eq!(scored.queue_p95_ms, 0, "a flat 12 ms owd is all base delay");
        assert_eq!(field(summary_line, "windows"), "8");
        assert_eq!(field(summary_line, "frames"), "400");
    }

    /// A window line carries the driver's own numbers, and the fields the rig
    /// reads back are where it left them.
    #[test]
    fn a_window_line_is_readable_json() {
        let mut w = rec(1_500, 9_800, Some(6_800), 2);
        w.discarded = true;
        w.reason = Reason::Owd;
        let line = window_json(&w, 480);
        for want in [
            r#""t_ms":1500"#,
            r#""target_kbps":9800"#,
            r#""request_kbps":6800"#,
            r#""delivered_kbps":8820"#,
            r#""lost_frames":2"#,
            r#""owd_mean_us":12000"#,
            r#""decode_mean_us":null"#,
            r#""discarded":true"#,
            r#""held_ms":480"#,
            r#""reason":"Owd""#,
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
    }

    /// A lost frame freezes the picture; the P-frames after it do not thaw it,
    /// however many arrive and however contiguous their indexes. Only a frame
    /// carrying the re-anchor does — and while frozen the client asks once per
    /// [`KEYFRAME_REQUEST_MIN_INTERVAL`], which is what the controller counts.
    #[test]
    fn a_held_picture_thaws_on_a_keyframe_and_not_on_p_frames() {
        const PIC: u32 = 0x1;
        const IDR: u32 = PIC | (punktfunk_core::packet::FLAG_SOF as u32);
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        let mut h = Hold::default();

        // Clean stream: nothing held, nothing asked.
        assert_eq!(
            h.step(false, false, PIC, at(0)),
            (std::time::Duration::ZERO, false)
        );
        // A lost frame freezes it and asks straight away.
        let (held, ask) = h.step(false, true, PIC, at(10));
        assert!(ask && held.is_zero(), "the loss freezes and asks");
        // Eight P-frames over 250 ms: still frozen, and the ask is throttled to
        // one per 100 ms rather than one per frame.
        let asks = (1..=8)
            .map(|i| h.step(false, false, PIC, at(10 + i * 30)))
            .filter(|(held, ask)| {
                assert!(held.is_zero(), "a P-frame does not thaw a broken reference");
                *ask
            })
            .count();
        assert_eq!(asks, 2, "one ask per 100 ms over 240 ms, not one per frame");
        // The keyframe thaws it, and reports the freeze it ended.
        let (held, _) = h.step(false, false, IDR, at(300));
        assert_eq!(held.as_millis(), 290);
        // Thawed: the next P-frame is ordinary again.
        assert_eq!(
            h.step(false, false, PIC, at(320)),
            (std::time::Duration::ZERO, false)
        );
    }

    /// A host that never answers must not freeze the picture for ever: the
    /// client gives up after [`HOLD_GIVE_UP`] and shows what it has.
    #[test]
    fn a_hold_nobody_answers_gives_up() {
        let t0 = std::time::Instant::now();
        let mut h = Hold::default();
        h.step(true, false, 0x1, t0);
        assert!(h.step(false, false, 0x1, t0 + HOLD_GIVE_UP / 2).0.is_zero());
        let (held, _) = h.step(false, false, 0x1, t0 + HOLD_GIVE_UP);
        assert_eq!(held, HOLD_GIVE_UP, "gave up and resumed on a P-frame");
    }

    #[test]
    fn a_link_spec_takes_one_or_two_numbers() {
        assert_eq!(
            Link::parse("12000:12500").map(|l| (l.achievable_kbps, l.capacity_kbps)),
            Some((12_000, 12_500))
        );
        assert_eq!(
            Link::parse("12000").map(|l| (l.achievable_kbps, l.capacity_kbps)),
            Some((12_000, 0))
        );
        assert!(Link::parse("fast").is_none());
    }
}
