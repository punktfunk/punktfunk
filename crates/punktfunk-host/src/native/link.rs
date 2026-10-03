//! The session's control connection, whichever transport carries it.
//!
//! The native plane rides quinn; a browser rides WebTransport (`design/web-client.md`). Video was
//! always portable — `Session` holds a `Box<dyn Transport>` — but audio, cursor, rumble and HID
//! out call `send_datagram` on the connection directly, and the handshake and control plane are
//! streams on it. Those call sites named `quinn::Connection`, which is what kept a browser to the
//! video half.
//!
//! **An enum, not a trait.** Three variants, all known at compile time, so no `dyn` — and, the
//! reason that matters, `async fn` stays an ordinary `async fn`. `punktfunk/2` is quinn with
//! its datagrams kinded and its control stream translated at the edge. A `dyn`-compatible trait would
//! box a future on `closed()`, which is on the per-session path.
//!
//! Most of this delegates rather than branches: WebTransport *is* QUIC, and `wtransport` hands
//! out the `quinn::Connection` underneath, so path and lifecycle questions have one answer for
//! both. Only the two genuinely carrier-shaped questions branch.

use punktfunk_core::quic::v2::io::{V2Reader, V2Writer};
use punktfunk_core::quic::v2::translate::{RxEdge, SessionFields, TxEdge};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The control stream's write half, whichever carrier. Delegates every poll, because both
/// halves already implement tokio's traits — this exists so the session code can name one type.
pub(crate) enum CtlSend {
    Quic(quinn::SendStream),
    /// `punktfunk/2`: v1 messages in, v2 frames on the wire.
    QuicV2(V2Writer<quinn::SendStream>),
    Web(wtransport::SendStream),
}

/// The control stream's read half. See [`CtlSend`].
pub(crate) enum CtlRecv {
    Quic(quinn::RecvStream),
    /// `punktfunk/2`: v2 frames on the wire, v1 messages out.
    QuicV2(V2Reader<quinn::RecvStream>),
    Web(wtransport::RecvStream),
}

