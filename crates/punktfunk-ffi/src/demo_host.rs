//! A host on `127.0.0.1`, inside the client library: the app's demo mode.
//!
//! The embedder draws and encodes the pictures and hands each access unit to
//! [`DemoHost::submit_video`]. This side speaks what a real host speaks — `punktfunk/2`: the
//! handshake, the clock handshake, the sealed media and the Opus audio plane — so the embedder's
//! connect, decode, present, HUD and input paths run unchanged against a pinned loopback host.
//!
//! One session at a time: a new client supersedes the old one. Input comes back through
//! [`DemoHost::next_input`] so the picture can answer it. Audio is Opus silence with a short
//! chime on each press.

use punktfunk_core::audio::{LAYOUT_STEREO, SAMPLE_RATE_HZ};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, Mode, Role};
use punktfunk_core::crypto::MediaSuite;
use punktfunk_core::error::{PunktfunkError, Result};
use punktfunk_core::input::{GamepadSnapshot, InputEvent, InputKind, INPUT_MAGIC};
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF};
use punktfunk_core::quic::v2::clock::SessionClock;
use punktfunk_core::quic::v2::translate::{RxEdge, SessionFields, TxEdge};
use punktfunk_core::quic::v2::{dgram, io as v2io, registry};
use punktfunk_core::quic::{
    self, endpoint, io, wall_clock_ns, ClockEcho, ClockProbe, Hello, Reconfigure, Reconfigured,
    RequestKeyframe, RfiRequest, Start, Welcome,
};
use punktfunk_core::session::{MediaV2, Session};
use punktfunk_core::transport::shared::MediaSender;
use rand::RngCore;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Largest picture the demo asks for. 1080p60 keeps a CPU-drawn scene and a TV's encoder
/// well inside one frame time.
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const MAX_REFRESH_HZ: u32 = 60;
const DEFAULT_BITRATE_KBPS: u32 = 12_000;
/// AUs queued between the embedder and the send thread. A full queue drops the AU and asks
/// for an IDR: every P-frame after a lost one is undecodable.
const VIDEO_QUEUE: usize = 4;
const INPUT_QUEUE: usize = 512;
/// One `0xC9` Opus frame: 5 ms at 48 kHz.
const AUDIO_FRAME_SAMPLES: usize = 240;

/// The live session, as the embedder should render it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoSession {
    /// Bumped on every new session and accepted mode switch: rebuild the encoder, start on an IDR.
    pub generation: u32,
    pub mode: Mode,
    /// [`quic::CODEC_H264`] or [`quic::CODEC_HEVC`].
    pub codec: u8,
    pub bitrate_kbps: u32,
    /// The library id the client asked to launch ([`Hello::launch`]).
    pub launch: Option<String>,
}

/// Annex-B access units and their keyframe bit, embedder → send thread.
type VideoQueue = mpsc::SyncSender<(Vec<u8>, bool)>;

/// The control stream's halves, `punktfunk/2`'s behind the translation edges.
type CtlSend = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;
type CtlRecv = io::MsgReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;

/// State one session owns, tagged with its connection's `stable_id` so a superseded session's
/// teardown cannot clear its successor.
#[derive(Default)]
struct Shared {
    session: Mutex<Option<(usize, DemoSession)>>,
    video: Mutex<Option<(usize, VideoQueue)>>,
    current: Mutex<Option<quinn::Connection>>,
    generation: AtomicU32,
    keyframe: AtomicBool,
    input: Mutex<VecDeque<InputEvent>>,
    /// Press counter; the audio plane chimes when it moves.
    presses: AtomicU32,
}

pub struct DemoHost {
    shared: Arc<Shared>,
    endpoint: quinn::Endpoint,
    port: u16,
    fingerprint: [u8; 32],
    rt: Option<tokio::runtime::Runtime>,
}

