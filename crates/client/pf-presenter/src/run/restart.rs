//! Why a stream is dialled again without the player. The pump ends a stream with one terminal
//! [`SessionEvent`]; three dial the same host again: a lost link on the shared redial schedule,
//! a woken host that isn't ready yet inside its settle window, and a codec out of decode rungs
//! with that codec excluded. [`restart_for`] decides from the event and [`RestartFacts`];
//! `Shell::restart` in `stream.rs` performs it. `None` ends the stream as the event says.
//! The table test pins every branch.

use super::session::WAKE_SETTLE;
use super::{PunktfunkEndReason, SessionEvent, StreamState};
use std::time::{Duration, Instant};

/// How the stream's host is dialled again.
#[derive(Debug, PartialEq)]
pub(crate) enum Dial {
    /// After `wait`; `attempt` counts the redial schedule from 1.
    Redial { wait: Duration, attempt: u32 },
    /// Dialled again while the host refuses, until `settle_until`.
    Wake { settle_until: Instant },
    /// Advertising none of the codecs in `exclude`.
    Fallback { exclude: u8 },
}

impl Dial {
    /// The pump's wait before it dials: only a redial after a loss waits.
    pub(crate) fn wait(&self) -> Duration {
        match self {
            Dial::Redial { wait, .. } => *wait,
            Dial::Wake { .. } | Dial::Fallback { .. } => Duration::ZERO,
        }
    }
}

/// A dial and what the shell does around it.
#[derive(Debug, PartialEq)]
pub(crate) struct Restart<'a> {
    pub dial: Dial,
    /// Hand back the pads and the capture first. A dial that never connected holds neither.
    pub release: bool,
    /// Dial at the mode the connector ended on: a mid-session reconfigure lives only there.
    pub connector_mode: bool,
    /// The player had the mic muted: the next session starts with it, before its uplink runs.
    pub keep_mic_muted: bool,
    /// The console's `Reconnecting` line.
    pub phase: &'a str,
}

/// What the stream knows when a terminal event lands.
pub(crate) struct RestartFacts {
    /// Redials of a lost session this stream already is.
    pub redials: u32,
    /// The player cancelled the dial from the console.
    pub canceled: bool,
    pub keep_mic_muted: bool,
    /// The params' settle window, set once a wake re-dial opened it.
    pub settle_until: Option<Instant>,
    /// The codecs the params already exclude.
    pub exclude_codecs: u8,
    pub now: Instant,
}

impl RestartFacts {
    pub(super) fn of(st: &StreamState) -> RestartFacts {
        RestartFacts {
            redials: st.redials,
            canceled: st.canceled,
            keep_mic_muted: st.handle.mic.muted(),
            settle_until: st.params.settle_until,
            exclude_codecs: st.params.exclude_codecs,
            now: Instant::now(),
        }
    }
}

