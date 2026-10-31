//! v2 QUIC datagrams: `kind ‖ payload`, the kind a varint from the registry.
//!
//! Audio, input state and the host's events keep their encodings: the payload is one datagram
//! of the existing vocabulary, its own tag byte included, and the kind must agree with that tag.
//! Feedback is tagged fields: the client's whole receive state. It numbers its report window
//! and its ask, and a host acts on each once however often the client repeats it.

use super::field::*;
use super::registry::*;
use crate::error::Result;

/// One received datagram. Payloads borrow the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dgram<'a> {
    /// A media packet, on a carrier with no raw UDP path.
    Media(&'a [u8]),
    /// One audio datagram: desktop, redundant, PCM, bitstream, pad or mic.
    Audio(&'a [u8]),
    /// One input event or rich-input datagram.
    InputState(&'a [u8]),
    Feedback(Feedback),
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
            q::AUDIO_BITSTREAM_MAGIC,
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

/// The payload of a datagram a client reads: audio, input state or a host event. `None`
/// for any other kind, which the client skips.
pub fn client_payload(b: &[u8]) -> Option<&[u8]> {
    match decode(b)? {
        Dgram::Audio(p) | Dgram::InputState(p) | Dgram::HostEvent(p) => Some(p),
        _ => None,
    }
}

/// Most shards one NACK names.
pub const NACK_MAX: usize = 16;

/// Most shards past its parity a frame may lack and still ask for them: a resend of up to
/// [`NACK_MAX`] shards (~23 KB) costs less than an anchor and a dropped frame. More is a
/// queue, which a resend would feed. One NACK still names the shards the parity floor covers.
pub(crate) const NACK_SHORT: u32 = 8;
const _: () = assert!(NACK_SHORT as usize + crate::config::MIN_RECOVERY_SHARDS <= NACK_MAX);

/// Shards of one frame asked for again, by their index in the AU: data first, then parity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nack {
    pub frame: u32,
    len: u8,
    idx: [u16; NACK_MAX],
}

impl Nack {
    /// `None` when `idx` is empty or longer than [`NACK_MAX`].
    pub fn new(frame: u32, idx: &[u16]) -> Option<Nack> {
        if idx.is_empty() || idx.len() > NACK_MAX {
            return None;
        }
        let mut n = Nack {
            frame,
            len: idx.len() as u8,
            idx: [0; NACK_MAX],
        };
        n.idx[..idx.len()].copy_from_slice(idx);
        Some(n)
    }

    pub fn shards(&self) -> &[u16] {
        &self.idx[..usize::from(self.len)]
    }
}

/// `client → host` receive state. Sent when a report window closes, on every frame interval
/// while an ask is open until the frame that answers it arrives, and after each frame the
/// decoder takes clean. Every one repeats the last closed window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    /// The last closed report window's number, from 1; `0` before the first. It and its
    /// fields ride every datagram until the next window closes. A host reads each once.
    pub window: u32,
    /// Shards parity repaired over the window, and the holes partial frames went up with, ppm.
    pub loss_ppm: u32,
    /// What the window's dead frames add to `loss_ppm`: `loss_ppm + dead_ppm` is every data
    /// shard that never arrived. Dead-frame holes inform the report, link health and the
    /// operator; the host's automatic responses read `loss_ppm` alone, parity-repairable
    /// loss, until the FEC policy says otherwise. A client before it sends `0`.
    pub dead_ppm: u32,
    /// Media packets received this session.
    pub packets_received: u64,
    /// The window's lost shards by their place in the frame: the first twelve data shards,
    /// the middle, the last twelve with all parity.
    pub head: u32,
    pub mid: u32,
    pub tail: u32,
    /// Packets the client itself dropped over the window: its kernel socket's and its demux
    /// queue's.
    pub sock_drops: u32,
    /// The rate the client holds the link to carry, kbps; `0` = not measured yet. A level,
    /// not a count: it rides any feedback, with or without a window.
    pub link_kbps: u32,
    /// How the client wants frames shaped (`0` the host's default). A level, like
    /// `link_kbps`.
    pub shape: u8,
    /// Ask number from 1; `0` asks nothing. A host acts on each ask once.
    pub ask: u16,
    /// Frames to invalidate, first and last.
    pub invalidate: Option<(u32, u32)>,
    /// A keyframe, whatever else the ask says.
    pub keyframe: bool,
    /// Shards to send again.
    pub nack: Option<Nack>,
    /// The newest frame the client completed, and the sixteen before it as bits (bit `i`
    /// is frame `last − 1 − i`). Rides with the levels, without an ask.
    pub acked: Option<(u32, u16)>,
    /// The video stream the ask names frames of; `0` the first screen's. Another screen's
    /// datagram carries its keyframe ask only: the window and levels are the first screen's.
    pub stream: u8,
}

impl Feedback {
    pub fn encode(&self) -> Vec<u8> {
        let nz = |f: Fields, tag, v: u32| f.when(v != 0, |f| f.u32(tag, v));
        let f = Fields::new()
            .when(self.stream != 0, |f| f.u8(18, self.stream))
            .when(self.window != 0, |f| {
                let f = f
                    .u32(1, self.window)
                    .u32(2, self.loss_ppm)
                    .u64(3, self.packets_received);
                let f = nz(nz(f, 9, self.head), 10, self.mid);
                nz(
                    nz(nz(f, 11, self.tail), 12, self.sock_drops),
                    19,
                    self.dead_ppm,
                )
            })
            .when(self.link_kbps != 0, |f| f.u32(8, self.link_kbps))
            .when(self.shape != 0, |f| f.u8(16, self.shape))
            .when(self.ask != 0, |f| {
                f.u16(4, self.ask)
                    .when(self.invalidate.is_some(), |f| {
                        let (first, last) = self.invalidate.unwrap_or_default();
                        f.u32(5, first).u32(6, last)
                    })
                    .when(self.keyframe, |f| f.bool(7, true))
                    .when(self.nack.is_some(), |f| {
                        let n = self.nack.unwrap_or_default();
                        f.u32(17, n.frame).u16s(13, n.shards())
                    })
            })
            .when(self.acked.is_some(), |f| {
                let (last, mask) = self.acked.unwrap_or_default();
                f.u32(14, last).u16(15, mask)
            });
        encode(DGRAM_FEEDBACK, &f.into_body())
    }

    fn from_fields(body: &[u8]) -> Result<Feedback> {
        let mut fb = Feedback::default();
        let (mut first, mut last) = (None, None);
        let (mut nack_frame, mut nack) = (None, Vec::new());
        let (mut acked_last, mut acked_mask) = (None, None);
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
                8 => fb.link_kbps = u32_of(v)?,
                9 => fb.head = u32_of(v)?,
                10 => fb.mid = u32_of(v)?,
                11 => fb.tail = u32_of(v)?,
                12 => fb.sock_drops = u32_of(v)?,
                13 => push_bounded(&mut nack, u16_of(v)?, NACK_MAX)?,
                14 => set_once(&mut acked_last, u32_of(v)?)?,
                15 => set_once(&mut acked_mask, u16_of(v)?)?,
                16 => fb.shape = u8_of(v)?,
                17 => set_once(&mut nack_frame, u32_of(v)?)?,
                18 => fb.stream = u8_of(v)?,
                19 => fb.dead_ppm = u32_of(v)?,
                _ => {}
            }
        }
        fb.invalidate = first.zip(last);
        if !nack.is_empty() {
            let frame = required(nack_frame, "NACK without its frame")?;
            fb.nack = Nack::new(frame, &nack);
        }
        fb.acked = acked_last.map(|l| (l, acked_mask.unwrap_or(0)));
        Ok(fb)
    }
}

