//! Every number on the `punktfunk/2` wire, in one file.
//!
//! Frame types, stream types, datagram kinds, feature bits and close codes are allocated here,
//! each with a doc line; a message's field tags live in the module named after it. Two branches
//! that take the same number then conflict in this file instead of on the wire. A number is
//! never reused for a second meaning: a peer that skips an unknown one cannot tell two apart.
//!
//! `tests::every_number_is_tabled` fails until a new constant is in its table, and
//! `tests::numbers_are_unique` until it differs from its neighbours.

/// ALPN of this wire. `punktfunk/1` is `pkf1`.
pub const ALPN: &[u8] = b"pkf2";

/// TLS exporter label for the media secrets; the context is the session id. 32 bytes out
/// feed [`crate::crypto::MediaKeys::derive`].
pub const MEDIA_EXPORTER_LABEL: &[u8] = b"EXPORTER-punktfunk/2 media";

/// Bound on the body of a frame whose type this build does not know. The reader skips it, but
/// holds it first, so the bound is what one unknown frame can make the peer buffer.
pub const UNKNOWN_MAX_BODY: usize = 64 * 1024;

// Frame types. Handshake 0x01–0x0F, pairing 0x10–0x1F, session control 0x20–0x3F,
// clipboard 0x40–0x4F, input stream 0x50–0x5F.

/// `client → host`, first frame: everything the client knows about itself and its ask.
pub const MSG_CLIENT_HELLO: u64 = 0x01;
/// `host → client`: the session — its id, the features in force, the first stream config.
pub const MSG_SERVER_HELLO: u64 = 0x02;
/// `client → host`: decoder and reassembler exist; send the first frame.
pub const MSG_READY: u64 = 0x03;
/// `host → client`, before `ServerHello`: still deciding (console approval). Repeats.
pub const MSG_PENDING: u64 = 0x04;
/// `host → client`, instead of `ServerHello`: dial this address and port instead, pinned to
/// the certificate it names.
pub const MSG_REDIRECT: u64 = 0x05;
/// `host → client`: why the close that follows happens, as a code and a sentence.
pub const MSG_REFUSED: u64 = 0x06;

/// `client → host`: begin pairing (`PairRequest`).
pub const MSG_PAIR_REQUEST: u64 = 0x10;
/// `host → client`: SPAKE2 answer and confirmation (`PairChallenge`).
pub const MSG_PAIR_CHALLENGE: u64 = 0x11;
/// `client → host`: the client's confirmation (`PairProof`).
pub const MSG_PAIR_PROOF: u64 = 0x12;
/// `host → client`: ceremony outcome (`PairResult`).
pub const MSG_PAIR_RESULT: u64 = 0x13;
/// `host → client`: prove the paired device key on a carrier without client certificates.
pub const MSG_AUTH_CHALLENGE: u64 = 0x14;
/// `client → host`: the device key and its signature over the challenge.
pub const MSG_AUTH_RESPONSE: u64 = 0x15;

