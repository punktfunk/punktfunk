//! The post-handshake messages on the v2 wire: the same structs as `punktfunk/1`, each a frame
//! of tagged fields ([`V2Message`]).
//!
//! A field the sender leaves out decodes to its type's zero, so a message gains a field by
//! taking a new tag and an older reader skips it. Each message keeps `punktfunk/1`'s bounds on
//! what it holds (name and reason lengths, cursor size, clipboard kinds), checked on decode.
//! Tags are per message and, like every number on this wire, never reused.

use super::field::*;
use super::registry as reg;
use crate::config::Mode;
use crate::error::{PunktfunkError, Result};
use crate::quic::*;

/// One v2 control or pairing message.
pub trait V2Message: Sized {
    const TYPE: u64;
    fn fields(&self) -> Fields;
    fn from_body(body: &[u8]) -> Result<Self>;

    /// The whole frame: `type ‖ len ‖ fields`. Not `encode`: the structs keep
    /// `punktfunk/1`'s inherent `encode` until that wire is removed.
    fn encode_v2(&self) -> Vec<u8> {
        self.fields().frame(Self::TYPE)
    }
}

/// How one struct field crosses the v2 wire. Absent decodes to `Default`.
pub trait FieldValue: Sized + Default {
    fn put(&self, f: Fields, tag: u64) -> Fields;
    fn get(v: &[u8]) -> Result<Self>;
}

impl FieldValue for u8 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u8(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u8_of(v)
    }
}

impl FieldValue for u16 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u16(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u16_of(v)
    }
}

impl FieldValue for u32 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u32(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u32_of(v)
    }
}

impl FieldValue for u64 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u64(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u64_of(v)
    }
}

impl FieldValue for bool {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bool(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        bool_of(v)
    }
}

impl FieldValue for String {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.str(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        str_of(v, v.len()).map(str::to_string)
    }
}

impl FieldValue for Vec<u8> {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        Ok(v.to_vec())
    }
}

impl FieldValue for [u8; 32] {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        v.try_into()
            .map_err(|_| PunktfunkError::InvalidArg("32-byte field of another length"))
    }
}

/// `width u32 ‖ height u32 ‖ refresh_hz u32` as one 12-byte value.
impl FieldValue for Mode {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.width.to_le_bytes());
        b[4..8].copy_from_slice(&self.height.to_le_bytes());
        b[8..].copy_from_slice(&self.refresh_hz.to_le_bytes());
        f.bytes(tag, &b)
    }
    fn get(v: &[u8]) -> Result<Self> {
        let b: &[u8; 12] = v
            .try_into()
            .map_err(|_| PunktfunkError::InvalidArg("mode field of another length"))?;
        let at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Mode {
            width: at(0),
            height: at(4),
            refresh_hz: at(8),
        })
    }
}

impl FieldValue for LaunchOutcomeKind {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u8(tag, *self as u8)
    }
    fn get(v: &[u8]) -> Result<Self> {
        Ok(LaunchOutcomeKind::from_u8(u8_of(v)?))
    }
}

/// A [`V2Message`] for a plain struct: `tag => field` per member, then an optional check of
/// the decoded value. Every member's type is a [`FieldValue`].
macro_rules! v2_message {
    ($ty:ident = $id:expr, { $($tag:literal => $field:ident),* $(,)? } $(, check $check:expr)?) => {
        impl V2Message for $ty {
            const TYPE: u64 = $id;

            fn fields(&self) -> Fields {
                let f = Fields::new();
                $( let f = FieldValue::put(&self.$field, f, $tag); )*
                f
            }

            fn from_body(body: &[u8]) -> Result<Self> {
                $( let mut $field = None; )*
                let mut r = FieldReader::new(body);
                while let Some((tag, v)) = r.next_field()? {
                    match tag {
                        $( $tag => set_once(&mut $field, FieldValue::get(v)?)?, )*
                        _ => {}
                    }
                }
                let m = $ty { $( $field: $field.unwrap_or_default(), )* };
                $(
                    let check: fn(&$ty) -> bool = $check;
                    if !check(&m) {
                        return Err(PunktfunkError::InvalidArg(concat!("bad ", stringify!($ty))));
                    }
                )?
                Ok(m)
            }
        }
    };
}