/// Control-stream frame types a client before the feedback datagram (0.43 and older) sends.
/// Retired: the registry never hands these numbers out again.
const RETIRED_LINK_REPORT: u64 = 0x27;
const RETIRED_LOSS_REPORT: u64 = 0x2E;
const RETIRED_DELIVERY_REPORT: u64 = 0x2F;
const RETIRED_RFI_REQUEST: u64 = 0x36;
const RETIRED_REQUEST_KEYFRAME: u64 = 0x37;
const RETIRED: [u64; 5] = [
    RETIRED_LINK_REPORT,
    RETIRED_LOSS_REPORT,
    RETIRED_DELIVERY_REPORT,
    RETIRED_RFI_REQUEST,
    RETIRED_REQUEST_KEYFRAME,
];

/// An older client's control-stream reports and asks, read as the [`Feedback`] a current
/// client sends, numbered from 1. Such a client sends its loss report and then its delivery
/// count each window; the count closes the window.
#[derive(Debug, Default)]
pub struct RetiredFeedback {
    window: u32,
    ask: u16,
    loss_ppm: u32,
    link_kbps: u32,
}

impl RetiredFeedback {
    pub fn reads(ty: u64) -> bool {
        RETIRED.contains(&ty)
    }

    /// The feedback frame `ty` stands for. `None` for a loss report, which waits for its
    /// window's delivery count, and for a body that does not parse.
    pub fn read(&mut self, ty: u64, body: &[u8]) -> Option<Feedback> {
        let (mut one, mut two) = (None, None);
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field().ok()? {
            match tag {
                1 => one = Some(v),
                2 => two = Some(v),
                _ => {}
            }
        }
        let u32_at = |v: Option<&[u8]>| v.map_or(Ok(0), u32_of).ok();
        let mut fb = Feedback {
            link_kbps: self.link_kbps,
            ..Feedback::default()
        };
        match ty {
            RETIRED_LOSS_REPORT => {
                self.loss_ppm = u32_at(one)?;
                return None;
            }
            RETIRED_DELIVERY_REPORT => {
                fb.packets_received = one.map_or(Ok(0), u64_of).ok()?;
                self.window += 1;
                fb.window = self.window;
                fb.loss_ppm = std::mem::take(&mut self.loss_ppm);
            }
            RETIRED_LINK_REPORT => {
                self.link_kbps = u32_at(one)?;
                fb.link_kbps = self.link_kbps;
            }
            RETIRED_RFI_REQUEST => {
                fb.invalidate = Some((u32_at(one)?, u32_at(two)?));
                fb.ask = self.next_ask();
            }
            RETIRED_REQUEST_KEYFRAME => {
                fb.keyframe = true;
                fb.ask = self.next_ask();
            }
            _ => return None,
        }
        Some(fb)
    }

