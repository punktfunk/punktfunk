//! The audio plane's open and recovery rules, split out of the device-only [`crate::audio`] so
//! their tables run on the host. No NDK type and no clock: the shell reads the device, passes
//! plain values in and does what comes back.

/// AAudio's sharing mode, as the shell hands it to the stream builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sharing {
    Exclusive,
    Shared,
}

/// AAudio's performance mode, as the shell hands it to the stream builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Perf {
    LowLatency,
    None,
}

/// One rung of the open ladder: a sharing mode, a performance mode and the rate they are tried
/// at. Its `Debug` form is what the audio log lines print.
///
/// `rate` is `Some(hz)` for an explicit request, which AAudio honours or fails the open, and
/// `None` for AAUDIO_UNSPECIFIED, which lets the HAL name its own. [`grant_matches`] holds an
/// unspecified rung to the session's rate: it rescues a device that refuses an explicit rate it
/// already runs at, and rejects one that grants another.
///
/// `track` opens an `AudioTrack` at `perf` instead; `sharing` then means nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenRung {
    pub(crate) sharing: Sharing,
    pub(crate) perf: Perf,
    pub(crate) rate: Option<i32>,
    pub(crate) track: bool,
}

/// AAudio configurations to try, best first.
///
/// Phones prefer Exclusive/LowLatency; TVs start at Shared because some HDMI HALs open MMAP
/// streams that never route. Every mode is tried at the session's rate before one final
/// unspecified-rate pass. No rung asks for a different explicit rate.
///
/// `sharing` and `perf` are the raw `debug.punktfunk.audio_sharing` and
/// `debug.punktfunk.audio_perf` values, which pin the modes for field diagnosis. Nothing pins
/// the rate: it must match the wire format.
///
/// With `track`, two `AudioTrack` rungs (LowLatency, then None) come first. The AAudio rungs
/// after them still play the front pair on a device whose track refuses the layout.
pub(crate) fn ladder_for(
    is_tv: bool,
    rate_hz: u32,
    track: bool,
    sharing: Option<&str>,
    perf: Option<&str>,
) -> Vec<OpenRung> {
    use Perf::{LowLatency, None as PerfNone};
    use Sharing::{Exclusive, Shared};
    let mut rungs = Vec::new();
    if track {
        for perf in [LowLatency, PerfNone] {
            rungs.push(OpenRung {
                sharing: Shared,
                perf,
                rate: Some(rate_hz as i32),
                track: true,
            });
        }
    }
    let mut modes: Vec<(Sharing, Perf)> = match sharing {
        Some("exclusive") => vec![(Exclusive, LowLatency), (Exclusive, PerfNone)],
        Some("shared") => vec![(Shared, LowLatency), (Shared, PerfNone)],
        _ if is_tv => vec![(Shared, LowLatency), (Shared, PerfNone)],
        _ => vec![
            (Exclusive, LowLatency),
            (Shared, LowLatency),
            (Shared, PerfNone),
        ],
    };
    // LowLatency is a request a HAL may mishandle, so `none` is reachable as a forced choice,
    // not only as the last rung.
    match perf {
        Some("none") => modes.iter_mut().for_each(|m| m.1 = PerfNone),
        Some("lowlatency") | Some("low") => modes.iter_mut().for_each(|m| m.1 = LowLatency),
        _ => {}
    }
    modes.dedup();
    for rate in [Some(rate_hz as i32), None] {
        for &(sharing, perf) in &modes {
            rungs.push(OpenRung {
                sharing,
                perf,
                rate,
                track: false,
            });
        }
    }
    rungs
}

/// Whether a device's grant, `(rate, channels, f32)`, plays the session: the rung's rate (the
/// session's on an unspecified rung), the session's channel count, float samples. The callback
/// writes `frames × channels` f32, so another layout or format overruns AAudio's buffer, and
/// another rate is the wrong audio. `Err` names both sides for the log.
pub(crate) fn grant_matches(
    granted: (i32, i32, bool),
    rung_rate: Option<i32>,
    rate_hz: u32,
    channels: u8,
) -> Result<(), String> {
    let (rate, ch, float) = granted;
    let want_rate = rung_rate.unwrap_or(rate_hz as i32);
    if ch != i32::from(channels) || rate != want_rate || !float {
        return Err(format!(
            "granted rate={rate} ch={ch} f32={float}, needed {want_rate}/{channels}/PCM_Float",
        ));
    }
    Ok(())
}