v2_message!(PairRequest = reg::MSG_PAIR_REQUEST, { 1 => name, 2 => spake_a, 3 => device_key },
    check |m| m.name.len() <= HELLO_NAME_MAX);
v2_message!(PairChallenge = reg::MSG_PAIR_CHALLENGE, { 1 => spake_b, 2 => confirm });
v2_message!(PairProof = reg::MSG_PAIR_PROOF, { 1 => confirm });
v2_message!(PairResult = reg::MSG_PAIR_RESULT, { 1 => ok });
v2_message!(AuthChallenge = reg::MSG_AUTH_CHALLENGE, { 1 => nonce });
v2_message!(AuthResponse = reg::MSG_AUTH_RESPONSE, { 1 => device_key, 2 => signature });
v2_message!(Refused = reg::MSG_REFUSED, { 1 => code, 2 => reason },
    check |m| m.reason.len() <= REFUSED_REASON_MAX);

v2_message!(Reconfigure = reg::MSG_RECONFIGURE, { 1 => mode });
v2_message!(Reconfigured = reg::MSG_RECONFIGURED, { 1 => accepted, 2 => mode });
v2_message!(SetBitrate = reg::MSG_SET_BITRATE, { 1 => bitrate_kbps });
v2_message!(PipelineGap = reg::MSG_PIPELINE_GAP, { 1 => gap_ms });
v2_message!(LinkReport = reg::MSG_LINK_REPORT, { 1 => proven_kbps });
v2_message!(SetDelivery = reg::MSG_SET_DELIVERY, { 1 => profile });
v2_message!(DeliveryChanged = reg::MSG_DELIVERY_CHANGED, { 1 => profile, 2 => forced });
v2_message!(HostFacts = reg::MSG_HOST_FACTS,
    { 1 => iface_kind, 2 => link_mbps, 3 => sndbuf_kb, 4 => forced_profile });
v2_message!(ProbeShaped = reg::MSG_PROBE_REQUEST,
    { 1 => target_kbps, 2 => duration_ms, 3 => burst_hz, 4 => group_bytes, 5 => group_rate_kbps });
v2_message!(ProbeResult = reg::MSG_PROBE_RESULT,
    { 1 => bytes_sent, 2 => packets_sent, 3 => duration_ms, 4 => wire_packets_sent,
      5 => send_dropped });
v2_message!(PhaseReport = reg::MSG_PHASE_REPORT,
    { 1 => next_latch_host_ns, 2 => latch_period_ns, 3 => uncertainty_ns, 4 => arrival_lead_ns,
      5 => coherence_milli });
v2_message!(CursorShape = reg::MSG_CURSOR_SHAPE,
    { 1 => serial, 2 => w, 3 => h, 4 => hot_x, 5 => hot_y, 6 => rgba },
    check |m| m.w > 0 && m.h > 0 && m.w <= CURSOR_SHAPE_MAX_SIDE && m.h <= CURSOR_SHAPE_MAX_SIDE
        && m.rgba.len() == m.w as usize * m.h as usize * 4);
v2_message!(CursorRenderMode = reg::MSG_CURSOR_RENDER, { 1 => client_draws });
v2_message!(AccessUpdate = reg::MSG_ACCESS_UPDATE, { 1 => grants, 2 => remaining_secs });
v2_message!(AudioState = reg::MSG_AUDIO_STATE, { 1 => muted });
v2_message!(LaunchOutcome = reg::MSG_LAUNCH_OUTCOME, { 1 => kind, 2 => message },
    check |m| m.message.len() <= LAUNCH_MESSAGE_MAX);