/// `client → host`: switch display mode without reconnecting.
pub const MSG_RECONFIGURE: u64 = 0x20;
/// `host → client`: the answer to a `Reconfigure`. Accepted means the epoch that delivers it follows.
pub const MSG_RECONFIGURED: u64 = 0x21;
/// `host → client`: the video stream's config from epoch `n` on, before its first packet.
pub const MSG_STREAM_CONFIG: u64 = 0x22;
/// `host → client`: one audio stream's config from epoch `n` on, before its first datagram.
pub const MSG_AUDIO_CONFIG: u64 = 0x23;
/// `client → host`: retarget the encoder rate.
pub const MSG_SET_BITRATE: u64 = 0x24;
/// `host → client`: the rate in force, and why it is short of the ask.
pub const MSG_BITRATE_CHANGED: u64 = 0x25;
/// `host → client`: the host rebuilt its pipeline and nothing flowed for a while.
pub const MSG_PIPELINE_GAP: u64 = 0x26;
/// `client → host`: the rate the bring-up ramp proved.
pub const MSG_LINK_REPORT: u64 = 0x27;
/// `client → host`: stream under this delivery profile.
pub const MSG_SET_DELIVERY: u64 = 0x28;
/// `host → client`: the delivery profile in force.
pub const MSG_DELIVERY_CHANGED: u64 = 0x29;
/// `host → client`: what the host knows about its end of the path.
pub const MSG_HOST_FACTS: u64 = 0x2A;
/// `client → host`: a bandwidth probe, smooth or shaped.
pub const MSG_PROBE_REQUEST: u64 = 0x2B;
/// `host → client`: the probe's counts.
pub const MSG_PROBE_RESULT: u64 = 0x2C;
/// `client → host`: the display-latch grid for phase lock.
pub const MSG_PHASE_REPORT: u64 = 0x2D;
/// `client → host`: shard loss parity repaired over a report window (`LossReport`).
pub const MSG_LOSS_REPORT: u64 = 0x2E;
/// `client → host`: media packets received this session (`DeliveryReport`).
pub const MSG_DELIVERY_REPORT: u64 = 0x2F;
/// `host → client`: the pointer bitmap changed.
pub const MSG_CURSOR_SHAPE: u64 = 0x30;
/// `host → client`: who draws the cursor.
pub const MSG_CURSOR_RENDER: u64 = 0x31;
/// `host → client`: grants or remaining lifetime changed.
pub const MSG_ACCESS_UPDATE: u64 = 0x32;
/// `host → client`: the operator muted or unmuted this session.
pub const MSG_AUDIO_STATE: u64 = 0x33;
/// `host → client`: how the session's library launch turned out.
pub const MSG_LAUNCH_OUTCOME: u64 = 0x34;
/// `host → client`: the OS pad slots this session holds.
pub const MSG_PAD_SLOTS: u64 = 0x35;
/// `client → host`: invalidate a frame range instead of a keyframe (`RfiRequest`).
pub const MSG_RFI_REQUEST: u64 = 0x36;
/// `client → host`: the next frame as a keyframe (`RequestKeyframe`).
pub const MSG_REQUEST_KEYFRAME: u64 = 0x37;
/// `host → client`: the sealed shard size changes (`ShardPayloadChanged`).
pub const MSG_SHARD_PAYLOAD_CHANGED: u64 = 0x38;
/// `client → host`: the answer to a shard size change (`ShardPayloadAck`).
pub const MSG_SHARD_PAYLOAD_ACK: u64 = 0x39;
/// `client → host`: one clock skew round (`ClockProbe`).
pub const MSG_CLOCK_PROBE: u64 = 0x3A;
/// `host → client`: the answer to a clock round (`ClockEcho`).
pub const MSG_CLOCK_ECHO: u64 = 0x3B;

/// `client → host`, control stream: turn the shared clipboard on or off.
pub const MSG_CLIP_CONTROL: u64 = 0x40;
/// `host → client`, control stream: clipboard state and policy.
pub const MSG_CLIP_STATE: u64 = 0x41;
/// Both ways, control stream: the formats on offer, no bytes.
pub const MSG_CLIP_OFFER: u64 = 0x42;
/// Transfer stream, first frame: which format of which offer to pull.
pub const MSG_CLIP_FETCH: u64 = 0x43;
/// Transfer stream, answer: status and size, then the bytes until FIN.
pub const MSG_CLIP_FETCH_HDR: u64 = 0x44;

/// Control stream: one input edge — a key press or release — that must not be lost, in order
/// with the others. The body is the event's own encoding.
pub const MSG_INPUT_EVENT: u64 = 0x50;

