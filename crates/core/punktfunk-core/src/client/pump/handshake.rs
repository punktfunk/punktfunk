//! Connect + handshake: cert-pinned `pkf2` dial, `ClientHello`/`ServerHello`/`Ready` on the
//! control stream, wall-clock skew, and the [`Session`] over the connection's own socket, keyed from its exporter. A typed application
//! close from the host is [`PunktfunkError::Rejected`], not a transport error; a host that
//! answers no `pkf2` is the wire-version rejection. A host may answer with a `Redirect`
//! instead: the session runs on another host of the same box, and the dial repeats there, once.
//! A profile unlock rides the first dial only, right after its `ClientHello`.

use super::*;
use crate::crypto::MediaSuite;
use crate::quic::v2::msg::{ProfileUnlock, Redirect};

pub(super) struct HandshakeOut {
    pub(super) conn: ClientConn,
    /// Kept alive so [`super::run_pump`] can flush `CONNECTION_CLOSE` before the runtime
    /// drops. Without the driver, a deliberate quit is silence and the host lingers.
    pub(super) ep: quinn::Endpoint,
    pub(super) session: Session,
    pub(super) ctrl_send: CtlSend,
    pub(super) ctrl_recv: CtlRecv,
    pub(super) negotiated: Negotiated,
    /// The host and port that run the session: a seat's after a redirect. A lost session is
    /// kept under them, so the next dial's resume reaches the host that holds it.
    pub(super) landed_at: (String, u16),
}

/// What one dial came to: a session, or the host's word that it runs elsewhere on the box.
enum Dialed {
    Session(Box<HandshakeOut>),
    Redirected(Redirect),
}

/// The handshake's end: a session, or the host's `Redirect`.
enum Step {
    Session(Box<(Session, CtlSend, CtlRecv, Negotiated)>),
    Redirected(Redirect),
}

/// `unlock` goes to the first dial; the one a `Redirect` sends gets `None`.
pub(super) async fn connect_and_handshake(
    args: &WorkerArgs,
    mut unlock: Option<ProfileUnlock>,
) -> Result<HandshakeOut> {
    let p = &args.params;
    // One connect budget covers a redirect's second dial.
    let deadline = tokio::time::Instant::now() + p.timeout;
    let (mut host, mut port, mut pin) = (p.host.clone(), p.port, p.pin);
    let mut redirected = false;
    loop {
        match dial(args, &host, port, pin, deadline, unlock.take()).await? {
            Dialed::Session(out) => return Ok(*out),
            Dialed::Redirected(to) => {
                follow(&mut redirected, (&mut host, &mut port, &mut pin), &to)?
            }
        }
    }
}

/// What this client offers. A format epoch is the embedder's promise to rebuild its decoder,
/// screens its promise to show more than one picture, asked for at Hello or later, studio
/// range its report that the display mishandles full range, and a profile unlock its promise
/// to prompt, so core offers each only when the embedder made it.
fn client_features(p: &ConnectParams) -> crate::quic::v2::features::FeatureSet {
    use crate::quic::v2::registry;
    let mut set = crate::quic::v2::features::FeatureSet::default()
        .with(registry::FEATURE_STREAM_CONFIG)
        .with(registry::FEATURE_PAD_WRITES)
        .with(registry::FEATURE_PROFILES)
        .with(registry::FEATURE_CLEAR_PLACEMENT)
        .with(registry::FEATURE_SCROLL_AXIS_UNITS);
    if p.rebuilds_decoder {
        set = set.with(registry::FEATURE_FORMAT_EPOCH);
    }
    if p.reopens_audio {
        set = set.with(registry::FEATURE_AUDIO_EPOCH);
    }
    if p.shows_screens || !p.screens.is_empty() {
        set = set.with(registry::FEATURE_SCREENS);
    }
    if p.studio_range {
        set = set.with(registry::FEATURE_STUDIO_RANGE);
    }
    if p.studio_range_hdr {
        set = set.with(registry::FEATURE_STUDIO_RANGE_HDR);
    }
    if p.prompts_unlock {
        set = set.with(registry::FEATURE_PROFILE_UNLOCK);
    }
    set
}