v2_message!(PadSlots = reg::MSG_PAD_SLOTS, { 1 => slots });
v2_message!(ClipControl = reg::MSG_CLIP_CONTROL, { 1 => enabled, 2 => flags });
v2_message!(ClipState = reg::MSG_CLIP_STATE, { 1 => enabled, 2 => policy, 3 => reason });
v2_message!(ClipFetch = reg::MSG_CLIP_FETCH, { 1 => seq, 2 => file_index, 3 => mime },
    check |m| m.mime.len() <= CLIP_MAX_MIME);
v2_message!(ClipFetchHdr = reg::MSG_CLIP_FETCH_HDR, { 1 => status, 2 => total_size });

/// The rate in force (1) and, when short of the ask, why (2). A reason this build does not
/// know reads as none: the ack still stands.
impl V2Message for BitrateChanged {
    const TYPE: u64 = reg::MSG_BITRATE_CHANGED;

    fn fields(&self) -> Fields {
        Fields::new()
            .u32(1, self.bitrate_kbps)
            .when(self.reason.is_some(), |f| {
                f.u8(2, self.reason.map_or(0, AckReason::to_wire))
            })
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let (mut rate, mut reason) = (None, None);
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            match tag {
                1 => set_once(&mut rate, u32_of(v)?)?,
                2 => set_once(&mut reason, u8_of(v)?)?,
                _ => {}
            }
        }
        Ok(BitrateChanged {
            bitrate_kbps: rate.unwrap_or(0),
            reason: reason.and_then(AckReason::from_wire),
        })
    }
}

/// The offer's sequence (1) and one field per format (2, repeated): `mime ‖ size_hint u64`,
/// the mime's length implied by the field's.
impl V2Message for ClipOffer {
    const TYPE: u64 = reg::MSG_CLIP_OFFER;

    fn fields(&self) -> Fields {
        self.kinds
            .iter()
            .take(CLIP_MAX_KINDS)
            .fold(Fields::new().u32(1, self.seq), |f, k| {
                let mime = &k.mime.as_bytes()[..k.mime.len().min(CLIP_MAX_MIME)];
                let mut v = mime.to_vec();
                v.extend_from_slice(&k.size_hint.to_le_bytes());
                f.bytes(2, &v)
            })
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut seq = None;
        let mut kinds = Vec::new();
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            match tag {
                1 => set_once(&mut seq, u32_of(v)?)?,
                2 => {
                    if kinds.len() == CLIP_MAX_KINDS || v.len() < 8 || v.len() - 8 > CLIP_MAX_MIME {
                        return Err(PunktfunkError::InvalidArg("bad ClipOffer kind"));
                    }
                    let (mime, size) = v.split_at(v.len() - 8);
                    kinds.push(ClipKind {
                        mime: String::from_utf8_lossy(mime).into_owned(),
                        size_hint: u64::from_le_bytes(size.try_into().unwrap()),
                    });
                }
                _ => {}
            }
        }
        Ok(ClipOffer {
            seq: seq.unwrap_or(0),
            kinds,
        })
    }
}

/// `host → client`, before the first packet of epoch `epoch`: what the video stream is from
/// there on. The receiver holds frames of an epoch it has no config for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamConfig {
    pub epoch: u8,
    pub mode: Mode,
    /// One `CODEC_*` bit.
    pub codec: u8,
    pub bit_depth: u8,
    /// CICP `[primaries, transfer, matrix, full_range]`.
    pub color: [u8; 4],
    pub chroma_format: u8,
}

v2_message!(StreamConfig = reg::MSG_STREAM_CONFIG,
    { 1 => epoch, 2 => mode, 3 => codec, 4 => bit_depth, 5 => color, 6 => chroma_format });

/// `host → client`, before the first datagram of epoch `epoch` on audio stream `stream`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioConfig {
    pub stream: u8,
    pub epoch: u8,
    /// `AUDIO_CODEC_*`.
    pub codec: u8,
    pub rate_hz: u32,
    pub bits: u8,
    pub channels: u8,
    pub layout: u8,
    pub frame_us: u16,
    /// Each datagram carries the previous frame too.
    pub redundant: bool,
}

