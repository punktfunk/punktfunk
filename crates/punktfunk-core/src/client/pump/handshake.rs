//! Connect + handshake: cert-pinned `pkf2` dial, `ClientHello`/`ServerHello`/`Ready` on the
//! control stream, wall-clock skew, and the [`Session`] over the connection's own socket, keyed from its exporter. A typed application
//! close from the host is [`PunktfunkError::Rejected`], not a transport error; a host that
//! answers no `pkf2` is the wire-version rejection. A host may answer with a `Redirect`
//! instead: the session runs on another host of the same box, and the dial repeats there, once.

use super::*;
use crate::crypto::MediaSuite;
use crate::quic::v2::msg::Redirect;

pub(super) struct HandshakeOut {
    pub(super) conn: ClientConn,
    /// Kept alive so [`super::run_pump`] can flush `CONNECTION_CLOSE` before the runtime
    /// drops. Without the driver, a deliberate quit is silence and the host lingers.
    pub(super) ep: quinn::Endpoint,
    pub(super) session: Session,
    pub(super) ctrl_send: CtlSend,
    pub(super) ctrl_recv: CtlRecv,
    pub(super) negotiated: Negotiated,
    pub(super) host_caps: u8,
}

/// What one dial came to: a session, or the host's word that it runs elsewhere on the box.
enum Dialed {
    Session(Box<HandshakeOut>),
    Redirected(Redirect),
}

/// The handshake's end: a session, or the host's `Redirect`.
enum Step {
    Session(Box<(Session, CtlSend, CtlRecv, Negotiated, u8)>),
    Redirected(Redirect),
}

pub(super) async fn connect_and_handshake(args: &WorkerArgs) -> Result<HandshakeOut> {
    let p = &args.params;
    // One connect budget covers a redirect's second dial.
    let deadline = tokio::time::Instant::now() + p.timeout;
    let (mut host, mut port) = (p.host.clone(), p.port);
    let mut redirected = false;
    loop {
        match dial(args, &host, port, deadline).await? {
            Dialed::Session(out) => return Ok(*out),
            Dialed::Redirected(to) => follow(&mut redirected, &mut host, &mut port, &to)?,
        }
    }
}

/// Take a `Redirect` once: the next dial goes to its address and port with the same pin and
/// the same `ClientHello`. A second one on the same connect is a host that can't place this
/// client, not a seat.
fn follow(redirected: &mut bool, host: &mut String, port: &mut u16, to: &Redirect) -> Result<()> {
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
    Ok(())
}

async fn dial(
    args: &WorkerArgs,
    host: &str,
    port: u16,
    deadline: tokio::time::Instant,
) -> Result<Dialed> {
    let p = &args.params;
    let (pin, shutdown) = (p.pin, &args.shared.shutdown);
    let remote = dial_addr(host, port).await?;
    let identity = p.identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str()));
    let io_err = |e: endpoint::anyhow_result::Error| {
        PunktfunkError::Io(std::io::Error::other(e.to_string()))
    };
    let (r, observed) = endpoint::client_shared(pin, identity, &[crate::quic::v2::registry::ALPN]);
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
    media.stats().set_host(conn.remote_address());
    // Inner future so a failure can read `conn.close_reason()`: a typed application
    // close is `Rejected`, not the generic transport error the failed read produces.
    let handshake = async {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        let label = super::super::client_label();
        // Core decides the ABR byte for every embedder: the controller that reads the ack's
        // reason is this crate's, so no client app can leave it clear and make one host
        // answer two ways.
        let abr = [crate::quic::EXT_ABR_ACK_REASON];
        let preset = p.preset.as_ref().map(|s| s.encode()).unwrap_or_default();
        // The delivery ask rides only when the dial made one; a host that reads it answers.
        let delivery: Vec<u8> = p.delivery.map(|d| d.encode().to_vec()).unwrap_or_default();
        use crate::quic::v2::features::FeatureSet;
        use crate::quic::v2::hello::{ClientHello, Ready, ServerHello};
        use crate::quic::v2::{io as v2io, msg::V2Message, registry};
        v2io::write_stream_type(&mut send, registry::STREAM_CONTROL).await?;
        let entries = crate::quic::start_ext(&label, &abr, &preset, &delivery);
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
            },
            // The `Start` entries: every host reads them.
            start_ext: entries.iter().map(|(t, v)| (*t, v.to_vec())).collect(),
            resume: crate::client::resume::peek(host, port),
            suites: if wants_chacha {
                vec![MediaSuite::ChaCha20Poly1305, MediaSuite::Aes128Gcm]
            } else {
                vec![MediaSuite::Aes128Gcm]
            },
            features: FeatureSet::default()
                .with(registry::FEATURE_STREAM_CONFIG)
                .with(registry::FEATURE_PROFILES),
            profile: p.profile.clone(),
        };
        v2io::send(&mut send, &hello).await?;
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
        *args.shared.local_ip.lock().unwrap() = conn.local_ip();
        *args.shared.v2_session.lock().unwrap() = Some(server.session_id);
        args.shared.anchor.lock().unwrap().on =
            server.features.has(registry::FEATURE_STREAM_CONFIG);
        let cfg = welcome.session_config(Role::Client);
        let media_v2 = crate::session::MediaV2 {
            clock_origin_ns: server.clock_origin_ns,
            keys: Some(keys),
            clock: None,
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
        Ok::<_, PunktfunkError>(Step::Session(Box::new((
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
                profile: server.profile.clone(),
            },
            welcome.host_caps,
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
            let (session, send, recv, negotiated, host_caps) = *landed;
            Ok(Dialed::Session(Box::new(HandshakeOut {
                conn: ClientConn::new(conn),
                ep,
                session,
                ctrl_send: send,
                ctrl_recv: recv,
                negotiated,
                host_caps,
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
                Some((r, _)) => PunktfunkError::Rejected(r),
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
        let (mut redirected, mut host, mut port) = (false, "couch-pc".to_string(), 9777);
        let same_box = Redirect {
            port: 9778,
            ..Redirect::default()
        };
        follow(&mut redirected, &mut host, &mut port, &same_box).unwrap();
        assert_eq!((host.as_str(), port), ("couch-pc", 9778));
        let elsewhere = Redirect {
            addr: "10.0.0.5".into(),
            port: 9779,
            ..Redirect::default()
        };
        assert!(follow(&mut redirected, &mut host, &mut port, &elsewhere).is_err());
        assert_eq!((host.as_str(), port), ("couch-pc", 9778));

        let (mut redirected, mut host, mut port) = (false, "couch-pc".to_string(), 9777);
        follow(&mut redirected, &mut host, &mut port, &elsewhere).unwrap();
        assert_eq!((host.as_str(), port), ("10.0.0.5", 9779));
    }
}
