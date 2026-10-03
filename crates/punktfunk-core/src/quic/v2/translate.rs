//! The seam between the v2 wire and session logic that speaks `punktfunk/1` messages. Every
//! v1 message has one v2 frame, and these functions convert at the stream's edge, so the host's
//! and client's control loops run unchanged on either wire until v1 is removed.
//!
//! The handshake changes shape on the way. `Hello` plus `Start`'s extension block is one
//! `ClientHello`, and `Start` itself is `Ready`: [`RxEdge`] keeps the entries a `ClientHello`
//! carried and hands them back inside the `Start` it builds from `Ready`. A host's `Welcome`
//! becomes a `ServerHello` with the session fields [`TxEdge::set_session`] gave it. A client
//! writes its own handshake frames and uses the same edges after it.

use super::clock::SessionClock;
use super::hello::{ClientHello, Ready, ServerHello};
use super::msg::{decode_input_event, encode_input_event, V2Message};
use super::registry as reg;
use crate::crypto::MediaSuite;
use crate::quic::*;

/// One v1 control or pairing message (`CTL_MAGIC ‖ type ‖ …`) as its v2 frame. `None` for bytes
/// no v2 frame carries; the edge drops them, as an older peer drops an unknown type.
pub fn frame_from_v1(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.len() < 5 || &msg[..4] != CTL_MAGIC {
        return None;
    }
    macro_rules! re {
        ($t:ty) => {
            <$t>::decode(msg).ok().map(|m| m.encode_v2())
        };
    }
    match msg[4] {
        MSG_RECONFIGURE => re!(Reconfigure),
        MSG_RECONFIGURED => re!(Reconfigured),
        MSG_REQUEST_KEYFRAME => re!(RequestKeyframe),
        MSG_LOSS_REPORT => re!(LossReport),
        MSG_SET_BITRATE => re!(SetBitrate),
        MSG_BITRATE_CHANGED => re!(BitrateChanged),
        MSG_RFI_REQUEST => re!(RfiRequest),
        MSG_SHARD_PAYLOAD_CHANGED => re!(ShardPayloadChanged),
        MSG_SHARD_PAYLOAD_ACK => re!(ShardPayloadAck),
        MSG_PIPELINE_GAP => re!(PipelineGap),
        MSG_DELIVERY_REPORT => re!(DeliveryReport),
        MSG_LINK_REPORT => re!(LinkReport),
        MSG_SET_DELIVERY => re!(SetDelivery),
        MSG_DELIVERY_CHANGED => re!(DeliveryChanged),
        MSG_HOST_FACTS => re!(HostFacts),
        MSG_PAIR_REQUEST => re!(PairRequest),
        MSG_PAIR_CHALLENGE => re!(PairChallenge),
        MSG_PAIR_PROOF => re!(PairProof),
        MSG_PAIR_RESULT => re!(PairResult),
        MSG_AUTH_CHALLENGE => re!(AuthChallenge),
        MSG_AUTH_RESPONSE => re!(AuthResponse),
        MSG_REFUSED => re!(Refused),
        MSG_PROBE_REQUEST => ProbeRequest::decode(msg)
            .ok()
            .map(|p| ProbeShaped::from(p).encode_v2()),
        MSG_PROBE_RESULT => re!(ProbeResult),
        MSG_PROBE_SHAPED => re!(ProbeShaped),
        MSG_CLOCK_PROBE => re!(ClockProbe),
        MSG_CLOCK_ECHO => re!(ClockEcho),
        MSG_PHASE_REPORT => re!(PhaseReport),
        MSG_CLIP_CONTROL => re!(ClipControl),
        MSG_CLIP_STATE => re!(ClipState),
        MSG_CLIP_OFFER => re!(ClipOffer),
        MSG_CLIP_FETCH => re!(ClipFetch),
        MSG_CLIP_FETCH_HDR => re!(ClipFetchHdr),
        MSG_CURSOR_SHAPE => re!(CursorShape),
        MSG_CURSOR_RENDER => re!(CursorRenderMode),
        MSG_ACCESS_UPDATE => re!(AccessUpdate),
        MSG_AUDIO_STATE => re!(AudioState),
        MSG_LAUNCH_OUTCOME => re!(LaunchOutcome),
        MSG_PAD_SLOTS => re!(PadSlots),
        MSG_INPUT_EDGE => InputEdge::decode(msg)
            .ok()
            .map(|e| encode_input_event(&e.0)),
        _ => None,
    }
}

