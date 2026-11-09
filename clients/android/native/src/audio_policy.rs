//! The audio plane's open and recovery rules, split out of the device-only [`crate::audio`] so
//! their tables run on the host. No NDK type and no clock: the shell reads the device, passes
//! plain values in and does what comes back.

use std::sync::mpsc::TrySendError;

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

/// Reopens in a row that may find no usable device before the plane gives up: about 2 s with the
/// shell's settle time, which outlasts an HDMI mode switch. Bounded, so a device gone for good
/// ends in silence instead of a loop of opens on the audio thread.
pub(crate) const REOPEN_ATTEMPTS: u32 = 8;

/// Why the decode loop returned. Only `Disconnected` is worth reopening the device for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeExit {
    /// The playback was dropped: session teardown, or Kotlin's `nativeStopAudio`.
    Shutdown,
    /// The device went away: AAudio reported a disconnect, a track write failed, or the renderer
    /// closed the ring. A dead stream recovers only by a close and a new open.
    Disconnected,
    /// The connector closed: no more audio is coming.
    SessionClosed,
    /// The decoder refused the negotiated format. Another device would not change that.
    Fatal,
    /// The host changed the audio format; the packet that opens it waits for a device opened
    /// at it.
    Reformat,
}

/// What a chunk handed to the ring means for the decode loop. A full ring drops the chunk. A
/// closed ring is a renderer that went away: an `AudioTrack` writer ends on a failed write and
/// takes the ring with it.
pub(crate) fn sent<T>(result: Result<(), TrySendError<T>>) -> Result<(), DecodeExit> {
    match result {
        Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
        Err(TrySendError::Disconnected(_)) => Err(DecodeExit::Disconnected),
    }
}

/// What the supervisor does once a decode loop returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Settle, then open the device again as a new generation.
    Reopen,
    /// The device went away and stays closed: the session is stopping, or
    /// `debug.punktfunk.audio_reopen` pins reopening off.
    Stop,
    /// Nothing is left to reopen for.
    End,
}

/// What the supervisor does when no rung on the ladder gave it a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoDevice {
    /// The session is stopping.
    Quit,
    /// Settle and walk the ladder again; the number is this attempt, from 1.
    Retry(u32),
    /// Audio is off for the rest of this format.
    GiveUp,
}

/// One audio format's device generations: one per device opened, a new one after each
/// disconnect the supervisor reopens.
#[derive(Default)]
pub(crate) struct Supervisor {
    /// Reopens so far; 0 while the first device plays.
    pub(crate) generation: u32,
    /// Ladder walks since the last open that found no device.
    attempt: u32,
}

impl Supervisor {
    /// A rung gave a device: the next disconnect gets its own retries.
    pub(crate) fn opened(&mut self) {
        self.attempt = 0;
    }

    /// No rung gave a device. The first open gives up at once. A reopen retries up to
    /// [`REOPEN_ATTEMPTS`] times, since it can land inside the route change that caused the
    /// disconnect.
    pub(crate) fn no_device(&mut self, shutting_down: bool) -> NoDevice {
        if shutting_down {
            return NoDevice::Quit;
        }
        if self.generation > 0 && self.attempt < REOPEN_ATTEMPTS {
            self.attempt += 1;
            return NoDevice::Retry(self.attempt);
        }
        NoDevice::GiveUp
    }

    /// The decode loop returned `exit`. Only a device that went away reopens, and only while the
    /// session runs and `reopen_allowed` holds.
    pub(crate) fn next(
        &mut self,
        exit: DecodeExit,
        reopen_allowed: bool,
        shutting_down: bool,
    ) -> Next {
        match exit {
            DecodeExit::Disconnected if reopen_allowed && !shutting_down => {
                self.generation += 1;
                Next::Reopen
            }
            DecodeExit::Disconnected => Next::Stop,
            DecodeExit::Shutdown
            | DecodeExit::SessionClosed
            | DecodeExit::Fatal
            | DecodeExit::Reformat => Next::End,
        }
    }
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
    fn a_full_ring_drops_the_chunk_a_closed_one_is_a_lost_renderer() {
        assert_eq!(sent::<()>(Ok(())), Ok(()));
        assert_eq!(sent(Err(TrySendError::Full(()))), Ok(()));
        assert_eq!(
            sent(Err(TrySendError::Disconnected(()))),
            Err(DecodeExit::Disconnected)
        );
    }

    #[test]
    fn only_a_device_that_went_away_reopens() {
        let lost_renderer = sent(Err(TrySendError::Disconnected(()))).unwrap_err();
        // (row, exit, reopen allowed, shutting down, next)
        let rows = [
            (
                "R5-1: a renderer that went away reopens",
                lost_renderer,
                true,
                false,
                Next::Reopen,
            ),
            (
                "a disconnect while the session stops stays closed",
                DecodeExit::Disconnected,
                true,
                true,
                Next::Stop,
            ),
            (
                "audio_reopen=0 keeps a disconnect closed",
                DecodeExit::Disconnected,
                false,
                false,
                Next::Stop,
            ),
            (
                "a dropped playback ends",
                DecodeExit::Shutdown,
                true,
                false,
                Next::End,
            ),
            (
                "a closed connector ends",
                DecodeExit::SessionClosed,
                true,
                false,
                Next::End,
            ),
            (
                "a decoder that will not build ends",
                DecodeExit::Fatal,
                true,
                false,
                Next::End,
            ),
            (
                "a format change ends this format's supervisor",
                DecodeExit::Reformat,
                true,
                false,
                Next::End,
            ),
        ];
        for (row, exit, allowed, stopping, want) in rows {
            let mut sup = Supervisor::default();
            assert_eq!(sup.next(exit, allowed, stopping), want, "{row}");
            assert_eq!(sup.generation, u32::from(want == Next::Reopen), "{row}");
        }
    }

    #[test]
    fn the_first_open_that_finds_no_device_gives_up_at_once() {
        assert_eq!(Supervisor::default().no_device(false), NoDevice::GiveUp);
    }

    #[test]
    fn a_reopen_retries_across_a_route_change_then_gives_up() {
        let mut sup = Supervisor::default();
        sup.next(DecodeExit::Disconnected, true, false);
        let walks: Vec<_> = (0..=REOPEN_ATTEMPTS)
            .map(|_| sup.no_device(false))
            .collect();
        let want: Vec<_> = (1..=REOPEN_ATTEMPTS)
            .map(NoDevice::Retry)
            .chain([NoDevice::GiveUp])
            .collect();
        assert_eq!(walks, want);
    }

    #[test]
    fn an_open_gives_the_next_disconnect_its_own_retries() {
        let mut sup = Supervisor::default();
        sup.next(DecodeExit::Disconnected, true, false);
        sup.no_device(false);
        sup.no_device(false);
        sup.opened();
        sup.next(DecodeExit::Disconnected, true, false);
        assert_eq!(sup.no_device(false), NoDevice::Retry(1));
        assert_eq!(sup.generation, 2);
    }

    #[test]
    fn a_stopping_session_quits_without_spending_an_attempt() {
        let mut sup = Supervisor::default();
        sup.next(DecodeExit::Disconnected, true, false);
        assert_eq!(sup.no_device(true), NoDevice::Quit);
        assert_eq!(sup.no_device(false), NoDevice::Retry(1));
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