    fn next_ask(&mut self) -> u16 {
        self.ask = self.ask.wrapping_add(1).max(1);
        self.ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic as q;
    use proptest::prelude::*;

    #[test]
    fn existing_datagrams_ride_under_their_kind() {
        let audio = q::encode_audio_datagram(7, 1_000, &[1, 2, 3]);
        let bitstream = q::encode_audio_bitstream_datagram(7, 1_000, 0, 2, &[1, 2, 3]);
        let rumble = q::encode_rumble_datagram(1, 2, 3);
        for (v1, kind) in [
            (&audio[..], DGRAM_AUDIO),
            (&bitstream[..], DGRAM_AUDIO),
            (&rumble[..], DGRAM_RUMBLE),
        ] {
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

    /// A client takes audio, input and host events, and skips the kinds the pump handles
    /// itself.
    #[test]
    fn a_client_reads_the_payload_of_the_kinds_it_takes() {
        let audio = q::encode_audio_datagram(7, 1_000, &[1, 2, 3]);
        let rumble = q::encode_rumble_datagram(1, 2, 3);
        assert_eq!(client_payload(&wrap(&audio).unwrap()), Some(&audio[..]));
        assert_eq!(client_payload(&wrap(&rumble).unwrap()), Some(&rumble[..]));
        assert_eq!(client_payload(&Feedback::default().encode()), None);
        assert_eq!(client_payload(&encode(DGRAM_MEDIA, &[9, 9])), None);
        assert_eq!(client_payload(&[]), None);
    }

    /// The frames a 0.43 client sends, field for field as it encoded them.
    #[test]
    fn an_older_clients_control_frames_read_as_feedback() {
        let body = |f: Fields| f.into_body();
        let mut r = RetiredFeedback::default();
        assert!(RetiredFeedback::reads(RETIRED_RFI_REQUEST));
        assert!(!RetiredFeedback::reads(MSG_SET_BITRATE));
        assert!(FRAMES.iter().all(|(_, t, _)| !RETIRED.contains(t)));

        let link = r.read(RETIRED_LINK_REPORT, &body(Fields::new().u32(1, 80_000)));
        assert_eq!(
            link.map(|f| (f.link_kbps, f.window, f.ask)),
            Some((80_000, 0, 0))
        );
        assert_eq!(
            r.read(RETIRED_LOSS_REPORT, &body(Fields::new().u32(1, 1_500))),
            None
        );
        let w = r
            .read(RETIRED_DELIVERY_REPORT, &body(Fields::new().u64(1, 9_000)))
            .unwrap();
        assert_eq!(
            (w.window, w.loss_ppm, w.packets_received, w.link_kbps, w.ask),
            (1, 1_500, 9_000, 80_000, 0)
        );
        // A window's loss is its own: the next window without a loss report reads none.
        let next = r.read(RETIRED_DELIVERY_REPORT, &body(Fields::new().u64(1, 9_400)));
        assert_eq!(next.map(|f| (f.window, f.loss_ppm)), Some((2, 0)));

        let rfi = r
            .read(
                RETIRED_RFI_REQUEST,
                &body(Fields::new().u32(1, 40).u32(2, 42)),
            )
            .unwrap();
        assert_eq!(
            (rfi.ask, rfi.invalidate, rfi.window),
            (1, Some((40, 42)), 0)
        );
        r.ask = u16::MAX;
        let kf = r.read(RETIRED_REQUEST_KEYFRAME, &[]).unwrap();
        assert_eq!((kf.ask, kf.keyframe), (1, true));
        assert_eq!(r.read(RETIRED_RFI_REQUEST, &[0xFF]), None);
    }

    #[test]
    fn feedback_and_clock_round_trip() {
        for fb in [
            Feedback {
                window: 4,
                loss_ppm: 1200,
                packets_received: 1 << 40,
                link_kbps: 940_000,
                head: 1,
                mid: 2,
                tail: 7,
                sock_drops: 3,
                shape: 1,
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
            Feedback {
                link_kbps: 2_250_000,
                ..Default::default()
            },
            Feedback {
                ask: 11,
                nack: Nack::new(812, &[3, 4, 9, 40]),
                ..Default::default()
            },
            Feedback {
                acked: Some((7_000, 0b1011)),
                ..Default::default()
            },
            Feedback {
                stream: 2,
                ask: 3,
                keyframe: true,
                ..Default::default()
            },
            Feedback {
                window: 6,
                loss_ppm: 40,
                dead_ppm: 52_000,
                packets_received: 90_000,
                tail: 2,
                link_kbps: 940_000,
                shape: 1,
                acked: Some((7_001, 0b1)),
                ..Default::default()
            },
        ] {
            assert_eq!(decode(&fb.encode()), Some(Dgram::Feedback(fb)));
        }
        // A shard list is bounded, and names its frame.
        let ask = |n: usize, frame: bool| {
            let f = Fields::new().u16(4, 1).when(frame, |f| f.u32(17, 9));
            let f = (0..n).fold(f, |f, i| f.u16(13, i as u16));
            match decode(&encode(DGRAM_FEEDBACK, &f.into_body())) {
                Some(Dgram::Feedback(fb)) => Some(fb),
                _ => None,
            }
        };
        assert!(ask(NACK_MAX, true).is_some());
        assert_eq!(ask(NACK_MAX + 1, true), None);
        assert_eq!(ask(2, false), None);
        // The clock kind stays reserved and nothing reads it.
        let probe = encode(DGRAM_CLOCK, &Fields::new().u64(1, 5).into_body());
        assert_eq!(decode(&probe), None);
    }

    proptest! {
        #[test]
        fn any_bytes_decode_or_drop(b in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = decode(&b);
        }
    }
}