/// The restart `ev` calls for, or `None` when the stream ends as `ev` says. Single and browse
/// mode restart alike: single has no console to fall back to.
pub(crate) fn restart_for<'a>(ev: &'a SessionEvent, f: &RestartFacts) -> Option<Restart<'a>> {
    match ev {
        // A failed dial counts only as a redial's next attempt.
        SessionEvent::Lost(_)
        | SessionEvent::Failed {
            trust_rejected: false,
            refused: None,
            ..
        } if !f.canceled => {
            let lost = matches!(ev, SessionEvent::Lost(_));
            if !lost && f.redials == 0 {
                return None;
            }
            let wait = PunktfunkEndReason::Lost.redial_delay(f.redials)?;
            Some(Restart {
                dial: Dial::Redial {
                    wait,
                    attempt: f.redials + 1,
                },
                release: lost,
                connector_mode: true,
                keep_mic_muted: f.keep_mic_muted,
                phase: pf_l10n::client::notice_reconnecting(),
            })
        }
        SessionEvent::HostNotReady(_) if !f.canceled => Some(Restart {
            dial: Dial::Wake {
                settle_until: f.settle_until.unwrap_or(f.now + WAKE_SETTLE),
            },
            release: true,
            connector_mode: false,
            keep_mic_muted: f.keep_mic_muted,
            phase: pf_l10n::client::notice_host_not_ready_retry(),
        }),
        // Widened, never replaced: a second fallback must not re-offer what the first ruled out.
        // A cancelled stream never dials again, whatever ends it.
        SessionEvent::CodecFallback {
            exclude_codecs,
            msg,
            ..
        } if !f.canceled => Some(Restart {
            dial: Dial::Fallback {
                exclude: f.exclude_codecs | exclude_codecs,
            },
            release: true,
            connector_mode: true,
            keep_mic_muted: f.keep_mic_muted,
            phase: msg,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::reject::RejectReason;

    const LOST: &str = "Lost the connection — reconnecting.";
    const WAKE: &str = "The host isn't ready to stream yet — trying again.";
    const FELL_BACK: &str = "HEVC decoding failed on this device — reconnecting";

    fn failed(trust_rejected: bool, refused: Option<RejectReason>) -> SessionEvent {
        SessionEvent::Failed {
            msg: "The host didn't answer".into(),
            trust_rejected,
            refused,
        }
    }

    #[test]
    fn each_terminal_event_restarts_as_the_shell_does() {
        let now = Instant::now();
        let s = Duration::from_secs;
        let at = |redials| RestartFacts {
            redials,
            canceled: false,
            keep_mic_muted: false,
            settle_until: None,
            exclude_codecs: 0,
            now,
        };
        let cancelled = |redials| RestartFacts {
            canceled: true,
            ..at(redials)
        };
        let redial = |wait, attempt, release| {
            Some(Restart {
                dial: Dial::Redial {
                    wait: s(wait),
                    attempt,
                },
                release,
                connector_mode: true,
                keep_mic_muted: false,
                phase: LOST,
            })
        };
        let wake = |settle_until| {
            Some(Restart {
                dial: Dial::Wake { settle_until },
                release: true,
                connector_mode: false,
                keep_mic_muted: false,
                phase: WAKE,
            })
        };
        let fallback = |exclude| {
            Some(Restart {
                dial: Dial::Fallback { exclude },
                release: true,
                connector_mode: true,
                keep_mic_muted: false,
                phase: FELL_BACK,
            })
        };

        let lost = SessionEvent::Lost(Some("The connection dropped".into()));
        let ended = SessionEvent::Ended(None);
        let dial_failed = failed(false, None);
        let trust = failed(true, None);
        let refused = failed(false, Some(RejectReason::ProfileUnknown));
        let not_ready = SessionEvent::HostNotReady("capture is not up yet".into());
        let codec = SessionEvent::CodecFallback {
            exclude_codecs: 0b010,
            retry_caps: 0b101,
            msg: FELL_BACK.into(),
        };
        let open = now + s(3);
        let late = now + WAKE_SETTLE + s(5);
        let muted = RestartFacts {
            keep_mic_muted: true,
            ..at(0)
        };

        let rows = [
            (
                "a lost link redials at once",
                &lost,
                at(0),
                redial(0, 1, true),
            ),
            (
                "a third loss waits the schedule's 2 s",
                &lost,
                at(2),
                redial(2, 3, true),
            ),
            (
                "a failed redial is the next attempt and releases nothing",
                &dial_failed,
                at(1),
                redial(1, 2, false),
            ),
            ("a failed first dial ends", &dial_failed, at(0), None),
            ("a spent schedule ends a lost link", &lost, at(5), None),
            (
                "a spent schedule ends a failed redial",
                &dial_failed,
                at(5),
                None,
            ),
            ("a trust-rejected redial ends", &trust, at(1), None),
            ("a refused redial ends", &refused, at(1), None),
            (
                "a host that ended the session is not redialled",
                &ended,
                at(1),
                None,
            ),
            ("a cancelled lost link ends", &lost, cancelled(0), None),
            (
                "a cancelled failed redial ends",
                &dial_failed,
                cancelled(1),
                None,
            ),
            (
                "wake-settle: the first refusal opens the window, at the launch's mode",
                &not_ready,
                at(0),
                wake(now + WAKE_SETTLE),
            ),
            (
                "wake-settle: a later refusal keeps the window",
                &not_ready,
                RestartFacts {
                    settle_until: Some(open),
                    ..at(0)
                },
                wake(open),
            ),
            (
                "wake-settle: an open window is kept, not shortened",
                &not_ready,
                RestartFacts {
                    settle_until: Some(late),
                    ..at(0)
                },
                wake(late),
            ),
            ("a cancelled wake ends", &not_ready, cancelled(0), None),
            (
                "a codec fallback widens the exclusion, at the connector's mode",
                &codec,
                RestartFacts {
                    exclude_codecs: 0b100,
                    ..at(0)
                },
                fallback(0b110),
            ),
            (
                "a cancelled stream's codec fallback ends",
                &codec,
                cancelled(0),
                None,
            ),
            (
                "a muted mic rides the restart",
                &lost,
                muted,
                redial(0, 1, true).map(|r| Restart {
                    keep_mic_muted: true,
                    ..r
                }),
            ),
        ];
        for (name, ev, facts, want) in rows {
            assert_eq!(restart_for(ev, &facts), want, "{name}");
        }
    }

    #[test]
    fn only_a_redial_waits_before_it_dials() {
        let four = Duration::from_secs(4);
        let redial = Dial::Redial {
            wait: four,
            attempt: 4,
        };
        assert_eq!(redial.wait(), four);
        let wake = Dial::Wake {
            settle_until: Instant::now(),
        };
        assert_eq!(wake.wait(), Duration::ZERO, "a wake re-dial after a redial");
        let fallback = Dial::Fallback { exclude: 0b010 };
        assert_eq!(fallback.wait(), Duration::ZERO, "a fallback after a redial");
    }
}
