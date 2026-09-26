//! Connect + handshake: cert-pinned dial, Hello/Welcome/Start on the control stream,
//! wall-clock skew, data-port hole-punch, and the data-plane [`Session`]. A typed
//! application close from the host is [`PunktfunkError::Rejected`], not a transport error.

use super::*;

pub(super) struct HandshakeOut {
    pub(super) conn: quinn::Connection,
    /// Kept alive so [`super::run_pump`] can flush `CONNECTION_CLOSE` before the runtime
    /// drops. Without the driver, a deliberate quit is silence and the host lingers.
    pub(super) ep: quinn::Endpoint,
    pub(super) session: Session,
    pub(super) ctrl_send: quinn::SendStream,
    pub(super) ctrl_recv: io::MsgReader<quinn::RecvStream>,
    pub(super) negotiated: Negotiated,
    pub(super) host_caps: u8,
}

pub(super) async fn connect_and_handshake(args: &WorkerArgs) -> Result<HandshakeOut> {
    let (host, port, pin) = (&args.host, args.port, args.pin);
    let (mode, compositor, gamepad) = (args.mode, args.compositor, args.gamepad);
    let (bitrate_kbps, video_caps, audio_channels) =
        (args.bitrate_kbps, args.video_caps, args.audio_channels);
    let (video_codecs, preferred_codec, display_hdr) =
        (args.video_codecs, args.preferred_codec, args.display_hdr);
    let (launch, identity, shutdown) = (&args.launch, &args.identity, &args.shutdown);
    let remote: std::net::SocketAddr = join_host_port(host, port)
        .parse()
        .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;
    let (ep, observed) = endpoint::client_pinned_with_identity(
        pin,
        identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
    );
    let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
    // Retry silence across the connect budget. One quinn dial dies after the ~8 s idle
    // window, shorter than a suspend-to-RAM resume, and per-attempt retransmits back
    // off. A host that answers (pin/ALPN/typed close) must surface; shutdown stops us.
    const DIAL_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(3);
    // Leave Hello/Welcome/clock-sync room after a late dial, still inside the budget.
    const CONTROL_HEADROOM: std::time::Duration = std::time::Duration::from_secs(2);
    let start = tokio::time::Instant::now();
    let deadline = start + args.connect_timeout;
    let redial_until = start + args.connect_timeout.saturating_sub(CONTROL_HEADROOM);
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
    // Inner future so a failure can read `conn.close_reason()`: a typed application
    // close is `Rejected`, not the generic transport error the failed read produces.
    let handshake = async {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        // Resumable reader: `select!` and the clock-sync timeout can both interrupt a
        // read; a lost partial frame would misalign the stream for the session.
        let mut recv = io::MsgReader::new(recv);

        io::write_msg(
            &mut send,
            &Hello {
                abi_version: crate::WIRE_VERSION,
                mode,
                compositor,
                gamepad,
                bitrate_kbps,
                // Host pending-approval / paired-devices label. `None` → fingerprint "device abcd…".
                name: args.name.clone(),
                launch: launch.clone(),
                // HOST_TIMING / PROBE_SEQ / STREAMED_AU are OR'd in: every NativeClient
                // demuxes 0xCF, isolates probe seqs, and accepts streamed AUs. MULTI_SLICE
                // is decoder truth — only the embedder may set it.
                video_caps: video_caps
                    | crate::quic::VIDEO_CAP_HOST_TIMING
                    | crate::quic::VIDEO_CAP_PROBE_SEQ
                    | crate::quic::VIDEO_CAP_STREAMED_AU,
                audio_channels,
                video_codecs,
                preferred_codec,
                // Client panel HDR volume for the host virtual-display EDID. `None` = unknown/SDR.
                display_hdr,
                // Pass-through. CLIENT_CAP_CURSOR stops host pointer compositing — only
                // an embedder that draws the cursor locally may set it.
                client_caps: args.client_caps,
                // Unconditional: receive buffers are `MAX_DATAGRAM_BYTES`, so every
                // embedder accepts a mid-session shard grow (design/shard-payload-reneg.md).
                max_shard_payload: crate::config::max_shard_payload() as u16,
                // Asked-for format. Legacy 48 kHz / 16-bit omits both fields (Hello stays
                // pre-hi-res). Non-legacy travels with CLIENT_CAP_AUDIO_HIRES — the bit
                // is the opt-in, these are its parameters.
                audio_rate_hz: args.audio_rate_hz,
                audio_bits: args.audio_bits,
                // The coupling asked for; `0` (legacy) keeps the Hello byte-identical.
                audio_layout: args.audio_layout.wire(),
                // How this client fills its view; a host framing for another device reframes to it.
                video_fit: args.video_fit.wire(),
            }
            .encode(),
        )
        .await?;
        let welcome = Welcome::decode(&recv.read_msg().await?)?;
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

        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let udp_port = probe.local_addr()?.port();
        drop(probe);
        // Hello is frozen and first contact, so the client's own label rides here — and only
        // toward a host that said it parses the block. An older host reads the 6 bytes it knows.
        let start = Start {
            client_udp_port: udp_port,
        };
        let label = super::super::client_label();
        // Core decides the ABR byte for every embedder: the controller that reads the ack's
        // reason is this crate's, so no client app can leave it clear and make one host
        // answer two ways.
        let abr = [crate::quic::EXT_ABR_ACK_REASON];
        let preset = super::super::session_preset_bytes();
        let ext = crate::quic::start_ext(welcome.host_caps2, &label, &abr, &preset);
        let start_msg = if ext.is_empty() {
            start.encode()
        } else {
            start.encode_ext(&ext)?
        };
        io::write_msg(&mut send, &start_msg).await?;

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

        let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())?;
        // Punch the data port: video is raw UDP, unlike client-initiated QUIC side planes.
        // Stops with the shared shutdown flag.
        if let Ok(sock) = transport.try_clone_socket() {
            crate::transport::spawn_data_punch(sock, shutdown.clone());
        }
        let mut session = Session::new(welcome.session_config(Role::Client), Box::new(transport))?;
        // PyroWave: aged-out lossy frames as blocks-with-holes. All-intra renders
        // localized blur, better than a freeze.
        if welcome.codec == crate::quic::CODEC_PYROWAVE {
            session.set_deliver_partial_frames(true);
        }
        // Embedder opt-in: AU prefixes as `Frame::part` while the tail is still on
        // the wire. Never on PyroWave — newest-wins per queue entry shreds a mid-AU
        // (`FrameChannel::pop`). Unrelated to `VIDEO_CAP_STREAMED_AU` (whole Frame).
        if args.frame_parts && welcome.codec != crate::quic::CODEC_PYROWAVE {
            session.set_deliver_frame_parts(true);
        }
        Ok::<_, PunktfunkError>((
            session,
            send,
            recv,
            Negotiated {
                mode: welcome.mode,
                compositor: welcome.compositor,
                gamepad: welcome.gamepad,
                host_fingerprint: fingerprint,
                bitrate_kbps: welcome.bitrate_kbps,
                clock_offset_ns,
                clock_rtt_ns,
                bit_depth: welcome.bit_depth,
                color: welcome.color,
                chroma_format: welcome.chroma_format,
                audio_channels: welcome.audio_channels,
                // Welcome is the only authority — never claim a rate we did not get
                // (`design/hi-res-audio.md`). An omitted tail is Opus / 48 kHz / 16.
                audio_codec: welcome.audio_codec,
                audio_rate_hz: welcome.audio_rate_hz,
                audio_bits: welcome.audio_bits,
                audio_frame_us: welcome.audio_frame_us,
                audio_layout: welcome.audio_layout,
                codec: welcome.codec,
                shard_payload: welcome.shard_payload,
                host_caps: welcome.host_caps,
                host_caps2: welcome.host_caps2,
                mgmt_port: welcome.mgmt_port,
                grants: welcome.grants,
                expires_in_secs: welcome.expires_in_secs,
            },
            welcome.host_caps,
        ))
    };
    match handshake.await {
        Ok((session, send, recv, negotiated, host_caps)) => Ok(HandshakeOut {
            conn,
            ep,
            session,
            ctrl_send: send,
            ctrl_recv: recv,
            negotiated,
            host_caps,
        }),
        Err(e) => {
            // Typed close can land after the stream error (reset/FIN vs CONNECTION_CLOSE).
            // Brief wait so a host setup failure is `Rejected`, not mid-frame EOF.
            if conn.close_reason().is_none() {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(300), conn.closed())
                    .await;
            }
            Err(match reject_from_close(&conn) {
                Some((r, _)) => PunktfunkError::Rejected(r),
                None => e,
            })
        }
    }
}