/// One v2 frame as the v1 message bytes session logic reads. `None` for a type with no v1
/// form or a body that does not decode; the edge skips it.
pub fn v1_from_frame(ty: u64, body: &[u8]) -> Option<Vec<u8>> {
    macro_rules! back {
        ($t:ty) => {
            <$t>::from_body(body).ok().map(|m| m.encode())
        };
    }
    match ty {
        reg::MSG_RECONFIGURE => back!(Reconfigure),
        reg::MSG_RECONFIGURED => back!(Reconfigured),
        reg::MSG_REQUEST_KEYFRAME => back!(RequestKeyframe),
        reg::MSG_LOSS_REPORT => back!(LossReport),
        reg::MSG_SET_BITRATE => back!(SetBitrate),
        reg::MSG_BITRATE_CHANGED => back!(BitrateChanged),
        reg::MSG_RFI_REQUEST => back!(RfiRequest),
        reg::MSG_SHARD_PAYLOAD_CHANGED => back!(ShardPayloadChanged),
        reg::MSG_SHARD_PAYLOAD_ACK => back!(ShardPayloadAck),
        reg::MSG_PIPELINE_GAP => back!(PipelineGap),
        reg::MSG_DELIVERY_REPORT => back!(DeliveryReport),
        reg::MSG_LINK_REPORT => back!(LinkReport),
        reg::MSG_SET_DELIVERY => back!(SetDelivery),
        reg::MSG_DELIVERY_CHANGED => back!(DeliveryChanged),
        reg::MSG_HOST_FACTS => back!(HostFacts),
        reg::MSG_PAIR_REQUEST => back!(PairRequest),
        reg::MSG_PAIR_CHALLENGE => back!(PairChallenge),
        reg::MSG_PAIR_PROOF => back!(PairProof),
        reg::MSG_PAIR_RESULT => back!(PairResult),
        reg::MSG_AUTH_CHALLENGE => back!(AuthChallenge),
        reg::MSG_AUTH_RESPONSE => back!(AuthResponse),
        reg::MSG_REFUSED => back!(Refused),
        // A probe with no shape is the plain request an older host also reads.
        reg::MSG_PROBE_REQUEST => ProbeShaped::from_body(body).ok().map(|p| {
            if p.burst_hz == 0 && p.group_bytes == 0 && p.group_rate_kbps == 0 {
                ProbeRequest {
                    target_kbps: p.target_kbps,
                    duration_ms: p.duration_ms,
                }
                .encode()
            } else {
                p.encode()
            }
        }),
        reg::MSG_PROBE_RESULT => back!(ProbeResult),
        reg::MSG_CLOCK_PROBE => back!(ClockProbe),
        reg::MSG_CLOCK_ECHO => back!(ClockEcho),
        reg::MSG_PHASE_REPORT => back!(PhaseReport),
        reg::MSG_CLIP_CONTROL => back!(ClipControl),
        reg::MSG_CLIP_STATE => back!(ClipState),
        reg::MSG_CLIP_OFFER => back!(ClipOffer),
        reg::MSG_CLIP_FETCH => back!(ClipFetch),
        reg::MSG_CLIP_FETCH_HDR => back!(ClipFetchHdr),
        reg::MSG_CURSOR_SHAPE => back!(CursorShape),
        reg::MSG_CURSOR_RENDER => back!(CursorRenderMode),
        reg::MSG_ACCESS_UPDATE => back!(AccessUpdate),
        reg::MSG_AUDIO_STATE => back!(AudioState),
        reg::MSG_LAUNCH_OUTCOME => back!(LaunchOutcome),
        reg::MSG_PAD_SLOTS => back!(PadSlots),
        reg::MSG_INPUT_EVENT => decode_input_event(ty, body).map(|e| InputEdge(e).encode()),
        _ => None,
    }
}

/// What a `ClientHello` said beyond `Hello`, for the host to read after the `Hello` it became.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientExtra {
    pub start_ext: Vec<(u16, Vec<u8>)>,
    pub resume: Option<[u8; 16]>,
    pub suites: Vec<MediaSuite>,
}

/// A read edge: v2 frames in, v1 messages out.
#[derive(Debug, Default)]
pub struct RxEdge {
    /// Host side: set by the `ClientHello`; its entries return inside the `Start` that `Ready`
    /// becomes.
    pub client: Option<ClientExtra>,
    /// Client side: set by the `ServerHello` that arrives as `Welcome`.
    pub server: Option<SessionFields>,
    /// Host side: a `PhaseReport`'s latch arrives in wire time and leaves in host time.
    pub clock: Option<std::sync::Arc<SessionClock>>,
    /// Client side: a `Pending` arrived, so the host is still deciding on this device.
    pub pending: bool,
    /// A hello this build cannot read; the reader fails the stream instead of waiting on.
    pub bad_hello: Option<&'static str>,
}

