//! Per-pad dedup for the 0xCD HID-output plane, shared by DualSense, DS4, and Deck
//! managers via [`crate::uhid_manager`].
//!
//! A game packs rumble, lightbar, LEDs, and adaptive triggers into one report, so
//! an unchanged rich field is re-sent on every rumble tick. This forwards a field
//! only when its value changes. `TrackpadHaptic` pulses always fire. `HidRaw` is
//! never deduped: firmware watchdogs require identical periodic refreshes.
//!
//! The plane rides unreliable datagrams. A dropped change is not resent by the
//! game, so [`HidoutDedup::renewals`] re-emits latched state every [`RENEW_EVERY`].
//! Tests in this file pin the contract.

use punktfunk_core::quic::HidOutput;
use std::time::{Duration, Instant};

/// Interval for re-emitting latched 0xCD state when the game's value has not changed.
///
/// Dedup plus unreliable datagrams: a dropped change is never produced again.
/// 1000 ms bounds the stale window; ≤4 datagrams/pad/s sits under rumble's ~120 ms refresh.
const RENEW_EVERY: Duration = Duration::from_millis(1000);

/// Last-forwarded 0xCD values. Value kinds dedup; `TrackpadHaptic` and `HidRaw` always fire.
#[derive(Clone, Default)]
pub struct HidoutDedup {
    led: Option<(u8, u8, u8)>,
    player_leds: Option<u8>,
    mic_led: Option<u8>,
    /// Last-forwarded adaptive-trigger effect per side: `[0]` = L2, `[1]` = R2.
    trigger: [Option<Vec<u8>>; 2],
    audio_ctl: Option<(u8, [u8; 6])>,
    haptics_select_logged: bool,
    last_sent: Option<Instant>,
}

impl HidoutDedup {
    /// Drop latched values so the first report after a (re)plug is forwarded.
    pub fn clear(&mut self) {
        *self = HidoutDedup::default();
    }

    /// `true` to send `h`. A forward stamps `now` so [`Self::renewals`] waits a full window.
    pub fn should_forward(&mut self, h: &HidOutput, now: Instant) -> bool {
        let fwd = self.decide(h);
        if fwd {
            self.last_sent = Some(now);
        }
        fwd
    }

    /// Re-emit latched state once [`RENEW_EVERY`] has passed. Idempotent on the client.
    ///
    /// One-shots are omitted: replaying `TrackpadHaptic` would be a new pulse. `HidRaw`
    /// is already refreshed by the device's own cadence.
    pub fn renewals(&mut self, pad: u8, now: Instant) -> Vec<HidOutput> {
        if self
            .last_sent
            .is_none_or(|t| now.duration_since(t) < RENEW_EVERY)
        {
            return Vec::new();
        }
        self.last_sent = Some(now);
        let mut out = Vec::new();
        if let Some((r, g, b)) = self.led {
            out.push(HidOutput::Led { pad, r, g, b });
        }
        if let Some(bits) = self.player_leds {
            out.push(HidOutput::PlayerLeds { pad, bits });
        }
        if let Some(mode) = self.mic_led {
            out.push(HidOutput::MicLed { pad, mode });
        }
        for (which, effect) in self.trigger.iter().enumerate() {
            if let Some(effect) = effect {
                out.push(HidOutput::Trigger {
                    pad,
                    which: which as u8,
                    effect: effect.clone(),
                });
            }
        }
        // Routing and volumes are state too: unrenewed, one lost datagram leaves the
        // pad on rumble or muted until the title next changes the value.
        if let Some((flags, raw)) = self.audio_ctl {
            out.push(HidOutput::AudioCtl {
                pad: u16::from(pad),
                flags,
                raw,
            });
        }
        out
    }