/// Every frame type, its name and the bound on its body.
pub const FRAMES: &[(&str, u64, usize)] = &[
    ("MSG_CLIENT_HELLO", MSG_CLIENT_HELLO, 16 * 1024),
    ("MSG_SERVER_HELLO", MSG_SERVER_HELLO, 16 * 1024),
    ("MSG_READY", MSG_READY, 256),
    ("MSG_PENDING", MSG_PENDING, 1024),
    ("MSG_REDIRECT", MSG_REDIRECT, 1024),
    ("MSG_REFUSED", MSG_REFUSED, 1024),
    ("MSG_PAIR_REQUEST", MSG_PAIR_REQUEST, 4096),
    ("MSG_PAIR_CHALLENGE", MSG_PAIR_CHALLENGE, 1024),
    ("MSG_PAIR_PROOF", MSG_PAIR_PROOF, 256),
    ("MSG_PAIR_RESULT", MSG_PAIR_RESULT, 256),
    ("MSG_AUTH_CHALLENGE", MSG_AUTH_CHALLENGE, 256),
    ("MSG_AUTH_RESPONSE", MSG_AUTH_RESPONSE, 1024),
    ("MSG_RECONFIGURE", MSG_RECONFIGURE, 1024),
    ("MSG_RECONFIGURED", MSG_RECONFIGURED, 1024),
    ("MSG_STREAM_CONFIG", MSG_STREAM_CONFIG, 4096),
    ("MSG_AUDIO_CONFIG", MSG_AUDIO_CONFIG, 1024),
    ("MSG_SET_BITRATE", MSG_SET_BITRATE, 256),
    ("MSG_BITRATE_CHANGED", MSG_BITRATE_CHANGED, 256),
    ("MSG_PIPELINE_GAP", MSG_PIPELINE_GAP, 256),
    ("MSG_LINK_REPORT", MSG_LINK_REPORT, 256),
    ("MSG_SET_DELIVERY", MSG_SET_DELIVERY, 256),
    ("MSG_DELIVERY_CHANGED", MSG_DELIVERY_CHANGED, 256),
    ("MSG_HOST_FACTS", MSG_HOST_FACTS, 256),
    ("MSG_PROBE_REQUEST", MSG_PROBE_REQUEST, 256),
    ("MSG_PROBE_RESULT", MSG_PROBE_RESULT, 256),
    ("MSG_PHASE_REPORT", MSG_PHASE_REPORT, 256),
    ("MSG_LOSS_REPORT", MSG_LOSS_REPORT, 256),
    ("MSG_DELIVERY_REPORT", MSG_DELIVERY_REPORT, 256),
    ("MSG_CURSOR_SHAPE", MSG_CURSOR_SHAPE, 128 * 1024),
    ("MSG_CURSOR_RENDER", MSG_CURSOR_RENDER, 256),
    ("MSG_ACCESS_UPDATE", MSG_ACCESS_UPDATE, 256),
    ("MSG_AUDIO_STATE", MSG_AUDIO_STATE, 256),
    ("MSG_LAUNCH_OUTCOME", MSG_LAUNCH_OUTCOME, 1024),
    ("MSG_PAD_SLOTS", MSG_PAD_SLOTS, 256),
    ("MSG_RFI_REQUEST", MSG_RFI_REQUEST, 256),
    ("MSG_REQUEST_KEYFRAME", MSG_REQUEST_KEYFRAME, 256),
    ("MSG_SHARD_PAYLOAD_CHANGED", MSG_SHARD_PAYLOAD_CHANGED, 256),
    ("MSG_SHARD_PAYLOAD_ACK", MSG_SHARD_PAYLOAD_ACK, 256),
    ("MSG_CLOCK_PROBE", MSG_CLOCK_PROBE, 256),
    ("MSG_CLOCK_ECHO", MSG_CLOCK_ECHO, 256),
    ("MSG_CLIP_CONTROL", MSG_CLIP_CONTROL, 256),
    ("MSG_CLIP_STATE", MSG_CLIP_STATE, 256),
    ("MSG_CLIP_OFFER", MSG_CLIP_OFFER, 8 * 1024),
    ("MSG_CLIP_FETCH", MSG_CLIP_FETCH, 1024),
    ("MSG_CLIP_FETCH_HDR", MSG_CLIP_FETCH_HDR, 256),
    ("MSG_INPUT_EVENT", MSG_INPUT_EVENT, 256),
];