impl DemoHost {
    /// Listen on a free loopback port with a fresh certificate. `codecs` is the
    /// [`quic::CODEC_H264`]-family mask the embedder can encode.
    pub fn start(codecs: u8) -> Result<DemoHost> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pf-demo-host")
            .enable_all()
            .build()?;
        let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
            .map_err(|_| PunktfunkError::Crypto)?;
        let fingerprint = punktfunk_core::tls::cert_fingerprint(cert.cert.der());
        let (endpoint, media_socket) = {
            let _rt = rt.enter();
            endpoint::server_shared(
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                &cert.cert.pem(),
                &cert.signing_key.serialize_pem(),
                endpoint::DEFAULT_IDLE_TIMEOUT,
                &[registry::ALPN],
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?
        };
        let port = endpoint.local_addr()?.port();
        let shared = Arc::new(Shared::default());
        rt.spawn(accept_loop(
            endpoint.clone(),
            shared.clone(),
            codecs,
            Arc::new(media_socket),
        ));
        Ok(DemoHost {
            shared,
            endpoint,
            port,
            fingerprint,
            rt: Some(rt),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// SHA-256 of this host's certificate: the pin a client dials it with.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// The live session, or `None` while no client is streaming.
    pub fn session(&self) -> Option<DemoSession> {
        let session = self.shared.session.lock().unwrap();
        session.as_ref().map(|(_, s)| s.clone())
    }

    /// True once per client keyframe request (or dropped AU): encode the next frame as an IDR.
    pub fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::SeqCst)
    }

    /// Oldest unread input event from the client.
    pub fn next_input(&self) -> Option<InputEvent> {
        self.shared.input.lock().unwrap().pop_front()
    }

    /// Queue one Annex-B access unit (parameter sets in-band on an IDR). `false`: no session,
    /// or the queue was full and the AU dropped — the next frame should be an IDR.
    pub fn submit_video(&self, au: &[u8], keyframe: bool) -> bool {
        let video = self.shared.video.lock().unwrap();
        let Some((_, tx)) = video.as_ref() else {
            return false;
        };
        if tx.try_send((au.to_vec(), keyframe)).is_ok() {
            return true;
        }
        self.shared.keyframe.store(true, Ordering::SeqCst);
        false
    }
}

impl Drop for DemoHost {
    fn drop(&mut self) {
        if let Some(conn) = self.shared.current.lock().unwrap().take() {
            conn.close(0u32.into(), b"demo host stopped");
        }
        self.endpoint.close(0u32.into(), b"demo host stopped");
        // Disconnects the send thread's queue so it exits.
        *self.shared.video.lock().unwrap() = None;
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_millis(500));
        }
    }
}

/// The requested mode, scaled into [`MAX_WIDTH`]×[`MAX_HEIGHT`] at its aspect, even-sized.
fn demo_mode(asked: Mode) -> Mode {
    let (w, h) = (asked.width.max(2) as f64, asked.height.max(2) as f64);
    let scale = (MAX_WIDTH as f64 / w).min(MAX_HEIGHT as f64 / h).min(1.0);
    let even = |v: f64| ((v as u32) & !1).max(2);
    Mode {
        width: even(w * scale),
        height: even(h * scale),
        refresh_hz: match asked.refresh_hz {
            0 => MAX_REFRESH_HZ,
            hz => hz.min(MAX_REFRESH_HZ),
        },
    }
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    shared: Arc<Shared>,
    codecs: u8,
    media_socket: Arc<UdpSocket>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let (shared, media_socket) = (shared.clone(), media_socket.clone());
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            if let Err(e) = serve(&conn, &shared, codecs, &media_socket).await {
                tracing::debug!(error = %e, "demo session ended");
            }
            release(&shared, conn.stable_id());
        });
    }
}

/// Clear the shared slots if `id` still owns them.
fn release(shared: &Shared, id: usize) {
    let mut session = shared.session.lock().unwrap();
    if session.as_ref().is_some_and(|(owner, _)| *owner == id) {
        *session = None;
    }
    let mut video = shared.video.lock().unwrap();
    if video.as_ref().is_some_and(|(owner, _)| *owner == id) {
        *video = None;
    }
    let mut current = shared.current.lock().unwrap();
    if current.as_ref().is_some_and(|c| c.stable_id() == id) {
        *current = None;
    }
}