impl AsyncWrite for CtlSend {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
            CtlSend::QuicV2(s) => Pin::new(s).poll_write(cx, buf),
            CtlSend::Web(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
            CtlSend::QuicV2(s) => Pin::new(s).poll_flush(cx),
            CtlSend::Web(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlSend::Quic(s) => AsyncWrite::poll_shutdown(Pin::new(s), cx),
            CtlSend::QuicV2(s) => Pin::new(s).poll_shutdown(cx),
            CtlSend::Web(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for CtlRecv {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            CtlRecv::Quic(s) => AsyncRead::poll_read(Pin::new(s), cx, buf),
            CtlRecv::QuicV2(s) => Pin::new(s).poll_read(cx, buf),
            CtlRecv::Web(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

/// What accepting the control stream produced.
pub(crate) enum Accepted {
    Stream(CtlSend, CtlRecv),
    /// A `punktfunk/2` connection opened for management: its first stream is a request, and
    /// no session follows.
    Management(quinn::SendStream, quinn::RecvStream),
    /// A clean application close before any stream: a reachability probe, not a client.
    ProbeClose,
}

/// A `punktfunk/2` session's state beside its connection: who it is, its media clock, and the
/// edges that translate its control stream.
pub(crate) struct V2Link {
    pub conn: quinn::Connection,
    pub session_id: [u8; 16],
    /// The host instant, Unix ns, that media capture time 0 stands for.
    pub clock_origin_ns: u64,
    pub rx: Arc<Mutex<RxEdge>>,
    pub tx: Arc<Mutex<TxEdge>>,
    /// A clone of the endpoint's socket: media leaves from the address the client dialed.
    pub media_socket: Arc<std::net::UdpSocket>,
    suite: Mutex<Option<punktfunk_core::crypto::MediaSuite>>,
}

impl V2Link {
    pub(crate) fn new(conn: quinn::Connection, media_socket: Arc<std::net::UdpSocket>) -> V2Link {
        let mut session_id = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut session_id);
        V2Link {
            conn,
            session_id,
            clock_origin_ns: punktfunk_core::quic::wall_clock_ns(),
            rx: Arc::default(),
            tx: Arc::default(),
            media_socket,
            suite: Mutex::new(None),
        }
    }

    /// The media AEAD, fixed by the handshake before `Welcome` leaves: it goes out in the
    /// `ServerHello` and keys the media.
    pub(crate) fn settle(&self, suite: punktfunk_core::crypto::MediaSuite) {
        *self.suite.lock().unwrap_or_else(|e| e.into_inner()) = Some(suite);
        self.tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_session(SessionFields {
                session_id: self.session_id,
                clock_origin_ns: self.clock_origin_ns,
                suite: Some(suite),
            });
    }

    pub(crate) fn suite(&self) -> Option<punktfunk_core::crypto::MediaSuite> {
        *self.suite.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One client's control connection. Cheap to clone — every variant is a handle.
#[derive(Clone)]
pub(crate) enum SessionLink {
    /// The native plane: `punktfunk/1` over quinn.
    Quic(quinn::Connection),
    /// The native plane on `punktfunk/2`: datagrams carry a kind, the control stream is
    /// translated at its edge, and media rides the connection's own socket.
    QuicV2(quinn::Connection, Arc<V2Link>),
    /// The browser plane: the same protocol over one WebTransport session, and the device
    /// fingerprint its key signature proved. `None` before admission and under `serve --open`.
    Web(wtransport::Connection, Option<[u8; 32]>),
}

impl SessionLink {
    /// The QUIC connection underneath, which both carriers have.
    pub(crate) fn quic(&self) -> &quinn::Connection {
        match self {
            SessionLink::Quic(c) | SessionLink::QuicV2(c, _) => c,
            SessionLink::Web(c, _) => c.quic_connection(),
        }
    }

    /// Smoothed round trip of the connection underneath.
    pub(crate) fn rtt(&self) -> std::time::Duration {
        self.quic().rtt()
    }

    /// Unreliable datagram: audio, cursor, rumble, HID out.
    ///
    /// The three outcomes callers actually act on, because they act differently: a frame too big
    /// for this path is dropped and the plane continues, while datagrams being unavailable at all
    /// ends the plane rather than pacing a wire that cannot take it.
    pub(crate) fn send_datagram(&self, payload: Vec<u8>) -> DatagramSend {
        match self {
            SessionLink::Quic(c) => match c.send_datagram(payload.into()) {
                Ok(()) => DatagramSend::Sent,
                Err(quinn::SendDatagramError::TooLarge) => DatagramSend::TooLarge,
                Err(_) => DatagramSend::Unavailable,
            },
            // Every datagram the host sends has a kind; one without is a bug, dropped here.
            SessionLink::QuicV2(c, _) => match punktfunk_core::quic::v2::dgram::wrap(&payload) {
                Some(w) => match c.send_datagram(w.into()) {
                    Ok(()) => DatagramSend::Sent,
                    Err(quinn::SendDatagramError::TooLarge) => DatagramSend::TooLarge,
                    Err(_) => DatagramSend::Unavailable,
                },
                None => DatagramSend::TooLarge,
            },
            // Not the quinn connection: a WebTransport datagram carries a session-id prefix, so
            // it has to go through the layer that writes one.
            SessionLink::Web(c, _) => match c.send_datagram(&payload) {
                Ok(()) => DatagramSend::Sent,
                Err(wtransport::error::SendDatagramError::TooLarge) => DatagramSend::TooLarge,
                Err(_) => DatagramSend::Unavailable,
            },
        }
    }

    /// The next datagram from the peer — mic, rich input, pen. `Err` once the peer is gone.
    pub(crate) async fn read_datagram(&self) -> Result<Vec<u8>, LinkClosed> {
        match self {
            SessionLink::Quic(c) => c
                .read_datagram()
                .await
                .map(|b| b.to_vec())
                .map_err(LinkClosed::from),
            // The payload is the datagram the session logic already reads. A kind it does not
            // take from a client is skipped.
            SessionLink::QuicV2(c, _) => loop {
                let b = c.read_datagram().await.map_err(LinkClosed::from)?;
                use punktfunk_core::quic::v2::dgram::{decode, Dgram};
                match decode(&b) {
                    Some(Dgram::Audio(p) | Dgram::InputState(p) | Dgram::HostEvent(p)) => {
                        break Ok(p.to_vec())
                    }
                    _ => continue,
                }
            },
            SessionLink::Web(c, _) => c
                .receive_datagram()
                .await
                .map(|d| d.payload().to_vec())
                .map_err(LinkClosed::from),
        }
    }

    /// Largest datagram this path will carry. Lower on the browser plane — HTTP/3 framing comes
    /// out of the same budget — which is why callers must ask rather than assume 1500-MTU maths.
    pub(crate) fn max_datagram_size(&self) -> Option<usize> {
        match self {
            SessionLink::Quic(c) => c.max_datagram_size(),
            // The kind byte comes out of the same budget.
            SessionLink::QuicV2(c, _) => c.max_datagram_size().map(|n| n.saturating_sub(1)),
            SessionLink::Web(c, _) => c.max_datagram_size(),
        }
    }

    pub(crate) fn remote_address(&self) -> SocketAddr {
        self.quic().remote_address()
    }

    /// Local address the connection arrived on, for binding a data socket on the same NIC.
    pub(crate) fn local_ip(&self) -> Option<IpAddr> {
        self.quic().local_ip()
    }

    /// Path MTU as the stack currently believes it.
    pub(crate) fn current_mtu(&self) -> u16 {
        self.quic().stats().path.current_mtu
    }

    /// `Some` once the connection has ended, without awaiting.
    pub(crate) fn close_reason(&self) -> Option<LinkClosed> {
        self.quic().close_reason().map(LinkClosed::from)
    }

    pub(crate) fn close(&self, code: u32, reason: &[u8]) {
        self.quic().close(code.into(), reason);
    }

    /// Close with a typed code and its reason. A browser is never shown a close reason, so it
    /// gets the same code and text on a stream first.
    pub(crate) async fn refuse(&self, code: u32, reason: &str) {
        if let SessionLink::Web(c, _) = self {
            crate::webtransport::refuse(c, code, reason).await;
        }
        self.close(code, reason.as_bytes());
    }

    /// Resolves when the peer is gone.
    ///
    /// A page's `close({closeCode})` ends the WebTransport session, not the QUIC connection
    /// under it, which the driver then closes with a code of its own. So a browser's close is
    /// read from the session. The page never opens a unidirectional stream, so this accept only
    /// ever returns the session's end.
    pub(crate) async fn closed(&self) -> LinkClosed {
        match self {
            SessionLink::Quic(c) | SessionLink::QuicV2(c, _) => LinkClosed::from(c.closed().await),
            SessionLink::Web(c, _) => loop {
                if let Err(e) = c.accept_uni().await {
                    break LinkClosed::from(e);
                }
            },
        }
    }

    /// The peer's first bidirectional stream — the control stream on both carriers, or on
    /// `punktfunk/2` a management request.
    ///
    /// A clean close before any stream is a reachability probe, and is reported rather than
    /// failed. Anything else that is not a stream is the error it was.
    pub(crate) async fn accept_bi(&self) -> anyhow::Result<Accepted> {
        match self {
            SessionLink::Quic(c) => match c.accept_bi().await {
                Ok((send, recv)) => Ok(Accepted::Stream(CtlSend::Quic(send), CtlRecv::Quic(recv))),
                Err(quinn::ConnectionError::ApplicationClosed(ref ac))
                    if ac.error_code == quinn::VarInt::from_u32(0) =>
                {
                    Ok(Accepted::ProbeClose)
                }
                Err(e) => Err(anyhow::Error::new(e).context("accept control stream")),
            },
            // The client's first stream must say it is the control stream or a management one.
            SessionLink::QuicV2(c, v2) => match c.accept_bi().await {
                Ok((send, mut recv)) => {
                    use punktfunk_core::quic::v2::{io, registry};
                    let ty = io::read_stream_type(&mut recv)
                        .await
                        .map_err(|e| anyhow::anyhow!("read control stream type: {e}"))?;
                    if ty == registry::STREAM_MANAGEMENT {
                        return Ok(Accepted::Management(send, recv));
                    }
                    anyhow::ensure!(
                        ty == registry::STREAM_CONTROL,
                        "first stream is type {ty}, not control"
                    );
                    Ok(Accepted::Stream(
                        CtlSend::QuicV2(V2Writer::new(send, v2.tx.clone())),
                        CtlRecv::QuicV2(V2Reader::new(recv, v2.rx.clone())),
                    ))
                }
                Err(quinn::ConnectionError::ApplicationClosed(ref ac))
                    if ac.error_code == quinn::VarInt::from_u32(0) =>
                {
                    Ok(Accepted::ProbeClose)
                }
                Err(e) => Err(anyhow::Error::new(e).context("accept control stream")),
            },
            SessionLink::Web(c, _) => {
                let (send, recv) = c
                    .accept_bi()
                    .await
                    .map_err(|e| anyhow::anyhow!("accept control stream: {e:?}"))?;
                Ok(Accepted::Stream(CtlSend::Web(send), CtlRecv::Web(recv)))
            }
        }
    }

    /// The quinn connection, for clipboard transfers on either native wire. `None` on WebTransport.
    pub(crate) fn as_quic(&self) -> Option<&quinn::Connection> {
        match self {
            SessionLink::Quic(c) | SessionLink::QuicV2(c, _) => Some(c),
            SessionLink::Web(..) => None,
        }
    }

    /// The `punktfunk/2` state, on that wire.
    pub(crate) fn v2(&self) -> Option<&Arc<V2Link>> {
        match self {
            SessionLink::QuicV2(_, v2) => Some(v2),
            _ => None,
        }
    }

    /// The device this session is keyed by: the client certificate's fingerprint on the native
    /// plane, the admitted device key's on the browser plane. `None` for an anonymous client.
    pub(crate) fn peer_fingerprint(&self) -> Option<[u8; 32]> {
        match self {
            SessionLink::Quic(c) | SessionLink::QuicV2(c, _) => {
                punktfunk_core::quic::endpoint::peer_fingerprint(c)
            }
            SessionLink::Web(_, fp) => *fp,
        }
    }

    /// The plane events and session rows name this session by.
    pub(crate) fn plane(&self) -> crate::events::Plane {
        match self {
            SessionLink::Quic(_) | SessionLink::QuicV2(..) => crate::events::Plane::Native,
            SessionLink::Web(..) => crate::events::Plane::Web,
        }
    }

    /// Whether a browser is on the other end. For the few decisions that really are about the
    /// carrier: there is no second UDP plane to punch, and the capabilities that ride quinn
    /// streams are not on offer.
    pub(crate) fn is_web(&self) -> bool {
        matches!(self, SessionLink::Web(..))
    }
}

/// What became of one datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatagramSend {
    Sent,
    /// Over this path's datagram ceiling. The frame is lost; the plane carries on, and a caller
    /// that sees these should be resizing what it sends rather than retrying.
    TooLarge,
    /// The peer will take no more datagrams for the rest of the connection.
    Unavailable,
}

impl DatagramSend {
    pub(crate) fn is_sent(self) -> bool {
        self == DatagramSend::Sent
    }
}

/// Why a connection ended, in the terms callers actually branch on: our own close codes ride an
/// application close, and a timeout is the one transport failure worth telling apart from the
/// rest. Everything else is noise for a log line.
#[derive(Clone, Debug)]
pub(crate) enum LinkClosed {
    /// The peer closed with an application code — where `QUIT_CODE` and the reject codes live.
    App {
        code: u64,
        reason: String,
    },
    /// The path went quiet. Distinguished because a client that timed out did not choose to leave.
    TimedOut,
    Other(String),
}

impl LinkClosed {
    /// The application close code, if that is how it ended.
    pub(crate) fn app_code(&self) -> Option<u64> {
        match self {
            LinkClosed::App { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Did the peer close with this application code?
    pub(crate) fn closed_with(&self, code: u32) -> bool {
        self.app_code() == Some(u64::from(code))
    }
}

impl From<quinn::ConnectionError> for LinkClosed {
    fn from(e: quinn::ConnectionError) -> LinkClosed {
        match e {
            quinn::ConnectionError::ApplicationClosed(ref ac) => LinkClosed::App {
                code: ac.error_code.into_inner(),
                reason: String::from_utf8_lossy(&ac.reason).into_owned(),
            },
            quinn::ConnectionError::TimedOut => LinkClosed::TimedOut,
            other => LinkClosed::Other(other.to_string()),
        }
    }
}

impl From<wtransport::error::ConnectionError> for LinkClosed {
    fn from(e: wtransport::error::ConnectionError) -> LinkClosed {
        use wtransport::error::ConnectionError;
        match e {
            ConnectionError::ApplicationClosed(ac) => LinkClosed::App {
                code: ac.code().into_inner(),
                reason: String::from_utf8_lossy(ac.reason()).into_owned(),
            },
            ConnectionError::TimedOut => LinkClosed::TimedOut,
            other => LinkClosed::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for LinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkClosed::App { code, reason } if reason.is_empty() => {
                write!(f, "closed by peer (code {code})")
            }
            LinkClosed::App { code, reason } => write!(f, "closed by peer (code {code}): {reason}"),
            LinkClosed::TimedOut => f.write_str("timed out"),
            LinkClosed::Other(s) => f.write_str(s),
        }
    }
}

impl From<quinn::Connection> for SessionLink {
    fn from(c: quinn::Connection) -> SessionLink {
        SessionLink::Quic(c)
    }
}