/// The bound on a body of type `ty`; [`UNKNOWN_MAX_BODY`] for a type this build does not know.
pub fn max_body(ty: u64) -> usize {
    FRAMES
        .iter()
        .find(|(_, t, _)| *t == ty)
        .map_or(UNKNOWN_MAX_BODY, |&(_, _, bound)| bound)
}

// Stream types: the first varint on every QUIC stream. A stream of a type this build does not
// know is stopped with [`STOP_UNKNOWN_STREAM`], never read as the session ending.

/// Bidirectional, opened by the client first: handshake, config, state.
pub const STREAM_CONTROL: u64 = 0x00;
/// Client → host, unidirectional: reserved for input edges. This release sends them on the
/// control stream as [`MSG_INPUT_EVENT`].
pub const STREAM_INPUT: u64 = 0x01;
/// Bidirectional, one per transfer: clipboard formats and files.
pub const STREAM_TRANSFER: u64 = 0x02;
/// Bidirectional, one per request: the management API.
pub const STREAM_MANAGEMENT: u64 = 0x03;

// Datagram kinds: the first varint of every QUIC (and WebTransport) datagram. An unknown kind
// is dropped and counted.

/// A media packet, on a carrier that has no raw UDP path (WebTransport).
pub const DGRAM_MEDIA: u64 = 0x00;
/// Audio of any stream: desktop, mic, a pad's haptics or speaker.
pub const DGRAM_AUDIO: u64 = 0x01;
/// Client → host input state: motion, sticks, scroll, pen, touch moves. Newest wins by `seq`.
pub const DGRAM_INPUT_STATE: u64 = 0x02;
/// Client → host receive state, repeated until cleared.
pub const DGRAM_FEEDBACK: u64 = 0x03;
/// Clock probe and echo.
pub const DGRAM_CLOCK: u64 = 0x04;
/// Host → client rumble.
pub const DGRAM_RUMBLE: u64 = 0x05;
/// Host → client HID output (lights, triggers, raw reports).
pub const DGRAM_HID_OUTPUT: u64 = 0x06;
/// Host → client cursor position and visibility.
pub const DGRAM_CURSOR_STATE: u64 = 0x07;
/// Host → client per-frame host timing.
pub const DGRAM_HOST_TIMING: u64 = 0x08;
/// Host → client HDR mastering metadata.
pub const DGRAM_HDR_META: u64 = 0x09;

/// Stream stop code for a stream type this build does not know.
pub const STOP_UNKNOWN_STREAM: u32 = 0x71;

// Feature bits 0–31 are the four `punktfunk/1` capability bytes in place, so core maps between
// the wires exactly: `video_caps` 0–7, `client_caps` 8–15, `host_caps` 16–23, `host_caps2`
// 24–31. A feature new to this wire takes the next free bit from 32 and a constant here.

/// First bit of `punktfunk/1`'s `video_caps`.
pub const FEATURE_V1_VIDEO_CAPS: u32 = 0;
/// First bit of `punktfunk/1`'s `client_caps`.
pub const FEATURE_V1_CLIENT_CAPS: u32 = 8;
/// First bit of `punktfunk/1`'s `host_caps`.
pub const FEATURE_V1_HOST_CAPS: u32 = 16;
/// First bit of `punktfunk/1`'s `host_caps2`.
pub const FEATURE_V1_HOST_CAPS2: u32 = 24;
/// Both ends: the host sends a `StreamConfig` for every epoch, and the client moves its mode
/// at that epoch's first frame. `Reconfigured` then only says a switch was accepted.
pub const FEATURE_STREAM_CONFIG: u32 = 32;
/// Both ends: the client reads `ServerHello`'s profile echo and follows a `Redirect`, and the
/// host resolves `ClientHello`'s profile. A host redirects only a client that sets it.
pub const FEATURE_PROFILES: u32 = 33;