async fn serve(
    conn: &quinn::Connection,
    shared: &Arc<Shared>,
    codecs: u8,
    media_socket: &UdpSocket,
) -> Result<()> {
    let id = conn.stable_id();
    // A reachability probe closes before opening a stream; that ends here.
    let (send, mut recv) = conn.accept_bi().await.map_err(|_| PunktfunkError::Closed)?;
    let clock = Arc::new(SessionClock::new());
    let rx_edge = Arc::new(Mutex::new(RxEdge {
        clock: Some(clock.clone()),
        ..RxEdge::default()
    }));
    let tx_edge = Arc::new(Mutex::new(TxEdge::host(clock.clone())));
    if v2io::read_stream_type(&mut recv).await? != registry::STREAM_CONTROL {
        return Err(PunktfunkError::InvalidArg("first stream is not control"));
    }
    let recv: Box<dyn tokio::io::AsyncRead + Send + Unpin> =
        Box::new(v2io::V2Reader::new(recv, rx_edge.clone()));
    let mut send: CtlSend = Box::new(v2io::V2Writer::new(send, tx_edge.clone()));
    let mut recv: CtlRecv = io::MsgReader::new(recv);
    let hello = Hello::decode(&recv.read_msg().await?)?;
    // AES-GCM unless the client offers only ChaCha20-Poly1305.
    let suite = {
        let rx = rx_edge.lock().unwrap();
        let offered = rx
            .client
            .as_ref()
            .map(|c| c.suites.clone())
            .unwrap_or_default();
        match offered.first() {
            Some(&only) if !offered.contains(&MediaSuite::Aes128Gcm) => only,
            _ => MediaSuite::Aes128Gcm,
        }
    };
    let mut session_id = [0u8; 16];
    rand::rng().fill_bytes(&mut session_id);
    tx_edge.lock().unwrap().set_session(SessionFields {
        session_id,
        clock_origin_ns: clock.origin_ns(),
        suite: Some(suite),
    });
    let Some(codec) = quic::resolve_codec(hello.video_codecs, codecs, hello.preferred_codec) else {
        conn.close(0u32.into(), b"no shared codec");
        return Err(PunktfunkError::Unsupported("no codec the demo can encode"));
    };
    if let Some(old) = shared.current.lock().unwrap().replace(conn.clone()) {
        old.close(0u32.into(), b"superseded");
    }

    let mode = demo_mode(hello.mode);
    let bitrate_kbps = match hello.bitrate_kbps {
        0 => DEFAULT_BITRATE_KBPS,
        kbps => kbps.clamp(2_000, 40_000),
    };
    let welcome = Welcome {
        abi_version: punktfunk_core::WIRE_VERSION,
        udp_port: 0,
        mode,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 10,
            max_data_per_block: 4096,
        },
        shard_payload: match hello.max_shard_payload {
            0 => 1408,
            max => max.min(1408),
        },
        // The media keys come from the connection's exporter.
        encrypt: false,
        key: [0; 16],
        salt: [0; 4],
        frames: 0,
        compositor: CompositorPref::Auto,
        gamepad: hello.gamepad,
        bitrate_kbps,
        bit_depth: 8,
        color: quic::ColorInfo::SDR_BT709,
        chroma_format: quic::CHROMA_IDC_420,
        audio_channels: 2,
        codec,
        host_caps: quic::HOST_CAP_GAMEPAD_STATE,
        cipher: quic::CIPHER_AES_128_GCM,
        mgmt_port: 0,
        grants: quic::GRANT_ALL,
        expires_in_secs: 0,
        key_chacha: None,
        audio_codec: quic::AUDIO_CODEC_OPUS,
        audio_rate_hz: SAMPLE_RATE_HZ,
        audio_bits: punktfunk_core::audio::pcm::BITS_16,
        audio_frame_us: 0,
        host_caps2: 0,
        audio_layout: 0,
    };
    io::write_msg(&mut send, &welcome.encode()).await?;
    Start::decode(&recv.read_msg().await?)?;

    let (video_tx, video_rx) = mpsc::sync_channel(VIDEO_QUEUE);
    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    shared.input.lock().unwrap().clear();
    *shared.session.lock().unwrap() = Some((
        id,
        DemoSession {
            generation,
            mode,
            codec,
            bitrate_kbps,
            launch: hello.launch.clone(),
        },
    ));
    *shared.video.lock().unwrap() = Some((id, video_tx));
    shared.keyframe.store(true, Ordering::SeqCst);
    tracing::info!(
        width = mode.width,
        height = mode.height,
        hz = mode.refresh_hz,
        codec,
        launch = hello.launch.as_deref().unwrap_or("-"),
        "demo session started"
    );

    let config = welcome.session_config(Role::Host);
    let keys = endpoint::media_keys(conn, &session_id, suite).ok_or(PunktfunkError::Crypto)?;
    let sender = MediaSender::new(media_socket, conn.clone())?;
    let media = MediaV2 {
        clock_origin_ns: clock.origin_ns(),
        keys: Some(keys),
        clock: Some(clock.clone()),
    };
    let open: OpenSession = Box::new(move || Session::new_v2(config, media, Box::new(sender)));
    let video = tokio::task::spawn_blocking(move || send_video(open, video_rx));
    let dgrams = Dgrams { conn, clock };
    tokio::select! {
        r = control_loop(send, recv, shared, id) => r,
        () = input_loop(&dgrams, shared) => Ok(()),
        r = audio_loop(&dgrams, shared) => r,
        r = video => r.map_err(|_| PunktfunkError::Closed)?,
    }
}

/// Opens the video session on the send thread.
type OpenSession = Box<dyn FnOnce() -> Result<Session> + Send>;