/// Take a `Redirect` once: the next dial goes to its address and port with the same
/// `ClientHello`, pinned to the certificate it names; an empty pin keeps the one dialed with.
/// A second one on the same connect is a host that can't place this client, not a seat.
fn follow(
    redirected: &mut bool,
    (host, port, pin): (&mut String, &mut u16, &mut Option<[u8; 32]>),
    to: &Redirect,
) -> Result<()> {
    if std::mem::replace(redirected, true) {
        return Err(PunktfunkError::InvalidArg(
            "redirected twice on one connect",
        ));
    }
    tracing::info!(
        addr = %to.addr,
        port = to.port,
        seat = %to.seat_name,
        "redirected to another host of this box"
    );
    if !to.addr.is_empty() {
        *host = to.addr.clone();
    }
    *port = to.port;
    if !to.pin.is_empty() {
        // The message's own check admits only 64 hex characters.
        *pin = crate::fp::parse_hex32(&to.pin);
    }
    Ok(())
}

/// One connection's handshake. `unlock` follows the `ClientHello`, which announces it, and is
/// wiped once written. An unlock without a `pin` is `InvalidArg` before anything is dialed.
async fn dial(
    args: &WorkerArgs,
    host: &str,
    port: u16,
    pin: Option<[u8; 32]>,
    deadline: tokio::time::Instant,
    unlock: Option<ProfileUnlock>,
) -> Result<Dialed> {
    // A password or key proof goes only to a host whose identity this client already pinned.
    if unlock.is_some() && pin.is_none() {
        return Err(PunktfunkError::InvalidArg(
            "a profile unlock needs a pinned host",
        ));
    }
    let p = &args.params;
    let shutdown = &args.shared.shutdown;
    let remote = dial_addr(host, port).await?;
    let identity = p.identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str()));
    let io_err = |e: endpoint::anyhow_result::Error| {
        PunktfunkError::Io(std::io::Error::other(e.to_string()))
    };
    let (r, observed) = endpoint::client_shared(
        pin,
        identity,
        &[crate::quic::v2::registry::ALPN],
        Some(remote.ip()),
    );
    let (ep, media) = r.map_err(io_err)?;
    // Retry silence across the connect budget. One quinn dial dies after the ~8 s idle
    // window, shorter than a suspend-to-RAM resume, and per-attempt retransmits back
    // off. A host that answers (pin/ALPN/typed close) must surface; shutdown stops us.
    const DIAL_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(3);
    // Leave Hello/Welcome/clock-sync room after a late dial, still inside the budget.
    const CONTROL_HEADROOM: std::time::Duration = std::time::Duration::from_secs(2);
    let redial_until = deadline.checked_sub(CONTROL_HEADROOM).unwrap_or(deadline);
    let conn = loop {
        let connecting = ep
            .connect(remote, "punktfunk")
            .map_err(|_| PunktfunkError::InvalidArg("connect"))?;
        // Remaining budget only: a late success must not land after `ready_rx` gave up.
        let now = tokio::time::Instant::now();
        let attempt = DIAL_ATTEMPT.min(deadline.saturating_duration_since(now));
        let gave_up = || {
            tokio::time::Instant::now() >= redial_until
                || shutdown.load(std::sync::atomic::Ordering::SeqCst)
        };
        match tokio::time::timeout(attempt, connecting).await {
            Ok(Ok(conn)) => break conn,
            Ok(Err(e)) => {
                // Pin mismatch arrives as TLS failure; Crypto, not Io, so identity is distinct.
                let fp_mismatch = pin.is_some()
                    && observed.lock().unwrap().map(|fp| Some(fp) != pin) == Some(true);
                if fp_mismatch {
                    return Err(PunktfunkError::Crypto);
                }
                if endpoint::refused_alpn(&e) {
                    return Err(PunktfunkError::Rejected(
                        crate::reject::RejectReason::WireVersionMismatch,
                        String::new(),
                    ));
                }
                // Only TimedOut (host never answered) is retryable.
                let host_silent = matches!(e, quinn::ConnectionError::TimedOut);
                if !host_silent {
                    return Err(PunktfunkError::Io(std::io::Error::other(e.to_string())));
                }
                if gave_up() {
                    return Err(PunktfunkError::Timeout);
                }
            }
            // Attempt window elapsed, host still silent. Drop `connecting` and redial.
            Err(_) => {
                if gave_up() {
                    return Err(PunktfunkError::Timeout);
                }
            }
        }
        tracing::debug!(%remote, "host silent — re-dialing (wake/resume tolerant connect)");
    };
    let fingerprint = observed.lock().unwrap().unwrap_or([0u8; 32]);
    tracing::info!("connected");
    // The host streams as soon as it has `Start`, so its address is whitelisted for media
    // before the handshake. Later is the opening IDR.
    media.set_host(conn.remote_address());
    *args.shared.reconnect.lock().unwrap() = Some(Box::new(media.reconnector()));
    // Inner future so a failure can read `conn.close_reason()`: a typed application
    // close is `Rejected`, not the generic transport error the failed read produces.
    let handshake = async {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        // Every dial says what its OS knows about this end of the path.
        let link = conn
            .local_ip()
            .map(crate::transport::ifinfo::link_facts)
            .unwrap_or_default();
        use crate::quic::v2::hello::{ClientHello, Ready, ServerHello};
        use crate::quic::v2::{io as v2io, msg::V2Message, registry};
        v2io::write_stream_type(&mut send, registry::STREAM_CONTROL).await?;
        let wants_chacha = p.video_caps & crate::quic::VIDEO_CAP_CHACHA20 != 0;
        // Resumable reader: `select!` and the clock-sync timeout can both interrupt a
        // read; a lost partial frame would misalign the stream for the session.
        let mut recv = CtlRecv::new(recv);

        let hello = ClientHello {
            hello: Hello {
                mode: p.mode,
                compositor: p.compositor,
                gamepad: p.gamepad,
                bitrate_kbps: p.bitrate_kbps,
                // Host pending-approval / paired-devices label. `None` → fingerprint "device abcd…".
                name: p.name.clone(),
                launch: p.launch.clone(),
                // HOST_TIMING / PROBE_SEQ / STREAMED_AU are OR'd in: every NativeClient
                // demuxes 0xCF, isolates probe seqs, and accepts streamed AUs. MULTI_SLICE
                // is decoder truth — only the embedder may set it.
                video_caps: p.video_caps
                    | crate::quic::VIDEO_CAP_HOST_TIMING
                    | crate::quic::VIDEO_CAP_PROBE_SEQ
                    | crate::quic::VIDEO_CAP_STREAMED_AU,
                audio_channels: p.audio_channels,
                video_codecs: p.video_codecs,
                preferred_codec: p.preferred_codec,
                // Client panel HDR volume for the host virtual-display EDID. `None` = unknown/SDR.
                display_hdr: p.display_hdr,
                // Pass-through. CLIENT_CAP_CURSOR stops host pointer compositing — only
                // an embedder that draws the cursor locally may set it.
                client_caps: p.client_caps,
                // Unconditional: receive buffers are `MAX_DATAGRAM_BYTES`, so every
                // embedder accepts a mid-session shard grow (design/shard-payload-reneg.md).
                max_shard_payload: crate::config::max_shard_payload() as u16,
                // Asked-for format. Legacy 48 kHz / 16-bit omits both fields (Hello stays
                // pre-hi-res). Non-legacy travels with CLIENT_CAP_AUDIO_HIRES — the bit
                // is the opt-in, these are its parameters.
                audio_rate_hz: p.audio_rate_hz,
                audio_bits: p.audio_bits,
                // The coupling asked for; `0` (legacy) keeps the Hello byte-identical.
                audio_layout: p.audio_layout.wire(),
                // How this client fills its view; a host framing for another device reframes to it.
                video_fit: p.video_fit.wire(),
                audio_bitstreams: p.audio_bitstreams,
            },
            client_label: Some(super::super::client_label()),
            // Core decides the ABR byte for every embedder: the controller that reads the ack's
            // reason is this crate's, so no client app can leave it clear and make one host
            // answer two ways.
            abr_features: crate::quic::EXT_ABR_ACK_REASON,
            preset: p.preset.clone(),
            link,
            probe_only: p.probe_only,
            pyrowave_bpp_x100: p.pyrowave_bpp_x100,
            slices: p.slices,
            resume: crate::client::resume::peek(host, port),
            suites: if wants_chacha {
                vec![MediaSuite::ChaCha20Poly1305, MediaSuite::Aes128Gcm]
            } else {
                vec![MediaSuite::Aes128Gcm]
            },
            features: match unlock {
                Some(_) => client_features(p).with(registry::FEATURE_UNLOCK_FOLLOWS),
                None => client_features(p),
            },
            profile: p.profile.clone(),
            access_only: false,
            screens: p.screens.clone(),
            view: p.view,
            screen_wish: p.screen_wish.clone(),
        };
        v2io::send(&mut send, &hello).await?;
        if let Some(unlock) = unlock {
            // quinn's send buffer holds an unwiped copy until the host acks it; wiping that
            // one needs a hook in quinn.
            v2io::write_frame(&mut send, &unlock.encode()).await?;
        }
        // The hello carried the resume id, so the entry is spent now, not by a dial that died.
        crate::client::resume::take(host, port);
        // `Pending` repeats while the host asks its console about this device.
        let mut waiting = false;
        let server = loop {
            let (ty, body) = recv.read_frame().await?;
            match ty {
                ServerHello::TYPE => break ServerHello::from_body(&body)?,
                Redirect::TYPE => return Ok(Step::Redirected(Redirect::from_body(&body)?)),
                registry::MSG_PENDING if !std::mem::replace(&mut waiting, true) => {
                    tracing::info!("the host is waiting for this device to be approved");
                    args.shared
                        .pending
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
        };
        let welcome = server.welcome;
        if welcome.compositor != CompositorPref::Auto {
            tracing::info!(
                compositor = welcome.compositor.as_str(),
                "host resolved compositor"
            );
        }
        if welcome.gamepad != GamepadPref::Auto {
            tracing::info!(
                gamepad = welcome.gamepad.as_str(),
                "host resolved gamepad backend"
            );
        }

        v2io::send(&mut send, &Ready {}).await?;

        // Skew handshake before the control task takes the stream. 0 ⇒ old host did
        // not answer (shared-clock). Embedder present times are in the host capture clock.
        let (clock_offset_ns, clock_rtt_ns) =
            match crate::quic::clock_sync(&mut send, &mut recv).await {
                Some(skew) => {
                    tracing::info!(
                        offset_ns = skew.offset_ns,
                        rtt_us = skew.rtt_ns / 1000,
                        rounds = skew.rounds,
                        "clock skew estimated (host-client)"
                    );
                    (skew.offset_ns, Some(skew.rtt_ns))
                }
                None => (0, None),
            };

        let suite = server
            .suite
            .ok_or(PunktfunkError::Unsupported("unsealed native media"))?;
        let keys =
            endpoint::media_keys(&conn, &server.session_id, suite).ok_or(PunktfunkError::Crypto)?;
        if let Ok(sock) = media.try_clone_socket() {
            *args.shared.data_sock.lock().unwrap() = Some(sock);
        }
        *args.shared.demux.lock().unwrap() = Some(media.stats().clone());
        *args.shared.local_ip.lock().unwrap() = conn.local_ip();
        *args.shared.client_link.lock().unwrap() = link;
        tracing::info!(kind = link.kind, mbps = link.mbps, "this end's link facts");
        *args.shared.v2_session.lock().unwrap() = Some(server.session_id);
        {
            let mut anchor = args.shared.anchor.lock().unwrap();
            anchor.on = server.features.has(registry::FEATURE_STREAM_CONFIG);
            anchor.formats = anchor.on && server.features.has(registry::FEATURE_FORMAT_EPOCH);
            anchor.book(0, crate::quic::VideoFormat::of_welcome(&welcome));
        }
        *args.shared.video_format.lock().unwrap() = crate::quic::VideoFormat::of_welcome(&welcome);
        {
            let mut audio = args.shared.audio_anchor.lock().unwrap();
            audio.on = server.features.has(registry::FEATURE_AUDIO_EPOCH);
            audio.live = Some(crate::audio::plane::PlaneFormat::from_welcome(
                welcome.audio_codec,
                welcome.audio_rate_hz,
                welcome.audio_bits,
                welcome.audio_channels,
                u32::from(welcome.audio_frame_us),
                welcome.audio_layout,
            ));
        }
        let cfg = welcome.session_config(Role::Client);
        let media_v2 = crate::session::MediaV2 {
            clock_origin_ns: server.clock_origin_ns,
            keys: Some(keys),
            clock: None,
            clear_placement: server.features.has(registry::FEATURE_CLEAR_PLACEMENT),
        };
        let mut session = Session::new(cfg, media_v2, Box::new(media))?;
        // PyroWave: aged-out lossy frames as blocks-with-holes. All-intra renders
        // localized blur, better than a freeze.
        if welcome.codec == crate::quic::CODEC_PYROWAVE {
            session.set_deliver_partial_frames(true);
        }
        // Embedder opt-in: AU prefixes as `Frame::part` while the tail is still on
        // the wire. Never on PyroWave — newest-wins per queue entry shreds a mid-AU
        // (`FrameChannel::pop`). Unrelated to `VIDEO_CAP_STREAMED_AU` (whole Frame).
        if p.frame_parts && welcome.codec != crate::quic::CODEC_PYROWAVE {
            session.set_deliver_frame_parts(true);
        }
        let screens = server.features.has(registry::FEATURE_SCREENS);
        args.shared.screens.set_on(screens);
        session.set_screens(screens);
        Ok::<_, PunktfunkError>(Step::Session(Box::new((
            session,
            send,
            recv,
            Negotiated {
                welcome,
                host_fingerprint: fingerprint,
                clock_offset_ns,
                clock_rtt_ns,
                profile: server.profile.clone(),
                screen: server.screen.clone(),
                role: server.role,
                scroll_axis_units: server.features.has(registry::FEATURE_SCROLL_AXIS_UNITS),
                picture: server.picture,
            },
        ))))
    };
    // Cancel and the connect deadline (both `shutdown`) reach a parked handshake too: the host
    // withdraws a request-access knock only when the connection closes.
    let cancelled = async {
        while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    let outcome = tokio::select! {
        r = handshake => Some(r),
        () = cancelled => None,
    };
    let Some(outcome) = outcome else {
        conn.close(crate::quic::QUIT_CLOSE_CODE.into(), b"client cancelled");
        let _ = tokio::time::timeout(std::time::Duration::from_millis(300), ep.wait_idle()).await;
        return Err(PunktfunkError::Timeout);
    };
    match outcome {
        Ok(Step::Session(landed)) => {
            let (session, send, recv, negotiated) = *landed;
            Ok(Dialed::Session(Box::new(HandshakeOut {
                conn: ClientConn::new(conn),
                ep,
                session,
                ctrl_send: send,
                ctrl_recv: recv,
                negotiated,
                landed_at: (host.to_string(), port),
            })))
        }
        // Nothing of this connection carries over; the next dial is a fresh handshake.
        Ok(Step::Redirected(to)) => {
            conn.close(crate::quic::QUIT_CLOSE_CODE.into(), b"redirected");
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(300), ep.wait_idle()).await;
            Ok(Dialed::Redirected(to))
        }
        Err(e) => {
            // Typed close can land after the stream error (reset/FIN vs CONNECTION_CLOSE).
            // Brief wait so a host setup failure is `Rejected`, not mid-frame EOF.
            if conn.close_reason().is_none() {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(300), conn.closed())
                    .await;
            }
            Err(match reject_from_close(&conn) {
                Some((r, said)) => PunktfunkError::Rejected(r, said),
                None => e,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first redirect moves the dial, an empty address keeps the host, and a second
    /// redirect on the same connect is refused with the dial left where the first put it.
    #[test]
    fn one_redirect_per_connect() {
        let (mut redirected, mut host, mut port, mut pin) =
            (false, "couch-pc".to_string(), 9777, Some([1; 32]));
        let same_box = Redirect {
            port: 9778,
            ..Redirect::default()
        };
        follow(&mut redirected, (&mut host, &mut port, &mut pin), &same_box).unwrap();
        assert_eq!(
            (host.as_str(), port, pin),
            ("couch-pc", 9778, Some([1; 32]))
        );
        let elsewhere = Redirect {
            addr: "10.0.0.5".into(),
            port: 9779,
            pin: "ab".repeat(32),
            ..Redirect::default()
        };
        let second = follow(
            &mut redirected,
            (&mut host, &mut port, &mut pin),
            &elsewhere,
        );
        assert!(second.is_err());
        assert_eq!(
            (host.as_str(), port, pin),
            ("couch-pc", 9778, Some([1; 32]))
        );

        let (mut redirected, mut host, mut port, mut pin) =
            (false, "couch-pc".to_string(), 9777, Some([1; 32]));
        follow(
            &mut redirected,
            (&mut host, &mut port, &mut pin),
            &elsewhere,
        )
        .unwrap();
        assert_eq!(
            (host.as_str(), port, pin),
            ("10.0.0.5", 9779, Some([0xab; 32]))
        );
    }

    /// Screens are offered on the embedder's promise or its Hello ask, never without either.
    #[test]
    fn screens_are_offered_on_the_promise() {
        use crate::quic::v2::registry::FEATURE_SCREENS;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let mut p = ConnectParams::new("couch-pc", 9777, mode, Duration::from_secs(1));
        assert!(!client_features(&p).has(FEATURE_SCREENS));
        p.shows_screens = true;
        assert!(
            client_features(&p).has(FEATURE_SCREENS),
            "a promise with no ask"
        );
        p.shows_screens = false;
        p.screens = vec![crate::quic::v2::msg::Screen {
            mode,
            ..Default::default()
        }];
        assert!(client_features(&p).has(FEATURE_SCREENS), "an ask");
    }

    /// The unlock feature is offered on the embedder's promise to prompt, never by default.
    #[test]
    fn an_unlock_is_offered_on_the_promise() {
        use crate::quic::v2::registry::FEATURE_PROFILE_UNLOCK;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let mut p = ConnectParams::new("couch-pc", 9777, mode, Duration::from_secs(1));
        assert!(!client_features(&p).has(FEATURE_PROFILE_UNLOCK));
        p.prompts_unlock = true;
        assert!(client_features(&p).has(FEATURE_PROFILE_UNLOCK));
    }

    fn mode() -> Mode {
        Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        }
    }

    fn password() -> ProfileUnlock {
        use crate::quic::v2::msg::{ENROLL_NONE, UNLOCK_PASSWORD};
        let secret = zeroize::Zeroizing::new(b" open sesame ".to_vec());
        ProfileUnlock::new(UNLOCK_PASSWORD, secret, false, ENROLL_NONE, vec![]).unwrap()
    }

    /// Two loopback hosts of one box and its pin: one identity, so the door's pin holds at the
    /// seat.
    fn one_box() -> ([quinn::Endpoint; 2], [u8; 32]) {
        let (cert, key) = endpoint::generate_identity().unwrap();
        let host = || {
            let addr = "127.0.0.1:0".parse().unwrap();
            let alpns = [crate::quic::v2::registry::ALPN];
            endpoint::server_shared(addr, &cert, &key, Duration::from_secs(10), &alpns)
                .unwrap()
                .0
        };
        (
            [host(), host()],
            endpoint::fingerprint_of_pem(&cert).unwrap(),
        )
    }

    /// One dial's control stream and its `ClientHello`, host side.
    async fn hello(
        ep: &quinn::Endpoint,
    ) -> (
        quinn::Connection,
        quinn::SendStream,
        crate::quic::v2::io::FrameReader<quinn::RecvStream>,
        crate::quic::v2::hello::ClientHello,
    ) {
        use crate::quic::v2::io::{read_stream_type, FrameReader};
        let conn = ep.accept().await.unwrap().await.unwrap();
        let (tx, mut rx) = conn.accept_bi().await.unwrap();
        let control = crate::quic::v2::registry::STREAM_CONTROL;
        assert_eq!(read_stream_type(&mut rx).await.unwrap(), control);
        let mut r = FrameReader::new(rx);
        let (ty, body) = r.read_frame().await.unwrap();
        let h = crate::quic::v2::msg::decode(ty, &body).unwrap();
        (conn, tx, r, h)
    }

    /// The first dial announces the unlock in its hello and sends it right after; the dial a
    /// `Redirect` sends announces and carries none. Both offer the feature. The seat's
    /// `ProfileLocked` close reaches the caller with its sentence.
    #[test]
    fn the_unlock_rides_the_first_dial_only() {
        use crate::quic::v2::registry::FEATURE_PROFILE_UNLOCK;
        let soon = |ms| tokio::time::Instant::now() + Duration::from_millis(ms);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _in_rt = rt.enter();
        let ([door, seat], pin) = one_box();
        let door_port = door.local_addr().unwrap().port();
        let seat_port = seat.local_addr().unwrap().port();
        let door_side = rt.spawn(async move {
            let (conn, mut tx, mut r, h) = hello(&door).await;
            let unlock = r.read_profile_unlock(soon(1000)).await.unwrap();
            let to = Redirect {
                port: seat_port,
                ..Redirect::default()
            };
            crate::quic::v2::io::send(&mut tx, &to).await.unwrap();
            conn.closed().await;
            let offered = h.features.has(FEATURE_PROFILE_UNLOCK);
            (offered, h.unlock_follows(), unlock.secret.to_vec())
        });
        let seat_side = rt.spawn(async move {
            let (conn, _tx, mut r, h) = hello(&seat).await;
            let more = r.read_profile_unlock(soon(300)).await.map(|_| ());
            let locked = crate::reject::PROFILE_LOCKED_CLOSE_CODE;
            conn.close(locked.into(), b"Wrong password. Try again.");
            let _ = tokio::time::timeout(Duration::from_secs(1), seat.wait_idle()).await;
            let offered = h.features.has(FEATURE_PROFILE_UNLOCK);
            (offered, h.unlock_follows(), more)
        });

        let params = ConnectParams {
            unlock: Some(password()),
            pin: Some(pin),
            ..ConnectParams::new("127.0.0.1", door_port, mode(), Duration::from_secs(5))
        };
        let got = crate::client::NativeClient::connect(params).map(|_| ());
        let five = Duration::from_secs(5);
        let door_got = rt.block_on(tokio::time::timeout(five, door_side));
        let (door_offered, door_promised, secret) = door_got.unwrap().unwrap();
        let seat_got = rt.block_on(tokio::time::timeout(five, seat_side));
        let (seat_offered, seat_promised, seat_heard) = seat_got.unwrap().unwrap();
        assert!(
            door_offered && seat_offered,
            "an unlock in hand offers the feature"
        );
        assert!(door_promised, "the first dial's hello announces its unlock");
        assert_eq!(secret.as_slice(), &b" open sesame "[..]);
        assert!(!seat_promised, "the redirected dial announced an unlock");
        let heard = seat_heard.map_err(|e| e.kind());
        assert_eq!(
            heard,
            Err(std::io::ErrorKind::TimedOut),
            "the redirected dial sent one"
        );
        match got {
            Err(PunktfunkError::Rejected(r, said)) => {
                assert_eq!(r, crate::reject::RejectReason::ProfileLocked);
                assert_eq!(said, "Wrong password. Try again.");
            }
            other => panic!("{other:?}"),
        }
    }

    /// An unlock goes only to a pinned host: without a pin the connect fails and nothing is
    /// dialed. A pinned dial with no unlock announces none.
    #[test]
    fn an_unlock_needs_a_pinned_host() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _in_rt = rt.enter();
        let ([door, _], pin) = one_box();
        let port = door.local_addr().unwrap().port();
        let new = || ConnectParams::new("127.0.0.1", port, mode(), Duration::from_secs(5));
        let unpinned = ConnectParams {
            unlock: Some(password()),
            ..new()
        };
        match crate::client::NativeClient::connect(unpinned).map(|_| ()) {
            Err(PunktfunkError::InvalidArg(_)) => {}
            other => panic!("{other:?}"),
        }
        let dialed = rt.block_on(tokio::time::timeout(
            Duration::from_millis(300),
            door.accept(),
        ));
        assert!(dialed.is_err(), "an unpinned unlock dialed");

        let door_side = rt.spawn(async move {
            let (conn, _tx, _r, h) = hello(&door).await;
            conn.close(0u32.into(), b"");
            h.unlock_follows()
        });
        let plain = ConnectParams {
            pin: Some(pin),
            ..new()
        };
        let _ = crate::client::NativeClient::connect(plain);
        let promised = rt.block_on(tokio::time::timeout(Duration::from_secs(5), door_side));
        assert!(
            !promised.unwrap().unwrap(),
            "a dial with no unlock announced one"
        );
    }

    /// Studio range is asked for only on the embedder's report, SDR and HDR each on its own.
    #[test]
    fn studio_range_is_asked_on_the_report() {
        use crate::quic::v2::registry::{FEATURE_STUDIO_RANGE, FEATURE_STUDIO_RANGE_HDR};
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let mut p = ConnectParams::new("couch-pc", 9777, mode, Duration::from_secs(1));
        assert!(!client_features(&p).has(FEATURE_STUDIO_RANGE));
        assert!(!client_features(&p).has(FEATURE_STUDIO_RANGE_HDR));
        p.studio_range = true;
        assert!(client_features(&p).has(FEATURE_STUDIO_RANGE));
        assert!(!client_features(&p).has(FEATURE_STUDIO_RANGE_HDR));
        p.studio_range_hdr = true;
        assert!(client_features(&p).has(FEATURE_STUDIO_RANGE_HDR));
    }
}