#[cfg(test)]
mod tests {
    use super::*;

    const STREAMS: &[(&str, u64)] = &[
        ("STREAM_CONTROL", STREAM_CONTROL),
        ("STREAM_INPUT", STREAM_INPUT),
        ("STREAM_TRANSFER", STREAM_TRANSFER),
        ("STREAM_MANAGEMENT", STREAM_MANAGEMENT),
    ];

    const DGRAMS: &[(&str, u64)] = &[
        ("DGRAM_MEDIA", DGRAM_MEDIA),
        ("DGRAM_AUDIO", DGRAM_AUDIO),
        ("DGRAM_INPUT_STATE", DGRAM_INPUT_STATE),
        ("DGRAM_FEEDBACK", DGRAM_FEEDBACK),
        ("DGRAM_CLOCK", DGRAM_CLOCK),
        ("DGRAM_RUMBLE", DGRAM_RUMBLE),
        ("DGRAM_HID_OUTPUT", DGRAM_HID_OUTPUT),
        ("DGRAM_CURSOR_STATE", DGRAM_CURSOR_STATE),
        ("DGRAM_HOST_TIMING", DGRAM_HOST_TIMING),
        ("DGRAM_HDR_META", DGRAM_HDR_META),
    ];

    /// Bit positions; the four `punktfunk/1` bytes are 8 wide each.
    const FEATURES: &[(&str, u32)] = &[
        ("FEATURE_V1_VIDEO_CAPS", FEATURE_V1_VIDEO_CAPS),
        ("FEATURE_V1_CLIENT_CAPS", FEATURE_V1_CLIENT_CAPS),
        ("FEATURE_V1_HOST_CAPS", FEATURE_V1_HOST_CAPS),
        ("FEATURE_V1_HOST_CAPS2", FEATURE_V1_HOST_CAPS2),
        ("FEATURE_STREAM_CONFIG", FEATURE_STREAM_CONFIG),
        ("FEATURE_PROFILES", FEATURE_PROFILES),
    ];

    /// Every close and stop code either wire uses: v1's live on, and v2 adds its own.
    const CODES: &[(&str, u32)] = &[
        (
            "REJECT_BUSY_CLOSE_CODE",
            crate::reject::REJECT_BUSY_CLOSE_CODE,
        ),
        ("QUIT_CLOSE_CODE", crate::quic::QUIT_CLOSE_CODE),
        ("APP_EXITED_CLOSE_CODE", crate::quic::APP_EXITED_CLOSE_CODE),
        (
            "PAIR_NOT_ARMED_CLOSE_CODE",
            crate::reject::PAIR_NOT_ARMED_CLOSE_CODE,
        ),
        (
            "PAIR_BOUND_OTHER_CLOSE_CODE",
            crate::reject::PAIR_BOUND_OTHER_CLOSE_CODE,
        ),
        (
            "PAIR_RATE_LIMITED_CLOSE_CODE",
            crate::reject::PAIR_RATE_LIMITED_CLOSE_CODE,
        ),
        (
            "PAIR_NO_IDENTITY_CLOSE_CODE",
            crate::reject::PAIR_NO_IDENTITY_CLOSE_CODE,
        ),
        (
            "PAIR_DENIED_CLOSE_CODE",
            crate::reject::PAIR_DENIED_CLOSE_CODE,
        ),
        (
            "PAIR_APPROVAL_TIMEOUT_CLOSE_CODE",
            crate::reject::PAIR_APPROVAL_TIMEOUT_CLOSE_CODE,
        ),
        (
            "PAIR_SUPERSEDED_CLOSE_CODE",
            crate::reject::PAIR_SUPERSEDED_CLOSE_CODE,
        ),
        (
            "WIRE_VERSION_CLOSE_CODE",
            crate::reject::WIRE_VERSION_CLOSE_CODE,
        ),
        (
            "SETUP_FAILED_CLOSE_CODE",
            crate::reject::SETUP_FAILED_CLOSE_CODE,
        ),
        (
            "ACCESS_EXPIRED_CLOSE_CODE",
            crate::reject::ACCESS_EXPIRED_CLOSE_CODE,
        ),
        (
            "LAUNCH_NOT_PERMITTED_CLOSE_CODE",
            crate::reject::LAUNCH_NOT_PERMITTED_CLOSE_CODE,
        ),
        (
            "HOST_POWER_CLOSE_CODE",
            crate::reject::HOST_POWER_CLOSE_CODE,
        ),
        (
            "PROFILE_UNKNOWN_CLOSE_CODE",
            crate::reject::PROFILE_UNKNOWN_CLOSE_CODE,
        ),
        ("NO_SEAT_CLOSE_CODE", crate::reject::NO_SEAT_CLOSE_CODE),
        (
            "SEAT_OCCUPIED_CLOSE_CODE",
            crate::reject::SEAT_OCCUPIED_CLOSE_CODE,
        ),
        (
            "SEAT_UNAVAILABLE_CLOSE_CODE",
            crate::reject::SEAT_UNAVAILABLE_CLOSE_CODE,
        ),
        ("CLIP_CANCELLED_CODE", 0x70),
        ("STOP_UNKNOWN_STREAM", STOP_UNKNOWN_STREAM),
    ];

