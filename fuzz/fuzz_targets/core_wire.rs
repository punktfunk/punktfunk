//! Every decoder punktfunk-core runs on a peer's bytes, behind one libFuzzer target.
//!
//! `data[0]` picks the arm and the rest is the wire. The crate is `deny(unsafe_code)`, so a
//! finding is a panic, which is a remote stop of a network-facing host and not a memory bug.
//! A decoder with an encoder is also held to it: what decodes must encode to bytes that
//! decode and encode the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use punktfunk_core::input::InputEvent;
use punktfunk_core::quic::v2::dgram::{self, Dgram, Feedback, RetiredFeedback};
use punktfunk_core::quic::v2::features::FeatureSet;
use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
use punktfunk_core::quic::v2::msg::{
    self, AudioConfig, Pending, ProfileUnlock, Redirect, ScreenLayout, Screens, StreamConfig,
    V2Message,
};
use punktfunk_core::quic::v2::{field, registry as reg};
use punktfunk_core::quic::*;
use punktfunk_core::{discovery, fp, packet, wol};

/// What decodes must encode to bytes that decode and encode the same.
fn settle<T>(b: &[u8], dec: impl Fn(&[u8]) -> Option<T>, enc: impl Fn(&T) -> Vec<u8>) {
    let Some(v) = dec(b) else { return };
    let wire = enc(&v);
    let again = dec(&wire).expect("a decoder refuses its own encoding");
    assert_eq!(enc(&again), wire, "an encoding does not settle");
}

macro_rules! frames {
    ($($m:ty),* $(,)?) => {
        const TYPED: &[u64] = &[$(<$m as V2Message>::TYPE),*];

        /// One frame's body, as the message its type names.
        fn frame(ty: u64, body: &[u8]) {
            $(if ty == <$m as V2Message>::TYPE {
                settle(body, |b| <$m>::from_body(b).ok(), |m: &$m| m.fields().into_body());
            })*
        }
    };
}

frames!(
    ClientHello,
    ServerHello,
    Ready,
    Pending,
    Redirect,
    Refused,
    ProfileUnlock,
    PairRequest,
    PairChallenge,
    PairProof,
    PairResult,
    AuthChallenge,
    AuthResponse,
    Reconfigure,
    Reconfigured,
    StreamConfig,
    AudioConfig,
    SetBitrate,
    BitrateChanged,
    PipelineGap,
    ScreenLayout,
    Screens,
    ProbeShaped,
    ProbeResult,
    CursorShape,
    CursorRenderMode,
    AccessUpdate,
    AudioState,
    LaunchOutcome,
    PadSlots,
    ShardPayloadChanged,
    ShardPayloadAck,
    ClockProbe,
    ClockEcho,
    ClipControl,
    ClipState,
    ClipOffer,
    ClipFetch,
    ClipFetchHdr,
    PadIdentity,
    PadFeature,
);

// A frame type the registry gains fails this build until `frames!` names its message. The
// input event is its own arm below, and the retired phase report has no reader.
const _: () = {
    let mut i = 0;
    while i < reg::FRAMES.len() {
        let ty = reg::FRAMES[i].1;
        let mut known = ty == reg::MSG_INPUT_EVENT || ty == reg::MSG_PHASE_REPORT;
        let mut j = 0;
        while j < TYPED.len() {
            known |= TYPED[j] == ty;
            j += 1;
        }
        assert!(
            known,
            "a frame type in reg::FRAMES has no message in frames!"
        );
        i += 1;
    }
};