    fn decide(&mut self, h: &HidOutput) -> bool {
        match h {
            HidOutput::Led { r, g, b, .. } => {
                let v = Some((*r, *g, *b));
                if self.led == v {
                    false
                } else {
                    self.led = v;
                    true
                }
            }
            HidOutput::PlayerLeds { bits, .. } => {
                let v = Some(*bits);
                if self.player_leds == v {
                    false
                } else {
                    self.player_leds = v;
                    true
                }
            }
            HidOutput::MicLed { mode, .. } => {
                let v = Some(*mode);
                if self.mic_led == v {
                    false
                } else {
                    self.mic_led = v;
                    true
                }
            }
            HidOutput::Trigger { which, effect, .. } => {
                let slot = (*which as usize).min(1);
                if self.trigger[slot].as_deref() == Some(effect.as_slice()) {
                    false
                } else {
                    self.trigger[slot] = Some(effect.clone());
                    true
                }
            }
            // One-shot: latching and replaying would fire a second pulse.
            HidOutput::TrackpadHaptic { .. } => true,
            HidOutput::AudioCtl { pad, flags, raw } => {
                let v = Some((*flags, *raw));
                if self.audio_ctl == v {
                    false
                } else {
                    // Once per pad: the title selected audio haptics, not rumble emulation.
                    if flags & 0x01 != 0 && !self.haptics_select_logged {
                        self.haptics_select_logged = true;
                        tracing::info!(
                            "DS5 title asserted haptics-select (audio haptics) pad={pad}"
                        );
                    }
                    self.audio_ctl = v;
                    true
                }
            }
            // Firmware watchdogs need identical refreshes (~40 ms rumble vs ~50 ms timeout;
            // lizard-off every ~3 s). Deduping a repeat silences motors or re-enables lizard mode.
            HidOutput::HidRaw { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidout_dedup_forwards_only_changes() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        assert!(d.should_forward(&led(10), t));
        assert!(!d.should_forward(&led(10), t));
        assert!(d.should_forward(&led(20), t));

        let pl = |bits| HidOutput::PlayerLeds { pad: 0, bits };
        assert!(d.should_forward(&pl(0b101), t));
        assert!(!d.should_forward(&pl(0b101), t));
        assert!(!d.should_forward(&led(20), t));

        let mic = |mode| HidOutput::MicLed { pad: 0, mode };
        assert!(d.should_forward(&mic(1), t));
        assert!(!d.should_forward(&mic(1), t));
        assert!(d.should_forward(&mic(0), t));
        let later = t + RENEW_EVERY;
        assert!(
            d.renewals(0, later).contains(&mic(0)),
            "the LED state renews"
        );

        let trig = |which, byte| HidOutput::Trigger {
            pad: 0,
            which,
            effect: vec![byte, 0, 0],
        };
        assert!(d.should_forward(&trig(0, 1), t));
        assert!(d.should_forward(&trig(1, 1), t));
        assert!(!d.should_forward(&trig(0, 1), t));
        assert!(d.should_forward(&trig(0, 2), t));

        let haptic = HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        assert!(d.should_forward(&haptic, t));
        assert!(d.should_forward(&haptic, t));

        d.clear();
        assert!(d.should_forward(&led(20), t));
        assert!(d.should_forward(&pl(0b101), t));
        assert!(d.should_forward(&trig(0, 2), t));
    }

    #[test]
    fn latched_state_is_renewed_so_a_lost_datagram_is_not_permanent() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        let trig = HidOutput::Trigger {
            pad: 3,
            which: 1,
            effect: vec![0x02, 0x90, 0xA0],
        };
        assert!(d.should_forward(&trig, t));
        assert!(
            !d.should_forward(&trig, t),
            "the game re-sends it; the dedup swallows it"
        );

        assert!(d.renewals(3, t + Duration::from_millis(999)).is_empty());

        let out = d.renewals(3, t + Duration::from_millis(1000));
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            HidOutput::Trigger { pad: 3, which: 1, effect } if effect == &vec![0x02, 0x90, 0xA0]
        ));

        assert!(d.renewals(3, t + Duration::from_millis(1500)).is_empty());
        assert_eq!(d.renewals(3, t + Duration::from_millis(2000)).len(), 1);
    }

    #[test]
    fn renewal_covers_every_latched_plane_and_an_active_plane_defers_it() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        assert!(d.should_forward(
            &HidOutput::Led {
                pad: 0,
                r: 9,
                g: 8,
                b: 7
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b100
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::Trigger {
                pad: 0,
                which: 0,
                effect: vec![1]
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::Trigger {
                pad: 0,
                which: 1,
                effect: vec![2]
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::AudioCtl {
                pad: 0,
                flags: 0x17,
                raw: [0x50, 0, 0, 0, 0, 0]
            },
            t
        ));

        let out = d.renewals(0, t + Duration::from_millis(1000));
        assert_eq!(
            out.len(),
            5,
            "lightbar + player LEDs + both triggers + audio routing, got {out:?}"
        );

        let later = t + Duration::from_millis(1500);
        assert!(d.should_forward(
            &HidOutput::Led {
                pad: 0,
                r: 1,
                g: 2,
                b: 3
            },
            later
        ));
        assert!(d.renewals(0, later + Duration::from_millis(999)).is_empty());
        assert!(!d
            .renewals(0, later + Duration::from_millis(1000))
            .is_empty());
    }

    #[test]
    fn renewal_is_silent_with_nothing_latched_and_never_replays_a_pulse() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        assert!(d.renewals(0, t + Duration::from_secs(60)).is_empty());

        let pulse = HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        assert!(d.should_forward(&pulse, t));
        // Pulse stamps the clock but latches nothing; a replay would be a new pulse.
        assert!(d.renewals(0, t + Duration::from_millis(1000)).is_empty());
    }

    #[test]
    fn audio_ctl_dedups_by_value() {
        let mut d = HidoutDedup::default();
        let t = Instant::now();
        let audio = |flags, vol| HidOutput::AudioCtl {
            pad: 0,
            flags,
            raw: [vol, 0, 0, 0, 0, 0],
        };
        assert!(d.should_forward(&audio(0x17, 0x50), t));
        assert!(!d.should_forward(&audio(0x17, 0x50), t));
        assert!(d.should_forward(&audio(0x16, 0x50), t));
        assert!(d.should_forward(&audio(0x16, 0x60), t));
        assert!(d.should_forward(&HidOutput::PlayerLeds { pad: 0, bits: 1 }, t));
        d.clear();
        assert!(d.should_forward(&audio(0x16, 0x60), t));
    }
}