impl RxEdge {
    pub fn to_v1(&mut self, ty: u64, body: &[u8]) -> Option<Vec<u8>> {
        match ty {
            reg::MSG_PENDING => {
                if !std::mem::replace(&mut self.pending, true) {
                    tracing::info!("the host is waiting for this device to be approved");
                }
                None
            }
            reg::MSG_PHASE_REPORT if self.clock.is_some() => {
                let mut pr = PhaseReport::from_body(body).ok()?;
                let clock = self.clock.as_ref()?;
                pr.next_latch_host_ns = clock.to_host(pr.next_latch_host_ns);
                Some(pr.encode())
            }
            reg::MSG_CLIENT_HELLO => {
                let Ok(ch) = ClientHello::from_body(body) else {
                    self.bad_hello = Some("unreadable ClientHello");
                    return None;
                };
                self.client = Some(ClientExtra {
                    start_ext: ch.start_ext,
                    resume: ch.resume,
                    suites: ch.suites,
                });
                Some(ch.hello.encode())
            }
            reg::MSG_READY => {
                Ready::from_body(body).ok()?;
                let entries = self
                    .client
                    .as_ref()
                    .map_or(Vec::new(), |c| c.start_ext.clone());
                let refs: Vec<(u16, &[u8])> =
                    entries.iter().map(|(t, v)| (*t, v.as_slice())).collect();
                Start { client_udp_port: 0 }.encode_ext(&refs).ok()
            }
            reg::MSG_SERVER_HELLO => {
                let Ok(sh) = ServerHello::from_body(body) else {
                    self.bad_hello = Some("unreadable ServerHello");
                    return None;
                };
                self.server = Some(SessionFields {
                    session_id: sh.session_id,
                    clock_origin_ns: sh.clock_origin_ns,
                    suite: sh.suite,
                });
                Some(sh.welcome.encode())
            }
            _ => v1_from_frame(ty, body),
        }
    }
}

/// The session fields a `ServerHello` carries beyond `Welcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionFields {
    pub session_id: [u8; 16],
    pub clock_origin_ns: u64,
    pub suite: Option<MediaSuite>,
}

/// A write edge: v1 messages in, v2 frames out.
#[derive(Debug, Default)]
pub struct TxEdge {
    /// Host side: what the `Welcome` goes out with.
    session: Option<SessionFields>,
    /// Client side: what the `Hello` goes out with. Its first `Hello`-magic message is the
    /// `Hello`, the next the `Start`.
    client: Option<ClientExtra>,
    hello_sent: bool,
    /// Host side: a `ClockEcho` leaves stamped in wire time, the clock its media carries.
    clock: Option<std::sync::Arc<SessionClock>>,
}

impl TxEdge {
    /// A client's edge: `Hello` leaves as a `ClientHello` carrying `extra`, and `Start` as
    /// `Ready`, its entries already sent.
    pub fn client(extra: ClientExtra) -> TxEdge {
        TxEdge {
            client: Some(extra),
            ..TxEdge::default()
        }
    }

    /// A host's edge: its clock echoes leave in `clock`'s time.
    pub fn host(clock: std::sync::Arc<SessionClock>) -> TxEdge {
        TxEdge {
            clock: Some(clock),
            ..TxEdge::default()
        }
    }

    /// The fields the next `Welcome` goes out with. Without them a `Welcome` is dropped: a
    /// `ServerHello` with no session id would be refused by the client anyway.
    pub fn set_session(&mut self, s: SessionFields) {
        self.session = Some(s);
    }

