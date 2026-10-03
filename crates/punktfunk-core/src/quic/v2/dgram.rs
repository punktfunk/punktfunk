//! v2 QUIC datagrams: `kind ‖ payload`, the kind a varint from the registry.
//!
//! Audio, input state and the host's events keep their encodings: the payload is one datagram
//! of the existing vocabulary, its own tag byte included, and the kind must agree with that tag.
//! Feedback and clock samples are tagged fields: feedback replaces the loss, delivery and
//! recovery messages that rode the reliable stream, so it numbers its report window and its ask,
//! and a host acts on each once however often the client repeats it.

use super::field::*;
use super::registry::*;
use crate::error::Result;
use crate::quic::{ClockEcho, ClockProbe};

/// One received datagram. Payloads borrow the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dgram<'a> {
    /// A media packet, on a carrier with no raw UDP path.
    Media(&'a [u8]),
    /// One audio datagram: desktop, redundant, PCM, pad or mic.
    Audio(&'a [u8]),
    /// One input event or rich-input datagram.
    InputState(&'a [u8]),
    Feedback(Feedback),
    ClockProbe(ClockProbe),
    ClockEcho(ClockEcho),
    /// Rumble, HID output, cursor state, host timing or HDR metadata, as its own datagram.
    HostEvent(&'a [u8]),
}

/// Tags the wrapped payload of each kind may start with.
fn inner_tags(kind: u64) -> &'static [u8] {
    use crate::quic as q;
    match kind {
        DGRAM_AUDIO => &[
            q::AUDIO_MAGIC,
            q::AUDIO_RED_MAGIC,
            q::AUDIO_PCM_MAGIC,
            q::PAD_AUDIO_MAGIC,
            q::MIC_MAGIC,
        ],
        DGRAM_INPUT_STATE => &[crate::input::INPUT_MAGIC, q::RICH_INPUT_MAGIC],
        DGRAM_RUMBLE => &[q::RUMBLE_MAGIC],
        DGRAM_HID_OUTPUT => &[q::HIDOUT_MAGIC],
        DGRAM_CURSOR_STATE => &[q::CURSOR_STATE_MAGIC],
        DGRAM_HOST_TIMING => &[q::HOST_TIMING_MAGIC],
        DGRAM_HDR_META => &[q::HDR_META_MAGIC],
        _ => &[],
    }
}

/// The v2 kind an existing datagram rides under, from its tag byte.
pub fn kind_of(v1_datagram: &[u8]) -> Option<u64> {
    let tag = *v1_datagram.first()?;
    [
        DGRAM_AUDIO,
        DGRAM_INPUT_STATE,
        DGRAM_RUMBLE,
        DGRAM_HID_OUTPUT,
        DGRAM_CURSOR_STATE,
        DGRAM_HOST_TIMING,
        DGRAM_HDR_META,
    ]
    .into_iter()
    .find(|&k| inner_tags(k).contains(&tag))
}

/// `kind ‖ payload`.
pub fn encode(kind: u64, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(payload.len() + 2);
    put_varint(&mut b, kind);
    b.extend_from_slice(payload);
    b
}

/// An existing datagram under its v2 kind; `None` for a tag no kind carries.
pub fn wrap(v1_datagram: &[u8]) -> Option<Vec<u8>> {
    kind_of(v1_datagram).map(|k| encode(k, v1_datagram))
}

/// `None` for an unknown kind, a payload whose tag disagrees with its kind, or bad fields:
/// the receiver drops the datagram and counts it.
pub fn decode(b: &[u8]) -> Option<Dgram<'_>> {
    let (kind, n) = get_varint(b)?;
    let payload = &b[n..];
    match kind {
        DGRAM_MEDIA => Some(Dgram::Media(payload)),
        DGRAM_FEEDBACK => Feedback::from_fields(payload).ok().map(Dgram::Feedback),
        DGRAM_CLOCK => clock_from_fields(payload).ok().flatten(),
        _ => {
            if !inner_tags(kind).contains(payload.first()?) {
                return None;
            }
            Some(match kind {
                DGRAM_AUDIO => Dgram::Audio(payload),
                DGRAM_INPUT_STATE => Dgram::InputState(payload),
                _ => Dgram::HostEvent(payload),
            })
        }
    }
}

/// `client → host` receive state. Sent at the end of each report window, and on every frame
/// interval while an ask is open, until the frame that answers it arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    /// Report window number from 1; `0` carries no window. A host reads each window once.
    pub window: u32,
    /// Shards parity repaired over the window, ppm (`LossReport`).
    pub loss_ppm: u32,
    /// Media packets received this session (`DeliveryReport`).
    pub packets_received: u64,
    /// Ask number from 1; `0` asks nothing. A host acts on each ask once.
    pub ask: u16,
    /// Frames to invalidate, first and last (`RfiRequest`).
    pub invalidate: Option<(u32, u32)>,
    /// A keyframe, whatever else the ask says (`RequestKeyframe`).
    pub keyframe: bool,
}