/// The connection's datagrams: each carries a kind, and host stamps leave in session time.
struct Dgrams<'a> {
    conn: &'a quinn::Connection,
    clock: Arc<SessionClock>,
}

impl Dgrams<'_> {
    /// `false` once the connection is gone.
    fn send(&self, mut d: Vec<u8>) -> bool {
        self.clock.retime_datagram(&mut d);
        let Some(w) = dgram::wrap(&d) else {
            return true;
        };
        self.conn.send_datagram(w.into()).is_ok()
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        use dgram::Dgram;
        loop {
            let b = self.conn.read_datagram().await.ok()?;
            if let Some(Dgram::Audio(p) | Dgram::InputState(p) | Dgram::HostEvent(p)) =
                dgram::decode(&b)
            {
                return Some(p.to_vec());
            }
        }
    }
}

fn send_video(open: OpenSession, rx: mpsc::Receiver<(Vec<u8>, bool)>) -> Result<()> {
    let mut session = open()?;
    for (au, keyframe) in rx {
        let flags = if keyframe {
            FLAG_PIC | FLAG_SOF
        } else {
            FLAG_PIC
        };
        session.submit_frame(&au, wall_clock_ns(), u32::from(flags))?;
    }
    Ok(())
}

async fn control_loop(
    mut send: CtlSend,
    mut recv: CtlRecv,
    shared: &Shared,
    id: usize,
) -> Result<()> {
    loop {
        let msg = recv.read_msg().await?;
        if let Ok(probe) = ClockProbe::decode(&msg) {
            let t2_ns = wall_clock_ns();
            let echo = ClockEcho {
                t1_ns: probe.t1_ns,
                t2_ns,
                t3_ns: wall_clock_ns(),
            };
            io::write_msg(&mut send, &echo.encode()).await?;
        } else if RequestKeyframe::decode(&msg).is_ok() || RfiRequest::decode(&msg).is_ok() {
            shared.keyframe.store(true, Ordering::SeqCst);
        } else if let Ok(asked) = Reconfigure::decode(&msg) {
            let mode = demo_mode(asked.mode);
            if let Some((owner, s)) = shared.session.lock().unwrap().as_mut() {
                if *owner == id {
                    s.mode = mode;
                    s.generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
                }
            }
            shared.keyframe.store(true, Ordering::SeqCst);
            let answer = Reconfigured {
                accepted: true,
                mode,
            };
            io::write_msg(&mut send, &answer.encode()).await?;
        }
        // Everything else (loss reports, bitrate asks, speed tests) has no demo answer.
    }
}

async fn input_loop(dgrams: &Dgrams<'_>, shared: &Shared) {
    let mut pad_buttons = [0u32; 16];
    while let Some(dg) = dgrams.recv().await {
        if dg.first() != Some(&INPUT_MAGIC) {
            continue;
        }
        let Some(ev) = InputEvent::decode(&dg) else {
            continue;
        };
        if is_press(&ev, &mut pad_buttons) {
            shared.presses.fetch_add(1, Ordering::SeqCst);
        }
        let mut input = shared.input.lock().unwrap();
        if input.len() == INPUT_QUEUE {
            input.pop_front();
        }
        input.push_back(ev);
    }
}

/// A key, button or touch going down. Gamepad snapshots count a newly set button bit.
fn is_press(ev: &InputEvent, pad_buttons: &mut [u32; 16]) -> bool {
    match ev.kind {
        InputKind::KeyDown | InputKind::MouseButtonDown | InputKind::TouchDown => true,
        InputKind::GamepadButton => ev.x != 0,
        InputKind::GamepadState => {
            let Some(snap) = GamepadSnapshot::from_event(ev) else {
                return false;
            };
            let slot = &mut pad_buttons[usize::from(snap.pad) % 16];
            let pressed = snap.buttons & !*slot != 0;
            *slot = snap.buttons;
            pressed
        }
        _ => false,
    }
}