/// When the newest frame written plays, ns on the timestamp's clock: the timestamp's time plus
/// the frames written past its position, at `rate_hz`.
pub(crate) fn heard_at(at_ns: i64, position: i64, written: i64, rate_hz: u32) -> i64 {
    at_ns + (written - position) * 1_000_000_000 / i64::from(rate_hz.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Perf::{LowLatency as LL, None as NO};
    use Sharing::{Exclusive as EX, Shared as SH};

    /// `(sharing, perf, rate, track)`, at a 48 kHz session.
    type Rung = (Sharing, Perf, Option<i32>, bool);
    /// `(row, is_tv, track, audio_sharing, audio_perf, ladder)`.
    type LadderCase = (
        &'static str,
        bool,
        bool,
        Option<&'static str>,
        Option<&'static str>,
        &'static [Rung],
    );

    const R: Option<i32> = Some(48_000);
    const PHONE: &[Rung] = &[
        (EX, LL, R, false),
        (SH, LL, R, false),
        (SH, NO, R, false),
        (EX, LL, None, false),
        (SH, LL, None, false),
        (SH, NO, None, false),
    ];
    const SHARED: &[Rung] = &[
        (SH, LL, R, false),
        (SH, NO, R, false),
        (SH, LL, None, false),
        (SH, NO, None, false),
    ];

    const LADDERS: &[LadderCase] = &[
        (
            "a phone tries exclusive first, every mode at the rate, then unspecified",
            false,
            false,
            None,
            None,
            PHONE,
        ),
        (
            "a TV never asks for exclusive",
            true,
            false,
            None,
            None,
            SHARED,
        ),
        (
            "the track fallback leads with two AudioTrack rungs",
            false,
            true,
            None,
            None,
            &[
                (SH, LL, R, true),
                (SH, NO, R, true),
                (EX, LL, R, false),
                (SH, LL, R, false),
                (SH, NO, R, false),
                (EX, LL, None, false),
                (SH, LL, None, false),
                (SH, NO, None, false),
            ],
        ),
        (
            "audio_sharing=exclusive pins a TV to exclusive",
            true,
            false,
            Some("exclusive"),
            None,
            &[
                (EX, LL, R, false),
                (EX, NO, R, false),
                (EX, LL, None, false),
                (EX, NO, None, false),
            ],
        ),
        (
            "audio_sharing=shared pins a phone to shared",
            false,
            false,
            Some("shared"),
            None,
            SHARED,
        ),
        (
            "an unknown audio_sharing keeps the device's ladder",
            false,
            false,
            Some("mmap"),
            None,
            PHONE,
        ),
        (
            "audio_perf=none drops low latency and the rung it duplicates",
            false,
            false,
            None,
            Some("none"),
            &[
                (EX, NO, R, false),
                (SH, NO, R, false),
                (EX, NO, None, false),
                (SH, NO, None, false),
            ],
        ),
        (
            "audio_perf=low pins low latency",
            false,
            false,
            None,
            Some("low"),
            &[
                (EX, LL, R, false),
                (SH, LL, R, false),
                (EX, LL, None, false),
                (SH, LL, None, false),
            ],
        ),
        (
            "audio_perf=lowlatency pins it too, here on a TV",
            true,
            false,
            None,
            Some("lowlatency"),
            &[(SH, LL, R, false), (SH, LL, None, false)],
        ),
        (
            "audio_perf never reaches the track rungs",
            true,
            true,
            None,
            Some("none"),
            &[
                (SH, LL, R, true),
                (SH, NO, R, true),
                (SH, NO, R, false),
                (SH, NO, None, false),
            ],
        ),
    ];

    #[test]
    fn ladders_follow_the_device_class_the_fallback_and_the_pins() {
        for &(row, is_tv, track, sharing, perf, want) in LADDERS {
            let got: Vec<Rung> = ladder_for(is_tv, 48_000, track, sharing, perf)
                .into_iter()
                .map(|r| (r.sharing, r.perf, r.rate, r.track))
                .collect();
            assert_eq!(got, want, "{row}");
        }
    }

    #[test]
    fn every_explicit_rung_asks_for_the_session_rate() {
        let ladder = ladder_for(false, 96_000, true, None, None);
        assert!(ladder.iter().all(|r| r.rate.is_none_or(|hz| hz == 96_000)));
    }

    /// The audio log lines print rungs with `{:?}`; field bundles grep that text.
    #[test]
    fn a_rung_prints_as_the_logs_have_it() {
        let rung = ladder_for(false, 48_000, false, None, None)[0];
        assert_eq!(
            format!("{rung:?}"),
            "OpenRung { sharing: Exclusive, perf: LowLatency, rate: Some(48000), track: false }"
        );
    }

    #[test]
    fn a_grant_plays_only_at_the_rate_layout_and_format_asked_for() {
        // (row, granted, rung rate, session rate, session channels, plays)
        let rows = [
            (
                "an exact grant plays",
                (48_000, 2, true),
                R,
                48_000,
                2,
                true,
            ),
            (
                "an unspecified rung back at the session rate plays",
                (96_000, 6, true),
                None,
                96_000,
                6,
                true,
            ),
            (
                "an unspecified rung at another rate is refused, never resampled",
                (48_000, 2, true),
                None,
                96_000,
                2,
                false,
            ),
            (
                "an explicit rung is held to its own rate",
                (44_100, 2, true),
                Some(44_100),
                48_000,
                2,
                true,
            ),
            (
                "a stereo grant for a 5.1 session is refused",
                (48_000, 2, true),
                R,
                48_000,
                6,
                false,
            ),
            (
                "integer samples are refused",
                (48_000, 2, false),
                R,
                48_000,
                2,
                false,
            ),
        ];
        for (row, granted, rung_rate, rate, channels, plays) in rows {
            let got = grant_matches(granted, rung_rate, rate, channels);
            assert_eq!(got.is_ok(), plays, "{row}: {got:?}");
        }
    }

    /// The refusal is a log line field bundles grep.
    #[test]
    fn a_refused_grant_names_both_sides() {
        assert_eq!(
            grant_matches((44_100, 2, true), None, 48_000, 2),
            Err("granted rate=44100 ch=2 f32=true, needed 48000/2/PCM_Float".into())
        );
    }

    #[test]
    fn heard_at_extrapolates_the_frames_past_the_timestamp() {
        // (row, at_ns, position, written, rate_hz, heard_at)
        let rows = [
            (
                "nothing past the stamp plays at it",
                5_000,
                960,
                960,
                48_000,
                5_000,
            ),
            (
                "480 frames at 48 kHz are 10 ms",
                7,
                100,
                580,
                48_000,
                10_000_007,
            ),
            ("44.1 kHz rounds toward zero", 0, 0, 1, 44_100, 22_675),
            ("a rate of 0 reads as 1 Hz", 0, 0, 1, 0, 1_000_000_000),
        ];
        for (row, at, position, written, rate, want) in rows {
            assert_eq!(heard_at(at, position, written, rate), want, "{row}");
        }
    }
}