impl Feedback {
    pub fn encode(&self) -> Vec<u8> {
        let f = Fields::new()
            .when(self.window != 0, |f| {
                f.u32(1, self.window)
                    .u32(2, self.loss_ppm)
                    .u64(3, self.packets_received)
            })
            .when(self.ask != 0, |f| {
                f.u16(4, self.ask)
                    .when(self.invalidate.is_some(), |f| {
                        let (first, last) = self.invalidate.unwrap_or_default();
                        f.u32(5, first).u32(6, last)
                    })
                    .when(self.keyframe, |f| f.bool(7, true))
            });
        encode(DGRAM_FEEDBACK, &f.into_body())
    }

    fn from_fields(body: &[u8]) -> Result<Feedback> {
        let mut fb = Feedback::default();
        let (mut first, mut last) = (None, None);
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            match tag {
                1 => fb.window = u32_of(v)?,
                2 => fb.loss_ppm = u32_of(v)?,
                3 => fb.packets_received = u64_of(v)?,
                4 => fb.ask = u16_of(v)?,
                5 => set_once(&mut first, u32_of(v)?)?,
                6 => set_once(&mut last, u32_of(v)?)?,
                7 => fb.keyframe = bool_of(v)?,
                _ => {}
            }
        }
        fb.invalidate = first.zip(last);
        Ok(fb)
    }
}

/// A probe carries `t1` (1); an echo adds the host's receive and send times (2, 3).
pub fn encode_clock_probe(p: &ClockProbe) -> Vec<u8> {
    encode(DGRAM_CLOCK, &Fields::new().u64(1, p.t1_ns).into_body())
}

pub fn encode_clock_echo(e: &ClockEcho) -> Vec<u8> {
    let f = Fields::new()
        .u64(1, e.t1_ns)
        .u64(2, e.t2_ns)
        .u64(3, e.t3_ns);
    encode(DGRAM_CLOCK, &f.into_body())
}

fn clock_from_fields(body: &[u8]) -> Result<Option<Dgram<'static>>> {
    let (mut t1, mut t2, mut t3) = (None, None, None);
    let mut r = FieldReader::new(body);
    while let Some((tag, v)) = r.next_field()? {
        match tag {
            1 => set_once(&mut t1, u64_of(v)?)?,
            2 => set_once(&mut t2, u64_of(v)?)?,
            3 => set_once(&mut t3, u64_of(v)?)?,
            _ => {}
        }
    }
    Ok(match (t1, t2, t3) {
        (Some(t1_ns), None, None) => Some(Dgram::ClockProbe(ClockProbe { t1_ns })),
        (Some(t1_ns), Some(t2_ns), Some(t3_ns)) => Some(Dgram::ClockEcho(ClockEcho {
            t1_ns,
            t2_ns,
            t3_ns,
        })),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic as q;
    use proptest::prelude::*;

    #[test]
    fn existing_datagrams_ride_under_their_kind() {
        let audio = q::encode_audio_datagram(7, 1_000, &[1, 2, 3]);
        let rumble = q::encode_rumble_datagram(1, 2, 3);
        for (v1, kind) in [(&audio[..], DGRAM_AUDIO), (&rumble[..], DGRAM_RUMBLE)] {
            let w = wrap(v1).unwrap();
            assert_eq!(get_varint(&w).unwrap().0, kind);
            let back = decode(&w).unwrap();
            let inner = match back {
                Dgram::Audio(p) | Dgram::HostEvent(p) => p,
                other => panic!("{other:?}"),
            };
            assert_eq!(inner, v1);
        }
        assert_eq!(
            q::decode_audio_datagram(match decode(&wrap(&audio).unwrap()).unwrap() {
                Dgram::Audio(p) => p,
                _ => unreachable!(),
            }),
            Some((7, 1_000, &[1u8, 2, 3][..]))
        );
        // A payload whose tag belongs to another kind is dropped.
        assert_eq!(decode(&encode(DGRAM_AUDIO, &rumble)), None);
        assert_eq!(decode(&encode(0x3F, &audio)), None);
        assert_eq!(wrap(&[0x01, 2, 3]), None);
        assert_eq!(
            decode(&encode(DGRAM_MEDIA, &[9, 9])),
            Some(Dgram::Media(&[9, 9]))
        );
    }

    #[test]
    fn feedback_and_clock_round_trip() {
        for fb in [
            Feedback {
                window: 4,
                loss_ppm: 1200,
                packets_received: 1 << 40,
                ..Default::default()
            },
            Feedback {
                ask: 9,
                invalidate: Some((100, 103)),
                ..Default::default()
            },
            Feedback {
                window: 5,
                ask: 10,
                keyframe: true,
                ..Default::default()
            },
        ] {
            assert_eq!(decode(&fb.encode()), Some(Dgram::Feedback(fb)));
        }
        let p = ClockProbe { t1_ns: 5 };
        assert_eq!(decode(&encode_clock_probe(&p)), Some(Dgram::ClockProbe(p)));
        let e = ClockEcho {
            t1_ns: 5,
            t2_ns: 6,
            t3_ns: 7,
        };
        assert_eq!(decode(&encode_clock_echo(&e)), Some(Dgram::ClockEcho(e)));
        let half = encode(DGRAM_CLOCK, &Fields::new().u64(2, 1).into_body());
        assert_eq!(decode(&half), None);
    }

    proptest! {
        #[test]
        fn any_bytes_decode_or_drop(b in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = decode(&b);
        }
    }
}