v2_message!(AudioConfig = reg::MSG_AUDIO_CONFIG,
    { 1 => stream, 2 => epoch, 3 => codec, 4 => rate_hz, 5 => bits, 6 => channels, 7 => layout,
      8 => frame_us, 9 => redundant });

impl FieldValue for [u8; 4] {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        v.try_into()
            .map_err(|_| PunktfunkError::InvalidArg("4-byte field of another length"))
    }
}

/// Decode a body of message type `M`, checking the frame type first.
pub fn decode<M: V2Message>(ty: u64, body: &[u8]) -> Result<M> {
    if ty != M::TYPE {
        return Err(PunktfunkError::InvalidArg("frame of another type"));
    }
    M::from_body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<M: V2Message + PartialEq + std::fmt::Debug>(m: M) {
        let wire = m.encode_v2();
        let (ty, body, n) = split_frame(&wire, reg::max_body).unwrap().unwrap();
        assert_eq!(n, wire.len());
        assert_eq!(decode::<M>(ty, body).unwrap(), m);
    }

    #[test]
    fn every_message_round_trips() {
        let mode = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        round_trip(PairRequest {
            name: "Living room TV".into(),
            spake_a: vec![1; 33],
            device_key: vec![2; 91],
        });
        round_trip(PairChallenge {
            spake_b: vec![3; 33],
            confirm: [4; 32],
        });
        round_trip(PairProof { confirm: [5; 32] });
        round_trip(PairResult { ok: true });
        round_trip(AuthChallenge { nonce: [6; 32] });
        round_trip(AuthResponse {
            device_key: vec![7; 91],
            signature: vec![8; 72],
        });
        round_trip(Refused {
            code: 0x67,
            reason: "update both".into(),
        });
        round_trip(Reconfigure { mode });
        round_trip(Reconfigured {
            accepted: true,
            mode,
        });
        round_trip(SetBitrate {
            bitrate_kbps: 80_000,
        });
        for reason in [None, Some(AckReason::Cadence), Some(AckReason::Pinned)] {
            round_trip(BitrateChanged {
                bitrate_kbps: 50_000,
                reason,
            });
        }
        round_trip(PipelineGap { gap_ms: 401 });
        round_trip(LinkReport { proven_kbps: 9 });
        round_trip(SetDelivery { profile: 2 });
        round_trip(DeliveryChanged {
            profile: 1,
            forced: true,
        });
        round_trip(HostFacts {
            iface_kind: 2,
            link_mbps: 1200,
            sndbuf_kb: 4096,
            forced_profile: FORCED_PROFILE_NONE,
        });
        round_trip(ProbeShaped {
            target_kbps: 100_000,
            duration_ms: 500,
            burst_hz: 60,
            group_bytes: 65_536,
            group_rate_kbps: 0,
        });
        round_trip(ProbeResult {
            bytes_sent: 1 << 33,
            packets_sent: 9,
            duration_ms: 500,
            wire_packets_sent: 9,
            send_dropped: 0,
        });
        round_trip(PhaseReport {
            next_latch_host_ns: 1_700_000_000_000_000_000,
            latch_period_ns: 8_333_333,
            uncertainty_ns: 400_000,
            arrival_lead_ns: 2_000_000,
            coherence_milli: 950,
        });
        round_trip(CursorShape {
            serial: 3,
            w: 2,
            h: 3,
            hot_x: 1,
            hot_y: 1,
            rgba: vec![0xAB; 24],
        });
        round_trip(CursorRenderMode { client_draws: true });
        round_trip(AccessUpdate {
            grants: GRANT_ALL,
            remaining_secs: 60,
        });
        round_trip(AudioState { muted: true });
        round_trip(LaunchOutcome::new(
            LaunchOutcomeKind::Failed,
            "Gone in a second.",
        ));
        round_trip(PadSlots { slots: 0b101 });
        round_trip(ClipControl {
            enabled: true,
            flags: CLIP_FLAG_FILES,
        });
        round_trip(ClipState {
            enabled: true,
            policy: CLIP_POLICY_TEXT,
            reason: CLIP_REASON_OK,
        });
        round_trip(ClipOffer {
            seq: 7,
            kinds: vec![
                ClipKind {
                    mime: "text/plain;charset=utf-8".into(),
                    size_hint: 12,
                },
                ClipKind {
                    mime: String::new(),
                    size_hint: 0,
                },
            ],
        });
        round_trip(ClipFetch {
            seq: 7,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "image/png".into(),
        });
        round_trip(ClipFetchHdr {
            status: CLIP_FETCH_OK,
            total_size: 1 << 40,
        });
        round_trip(StreamConfig {
            epoch: 4,
            mode,
            codec: CODEC_AV1,
            bit_depth: 10,
            color: [9, 16, 9, 0],
            chroma_format: CHROMA_IDC_420,
        });
        round_trip(AudioConfig {
            stream: 0,
            epoch: 1,
            codec: AUDIO_CODEC_PCM,
            rate_hz: 96_000,
            bits: 24,
            channels: 2,
            layout: 0,
            frame_us: 2000,
            redundant: false,
        });
    }

    /// The bytes on the wire are pinned: a change here is a wire change.
    #[test]
    fn golden_frames() {
        assert_eq!(
            SetBitrate {
                bitrate_kbps: 80_000
            }
            .encode_v2(),
            [0x24, 0x06, 0x01, 0x04, 0x80, 0x38, 0x01, 0x00]
        );
        assert_eq!(
            BitrateChanged {
                bitrate_kbps: 1,
                reason: Some(AckReason::Cadence)
            }
            .encode_v2(),
            [0x25, 0x09, 0x01, 0x04, 0x01, 0x00, 0x00, 0x00, 0x02, 0x01, 0x02]
        );
        assert_eq!(
            PairResult { ok: true }.encode_v2(),
            [0x13, 0x03, 0x01, 0x01, 0x01]
        );
    }

    #[test]
    fn bounds_hold_on_decode() {
        let long = |n: usize| "x".repeat(n);
        let bad_name = PairRequest {
            name: long(HELLO_NAME_MAX + 1),
            spake_a: vec![],
            device_key: vec![],
        };
        assert!(PairRequest::from_body(&bad_name.fields().into_body()).is_err());
        let bad_reason = Refused {
            code: 1,
            reason: long(REFUSED_REASON_MAX + 1),
        };
        assert!(Refused::from_body(&bad_reason.fields().into_body()).is_err());
        let bad_cursor = CursorShape {
            serial: 1,
            w: CURSOR_SHAPE_MAX_SIDE + 1,
            h: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0; (CURSOR_SHAPE_MAX_SIDE as usize + 1) * 4],
        };
        assert!(CursorShape::from_body(&bad_cursor.fields().into_body()).is_err());
        let short_rgba = CursorShape {
            rgba: vec![0; 3],
            w: 1,
            ..bad_cursor
        };
        assert!(CursorShape::from_body(&short_rgba.fields().into_body()).is_err());
        let many = ClipOffer {
            seq: 1,
            kinds: vec![],
        }
        .fields();
        let many = (0..=CLIP_MAX_KINDS).fold(many, |f, _| f.bytes(2, &[0; 8]));
        assert!(ClipOffer::from_body(&many.into_body()).is_err());
        assert!(decode::<SetBitrate>(reg::MSG_LINK_REPORT, &[]).is_err());
    }

    /// An unknown tag is skipped, an unknown ack reason reads as none, a repeat is refused.
    #[test]
    fn unknown_tags_skip_and_repeats_refuse() {
        let body = Fields::new()
            .u32(1, 9)
            .str(77, "from a newer peer")
            .u8(2, 200)
            .into_body();
        assert_eq!(
            BitrateChanged::from_body(&body).unwrap(),
            BitrateChanged {
                bitrate_kbps: 9,
                reason: None
            }
        );
        let twice = Fields::new().u32(1, 1).u32(1, 2).into_body();
        assert!(SetBitrate::from_body(&twice).is_err());
    }
}