    pub fn to_v2(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        if msg.len() >= 4 && &msg[..4] == MAGIC {
            if let Some(extra) = &self.client {
                if std::mem::replace(&mut self.hello_sent, true) {
                    return Some(Ready {}.encode_v2());
                }
                return Some(
                    ClientHello {
                        hello: Hello::decode(msg).ok()?,
                        start_ext: extra.start_ext.clone(),
                        resume: extra.resume,
                        suites: extra.suites.clone(),
                    }
                    .encode_v2(),
                );
            }
            let s = self.session?;
            let welcome = Welcome::decode(msg).ok()?;
            return Some(
                ServerHello {
                    welcome,
                    session_id: s.session_id,
                    clock_origin_ns: s.clock_origin_ns,
                    suite: s.suite,
                }
                .encode_v2(),
            );
        }
        if let (Some(clock), Ok(echo)) = (&self.clock, ClockEcho::decode(msg)) {
            let echo = ClockEcho {
                t2_ns: clock.to_wire(echo.t2_ns),
                t3_ns: clock.to_wire(echo.t3_ns),
                ..echo
            };
            return frame_from_v1(&echo.encode());
        }
        frame_from_v1(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::quic::v2::field::split_frame;

    fn through(v1: Vec<u8>) -> Vec<u8> {
        let frame = frame_from_v1(&v1).unwrap_or_else(|| panic!("no frame for type {:#x}", v1[4]));
        let (ty, body, _) = split_frame(&frame, reg::max_body).unwrap().unwrap();
        v1_from_frame(ty, body).unwrap()
    }

    /// Every v1 message survives v1 → v2 → v1 byte for byte.
    #[test]
    fn every_v1_message_crosses_and_comes_back() {
        let mode = Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 165,
        };
        let ev = crate::input::InputEvent {
            kind: crate::input::InputKind::KeyDown,
            _pad: [0; 3],
            code: 30,
            x: 0,
            y: 0,
            flags: 1,
        };
        let all: Vec<Vec<u8>> = vec![
            Reconfigure { mode }.encode(),
            Reconfigured {
                accepted: false,
                mode,
            }
            .encode(),
            RequestKeyframe.encode(),
            LossReport { loss_ppm: 1200 }.encode(),
            SetBitrate {
                bitrate_kbps: 60_000,
            }
            .encode(),
            BitrateChanged {
                bitrate_kbps: 50_000,
                reason: None,
            }
            .encode(),
            BitrateChanged {
                bitrate_kbps: 50_000,
                reason: Some(AckReason::EncoderLimit),
            }
            .encode(),
            RfiRequest {
                first_frame: 10,
                last_frame: 12,
            }
            .encode(),
            ShardPayloadChanged {
                shard_payload: 1216,
            }
            .encode(),
            ShardPayloadAck {
                shard_payload: 1216,
            }
            .encode(),
            PipelineGap { gap_ms: 12 }.encode(),
            DeliveryReport {
                packets_received: 99,
            }
            .encode(),
            LinkReport {
                proven_kbps: 300_000,
            }
            .encode(),
            SetDelivery { profile: 1 }.encode(),
            DeliveryChanged {
                profile: 1,
                forced: false,
            }
            .encode(),
            HostFacts {
                iface_kind: 1,
                link_mbps: 2500,
                sndbuf_kb: 8192,
                forced_profile: 2,
            }
            .encode(),
            PairRequest {
                name: "TV".into(),
                spake_a: vec![1; 33],
                device_key: vec![],
            }
            .encode(),
            PairChallenge {
                spake_b: vec![2; 33],
                confirm: [3; 32],
            }
            .encode(),
            PairProof { confirm: [4; 32] }.encode(),
            PairResult { ok: false }.encode(),
            AuthChallenge { nonce: [5; 32] }.encode(),
            AuthResponse {
                device_key: vec![6; 91],
                signature: vec![7; 70],
            }
            .encode(),
            Refused {
                code: 0x63,
                reason: "pair first".into(),
            }
            .encode(),
            ProbeRequest {
                target_kbps: 50_000,
                duration_ms: 400,
            }
            .encode(),
            ProbeShaped {
                target_kbps: 1,
                duration_ms: 2,
                burst_hz: 60,
                group_bytes: 4,
                group_rate_kbps: 0,
            }
            .encode(),
            ProbeResult {
                bytes_sent: 1,
                packets_sent: 2,
                duration_ms: 3,
                wire_packets_sent: 4,
                send_dropped: 5,
            }
            .encode(),
            ClockProbe { t1_ns: 11 }.encode(),
            ClockEcho {
                t1_ns: 11,
                t2_ns: 12,
                t3_ns: 13,
            }
            .encode(),
            PhaseReport {
                next_latch_host_ns: 1,
                latch_period_ns: 2,
                uncertainty_ns: 3,
                arrival_lead_ns: 4,
                coherence_milli: 999,
            }
            .encode(),
            ClipControl {
                enabled: true,
                flags: CLIP_FLAG_FILES,
            }
            .encode(),
            ClipState {
                enabled: true,
                policy: CLIP_POLICY_TEXT,
                reason: CLIP_REASON_OK,
            }
            .encode(),
            ClipOffer {
                seq: 3,
                kinds: vec![ClipKind {
                    mime: "text/plain".into(),
                    size_hint: 5,
                }],
            }
            .encode(),
            ClipFetch {
                seq: 3,
                file_index: CLIP_FILE_INDEX_NONE,
                mime: "text/plain".into(),
            }
            .encode(),
            ClipFetchHdr {
                status: CLIP_FETCH_OK,
                total_size: 5,
            }
            .encode(),
            CursorShape {
                serial: 1,
                w: 1,
                h: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![1, 2, 3, 4],
            }
            .encode(),
            CursorRenderMode {
                client_draws: false,
            }
            .encode(),
            AccessUpdate {
                grants: GRANT_GAMEPAD,
                remaining_secs: 10,
            }
            .encode(),
            AudioState { muted: false }.encode(),
            LaunchOutcome::new(LaunchOutcomeKind::SignInNeeded, "Sign in to Steam.").encode(),
            PadSlots { slots: 1 }.encode(),
            InputEdge(ev).encode(),
        ];
        for v1 in all {
            assert_eq!(through(v1.clone()), v1, "type {:#x}", v1[4]);
        }
        // A v1 PhaseReport without coherence keeps its short form.
        let short = PhaseReport {
            next_latch_host_ns: 1,
            latch_period_ns: 2,
            uncertainty_ns: 3,
            arrival_lead_ns: 4,
            coherence_milli: u16::MAX,
        }
        .encode();
        assert_eq!(through(short.clone()), short);
        assert_eq!(frame_from_v1(b"PKFc\x7f"), None);
        assert_eq!(v1_from_frame(0x3FF, &[]), None);
    }

    /// `ClientHello` reaches the host as a `Hello`, and `Ready` as a `Start` with the entries.
    #[test]
    fn the_handshake_changes_shape_at_the_edge() {
        let hello = Hello::decode(
            &[
                b"PKF1".as_slice(),
                &2u32.to_le_bytes(),
                &1920u32.to_le_bytes(),
                &1080u32.to_le_bytes(),
                &60u32.to_le_bytes(),
            ]
            .concat(),
        )
        .unwrap();
        let ch = ClientHello {
            hello: hello.clone(),
            start_ext: vec![(EXT_TAG_CLIENT, b"probe".to_vec())],
            resume: Some([1; 16]),
            suites: vec![MediaSuite::Aes128Gcm],
        };
        let mut rx = RxEdge::default();
        let frame = ch.encode_v2();
        let (ty, body, _) = split_frame(&frame, reg::max_body).unwrap().unwrap();
        assert_eq!(Hello::decode(&rx.to_v1(ty, body).unwrap()).unwrap(), hello);
        assert_eq!(rx.client.as_ref().unwrap().resume, Some([1; 16]));
        let ready = Ready {}.encode_v2();
        let (ty, body, _) = split_frame(&ready, reg::max_body).unwrap().unwrap();
        let start = rx.to_v1(ty, body).unwrap();
        assert_eq!(Start::decode(&start).unwrap().client_udp_port, 0);
        let entries = Start::decode_ext(&start).unwrap();
        assert_eq!(entries, vec![(EXT_TAG_CLIENT, &b"probe"[..])]);

        let mut tx = TxEdge::default();
        let welcome = ServerHello::from_body(
            &super::super::field::Fields::new()
                .bytes(1, &[0; 16])
                .into_body(),
        )
        .unwrap()
        .welcome;
        assert_eq!(tx.to_v2(&welcome.encode()), None, "no session fields yet");
        let fields = SessionFields {
            session_id: [9; 16],
            clock_origin_ns: 7,
            suite: Some(MediaSuite::Aes128Gcm),
        };
        tx.set_session(fields);
        let frame = tx.to_v2(&welcome.encode()).unwrap();
        let (ty, body, _) = split_frame(&frame, reg::max_body).unwrap().unwrap();
        assert_eq!(ty, reg::MSG_SERVER_HELLO);
        let sh = ServerHello::from_body(body).unwrap();
        assert_eq!((sh.session_id, sh.clock_origin_ns), ([9; 16], 7));
    }

    /// A client's own edges carry its handshake through a host's edges and back.
    /// `Pending` marks the client's edge and yields nothing: the handshake keeps waiting for
    /// the `ServerHello` behind it.
    #[test]
    fn pending_is_noted_not_delivered() {
        use crate::quic::v2::msg::Pending;
        let f = Pending {}.encode_v2();
        let (ty, body, _) = split_frame(&f, reg::max_body).unwrap().unwrap();
        let mut rx = RxEdge::default();
        assert_eq!(rx.to_v1(ty, body), None);
        assert!(rx.pending);
        assert_eq!(rx.to_v1(ty, body), None, "a repeat is quiet");
    }

    /// A host's edges put its clock echoes in wire time and take a client's phase latch back
    /// to host time; a client's edges change neither.
    #[test]
    fn host_edges_carry_session_time() {
        let clock = std::sync::Arc::new(SessionClock::new());
        let mut host_tx = TxEdge::host(clock.clone());
        let now = wall_clock_ns();
        let echo = ClockEcho {
            t1_ns: 5,
            t2_ns: now,
            t3_ns: now,
        };
        let f = host_tx.to_v2(&echo.encode()).unwrap();
        let (ty, body, _) = split_frame(&f, reg::max_body).unwrap().unwrap();
        let out = ClockEcho::decode(&RxEdge::default().to_v1(ty, body).unwrap()).unwrap();
        assert_eq!(out.t1_ns, 5, "the client's own stamp comes back untouched");
        assert!(out.t2_ns.abs_diff(clock.to_wire(now)) < 1_000_000);

        let report = PhaseReport {
            next_latch_host_ns: clock.to_wire(now) + 8_000_000,
            latch_period_ns: 16_666_667,
            uncertainty_ns: 1,
            arrival_lead_ns: 2,
            coherence_milli: 900,
        };
        let f = TxEdge::default().to_v2(&report.encode()).unwrap();
        let (ty, body, _) = split_frame(&f, reg::max_body).unwrap().unwrap();
        let mut host_rx = RxEdge {
            clock: Some(clock),
            ..RxEdge::default()
        };
        let back = PhaseReport::decode(&host_rx.to_v1(ty, body).unwrap()).unwrap();
        let off = back.next_latch_host_ns as i64 - (now + 8_000_000) as i64;
        assert!(off.abs() < 2_000_000, "latch off by {off}");
        assert_eq!(back.latch_period_ns, 16_666_667);
    }

    #[test]
    fn a_client_handshake_crosses_both_edges() {
        let hello = Hello::decode(
            &[
                b"PKF1".as_slice(),
                &2u32.to_le_bytes(),
                &640u32.to_le_bytes(),
                &480u32.to_le_bytes(),
                &30u32.to_le_bytes(),
            ]
            .concat(),
        )
        .unwrap();
        let extra = ClientExtra {
            start_ext: vec![(EXT_TAG_ABR, vec![EXT_ABR_ACK_REASON])],
            resume: None,
            suites: vec![MediaSuite::ChaCha20Poly1305],
        };
        let (mut client_tx, mut host_rx) = (TxEdge::client(extra.clone()), RxEdge::default());
        let cross = |tx: &mut TxEdge, rx: &mut RxEdge, v1: &[u8]| {
            let f = tx.to_v2(v1).unwrap();
            let (ty, body, _) = split_frame(&f, reg::max_body).unwrap().unwrap();
            rx.to_v1(ty, body).unwrap()
        };
        assert_eq!(
            Hello::decode(&cross(&mut client_tx, &mut host_rx, &hello.encode())).unwrap(),
            hello
        );
        assert_eq!(host_rx.client.as_ref(), Some(&extra));
        let start = cross(
            &mut client_tx,
            &mut host_rx,
            &Start { client_udp_port: 9 }.encode(),
        );
        assert_eq!(
            ext_abr_features(&Start::decode_ext(&start).unwrap()),
            EXT_ABR_ACK_REASON
        );

        let (mut host_tx, mut client_rx) = (TxEdge::default(), RxEdge::default());
        let fields = SessionFields {
            session_id: [4; 16],
            clock_origin_ns: 99,
            suite: Some(MediaSuite::ChaCha20Poly1305),
        };
        host_tx.set_session(fields);
        let welcome = ServerHello::from_body(
            &super::super::field::Fields::new()
                .bytes(1, &[0; 16])
                .into_body(),
        )
        .unwrap()
        .welcome;
        let back = cross(&mut host_tx, &mut client_rx, &welcome.encode());
        assert_eq!(
            Welcome::decode(&back).unwrap().cipher,
            CIPHER_CHACHA20_POLY1305
        );
        assert_eq!(client_rx.server, Some(fields));
    }
}