/// The payload of an audio, input or host-event datagram. Each decoder checks its own tag.
fn payload(b: &[u8]) {
    let owned = |(seq, pts, opus): (u32, u64, &[u8])| (seq, pts, opus.to_vec());
    let part =
        |(seq, pts, i, n, bytes): (u32, u64, u8, u8, &[u8])| (seq, pts, i, n, bytes.to_vec());
    settle(
        b,
        |b| decode_audio_datagram(b).map(owned),
        |(s, p, o)| encode_audio_datagram(*s, *p, o),
    );
    settle(
        b,
        |b| decode_audio_pcm_datagram(b).map(owned),
        |(s, p, o)| encode_audio_pcm_datagram(*s, *p, o),
    );
    settle(
        b,
        |b| decode_audio_bitstream_datagram(b).map(part),
        |(s, p, i, n, x)| encode_audio_bitstream_datagram(*s, *p, *i, *n, x),
    );
    settle(
        b,
        |b| decode_audio_pcm_part_datagram(b).map(part),
        |(s, p, i, n, x)| encode_audio_pcm_part_datagram(*s, *p, *i, *n, x),
    );
    settle(
        b,
        |b| decode_mic_datagram(b).map(owned),
        |(s, p, o)| encode_mic_datagram(*s, *p, o),
    );
    settle(
        b,
        |b| {
            decode_audio_red_datagram(b)
                .map(|(s, p, o, r)| (s, p, o.to_vec(), r.map(<[u8]>::to_vec)))
        },
        |(s, p, o, r)| encode_audio_red_datagram(*s, *p, o, r.as_deref().unwrap_or(&[])),
    );
    settle(b, decode_pad_audio_datagram, |f| {
        encode_pad_audio_datagram(f.pad, f.kind, f.seq, f.pts_ns, &f.opus)
    });
    settle(b, decode_rumble_datagram, |&(pad, low, high)| {
        encode_rumble_datagram(pad, low, high).to_vec()
    });
    let _ = decode_rumble_envelope(b);
    settle(b, RichInput::decode, RichInput::encode);
    settle(b, TouchpadFrame::decode, TouchpadFrame::encode);
    settle(b, HidOutput::decode, HidOutput::encode);
    settle(b, decode_hdr_meta_datagram, encode_hdr_meta_datagram);
    settle(b, decode_host_timing_datagram, encode_host_timing_datagram);
    settle(
        b,
        decode_cursor_state_datagram,
        encode_cursor_state_datagram,
    );
    settle(b, PenBatch::decode, PenBatch::encode);
    settle(b, InputEvent::decode, |e| e.encode().to_vec());
}

/// A media packet header, then the same one against the capture time the first left behind.
fn media(b: &[u8]) {
    let mut newest = None;
    let _ = packet::decode_v2(b, 0, &mut newest);
    let _ = packet::decode_v2(b, u64::MAX, &mut newest);
}

fuzz_target!(|data: &[u8]| {
    let Some((&arm, b)) = data.split_first() else {
        return;
    };
    match arm % 6 {
        // A control, pairing or input stream: frames back to back.
        0 => {
            let (mut rest, mut retired) = (b, RetiredFeedback::default());
            while let Ok(Some((ty, body, n))) = field::split_frame(rest, reg::max_body) {
                frame(ty, body);
                settle(
                    body,
                    |b| msg::decode_input_event(ty, b),
                    |e| e.encode().to_vec(),
                );
                let _ = retired.read(ty, body);
                rest = &rest[n..];
            }
        }
        // One datagram, and whatever its kind wraps.
        1 => {
            let fed = |b: &[u8]| match dgram::decode(b) {
                Some(Dgram::Feedback(f)) => Some(f),
                _ => None,
            };
            settle(b, fed, Feedback::encode);
            match dgram::decode(b) {
                Some(Dgram::Audio(p) | Dgram::InputState(p) | Dgram::HostEvent(p)) => payload(p),
                Some(Dgram::Media(p)) => media(p),
                _ => {}
            }
        }
        // The same payloads and the media header on a carrier that does not wrap them.
        2 => {
            payload(b);
            media(b);
        }
        // Values a hello field or a pad identity carries.
        3 => {
            settle(b, |b| Some(LinkFacts::decode(b)), |l| l.encode().to_vec());
            settle(b, SessionPreset::decode, SessionPreset::encode);
            settle(b, |b| Some(FeatureSet::decode(b)), |f| f.encode());
            settle(b, unpack_identity_replies, |v| {
                pack_identity_replies(
                    v.iter()
                        .map(|(id, reply)| (id.as_slice(), reply.as_slice())),
                )
            });
        }
        // The pairing ceremony an older client still speaks.
        4 => {
            settle(
                b,
                |b| PairRequest::decode_pkf1(b).ok(),
                PairRequest::encode_pkf1,
            );
            settle(
                b,
                |b| PairChallenge::decode_pkf1(b).ok(),
                PairChallenge::encode_pkf1,
            );
            settle(
                b,
                |b| PairProof::decode_pkf1(b).ok(),
                PairProof::encode_pkf1,
            );
            settle(
                b,
                |b| PairResult::decode_pkf1(b).ok(),
                PairResult::encode_pkf1,
            );
            settle(b, |b| Refused::decode_pkf1(b).ok(), Refused::encode_pkf1);
        }
        // Text off the LAN: an mDNS advert's TXT values, a fingerprint, a MAC.
        _ => {
            let Ok(s) = std::str::from_utf8(b) else {
                return;
            };
            let _ = fp::parse_hex32(s);
            let _ = wol::parse_mac(s);
            let lines: Vec<&str> = s.lines().collect();
            let txt = |key: &str| lines.get(key.len() % lines.len().max(1)).copied();
            let _ = discovery::advert_from_txt(s, 47989, &[std::net::Ipv4Addr::LOCALHOST], txt);
        }
    }
});