    fn assert_unique<T: PartialEq + std::fmt::Debug + Copy>(
        what: &str,
        xs: impl Iterator<Item = (&'static str, T)>,
    ) {
        let xs: Vec<_> = xs.collect();
        for (i, (name, v)) in xs.iter().enumerate() {
            if let Some((other, _)) = xs[..i].iter().find(|(_, w)| w == v) {
                panic!("{what}: {name} and {other} share {v:?}");
            }
        }
    }

    #[test]
    fn numbers_are_unique() {
        assert_unique("frame type", FRAMES.iter().map(|&(n, t, _)| (n, t)));
        assert_unique("stream type", STREAMS.iter().copied());
        assert_unique("datagram kind", DGRAMS.iter().copied());
        assert_unique("close code", CODES.iter().copied());
        assert_unique("feature bit", FEATURES.iter().copied());
        // A bit of its own never lands inside one of the four capability bytes.
        assert!(FEATURES
            .iter()
            .filter(|(n, _)| !n.starts_with("FEATURE_V1_"))
            .all(|&(_, bit)| (32..128).contains(&bit)));
    }

    #[test]
    fn every_number_is_tabled() {
        for line in include_str!("registry.rs").lines() {
            let Some(name) = line
                .strip_prefix("pub const ")
                .and_then(|r| r.split(':').next())
            else {
                continue;
            };
            let tabled = if name.starts_with("MSG_") {
                FRAMES.iter().any(|(n, _, _)| *n == name)
            } else if name.starts_with("STREAM_") {
                STREAMS.iter().any(|(n, _)| *n == name)
            } else if name.starts_with("DGRAM_") {
                DGRAMS.iter().any(|(n, _)| *n == name)
            } else if name.starts_with("STOP_") {
                CODES.iter().any(|(n, _)| *n == name)
            } else if name.starts_with("FEATURE_") {
                FEATURES.iter().any(|(n, _)| *n == name)
            } else {
                true
            };
            assert!(tabled, "{name} is missing from its registry table");
        }
    }

    #[test]
    fn bounds_are_set_and_unknown_types_get_the_default() {
        assert_eq!(max_body(MSG_CURSOR_SHAPE), 128 * 1024);
        assert_eq!(max_body(0x3FFF), UNKNOWN_MAX_BODY);
        assert!(FRAMES.iter().all(|&(_, _, b)| b >= 256));
    }
}