async fn audio_loop(dgrams: &Dgrams<'_>, shared: &Shared) -> Result<()> {
    let layout = LAYOUT_STEREO;
    let mut encoder = opus::MSEncoder::new(
        SAMPLE_RATE_HZ,
        layout.streams,
        layout.coupled,
        layout.mapping,
        opus::Application::LowDelay,
    )
    .map_err(|_| PunktfunkError::Unsupported("opus encoder"))?;
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pcm = [0f32; AUDIO_FRAME_SAMPLES * 2];
    let mut packet = vec![0u8; 1500];
    let mut seen = shared.presses.load(Ordering::SeqCst);
    let (mut seq, mut t) = (0u32, usize::MAX);
    loop {
        tick.tick().await;
        let presses = shared.presses.load(Ordering::SeqCst);
        if presses != seen {
            seen = presses;
            t = 0;
        }
        for frame in pcm.chunks_exact_mut(2) {
            let s = chime(t);
            frame[0] = s;
            frame[1] = s;
            t = t.saturating_add(1);
        }
        let n = encoder
            .encode_float(&pcm, &mut packet)
            .map_err(|_| PunktfunkError::Unsupported("opus encode"))?;
        let dg = quic::encode_audio_datagram(seq, wall_clock_ns(), &packet[..n]);
        if !dgrams.send(dg) {
            return Ok(());
        }
        seq = seq.wrapping_add(1);
    }
}

/// Sample `t` of a 300 ms two-partial bell; silence after it.
fn chime(t: usize) -> f32 {
    const LEN: usize = SAMPLE_RATE_HZ as usize * 3 / 10;
    if t >= LEN {
        return 0.0;
    }
    let secs = t as f32 / SAMPLE_RATE_HZ as f32;
    let envelope = (1.0 - t as f32 / LEN as f32).powi(3);
    let tau = std::f32::consts::TAU;
    0.12 * envelope * ((tau * 880.0 * secs).sin() + 0.5 * (tau * 1320.0 * secs).sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_mode_fits_1080p_at_the_asked_aspect() {
        let m = demo_mode(Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        });
        assert_eq!((m.width, m.height, m.refresh_hz), (1920, 1080, 60));
        let m = demo_mode(Mode {
            width: 2732,
            height: 2048,
            refresh_hz: 0,
        });
        assert!(m.width <= MAX_WIDTH && m.height <= MAX_HEIGHT);
        assert_eq!((m.width % 2, m.height % 2, m.refresh_hz), (0, 0, 60));
        let m = demo_mode(Mode {
            width: 1280,
            height: 720,
            refresh_hz: 30,
        });
        assert_eq!((m.width, m.height, m.refresh_hz), (1280, 720, 30));
    }

    #[test]
    fn a_pad_snapshot_presses_only_on_a_new_button() {
        let mut pads = [0u32; 16];
        let snap = |buttons| {
            GamepadSnapshot {
                buttons,
                ..Default::default()
            }
            .to_event()
        };
        assert!(is_press(&snap(1), &mut pads));
        assert!(
            !is_press(&snap(1), &mut pads),
            "a held button is not a new press"
        );
        assert!(is_press(&snap(3), &mut pads));
        assert!(!is_press(&snap(0), &mut pads), "a release is not a press");
    }

    /// A real client dials the demo host pinned, streams, and its input comes back.
    #[test]
    fn a_native_client_streams_from_the_demo_host() {
        let host = DemoHost::start(quic::CODEC_H264).expect("demo host");
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let client =
            punktfunk_core::client::NativeClient::connect(punktfunk_core::client::ConnectParams {
                video_codecs: quic::CODEC_H264 | quic::CODEC_HEVC,
                launch: Some("custom:aurora".into()),
                pin: Some(host.fingerprint()),
                ..punktfunk_core::client::ConnectParams::new(
                    "127.0.0.1",
                    host.port(),
                    mode,
                    Duration::from_secs(10),
                )
            })
            .expect("connect");
        let session = host.session().expect("a live session");
        assert_eq!(session.codec, quic::CODEC_H264);
        assert_eq!(session.launch.as_deref(), Some("custom:aurora"));
        assert_eq!((session.mode.width, session.mode.height), (1280, 720));

        // Media opens after the clock handshake; resend until a frame arrives.
        let au = vec![0x5a; 30_000];
        let frame = (0..100)
            .find_map(|_| {
                host.submit_video(&au, true);
                client.next_frame(Duration::from_millis(100)).ok()
            })
            .expect("a frame through the data plane");
        assert_eq!(frame.data.len(), au.len());

        let key = InputEvent {
            kind: InputKind::KeyDown,
            _pad: [0; 3],
            code: 0x41,
            x: 0,
            y: 0,
            flags: 0,
        };
        client.send_input(&key).expect("send input");
        let echoed = (0..50)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                host.next_input()
            })
            .expect("input reaches the demo host");
        assert_eq!((echoed.kind, echoed.code), (InputKind::KeyDown, 0x41));
        assert!(
            client.next_audio(Duration::from_secs(2)).is_ok(),
            "the audio plane runs"
        );

        client.disconnect_quit();
        drop(client);
        let ended = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            host.session().is_none()
        });
        assert!(ended, "a quit client ends the demo session");
    }
}
