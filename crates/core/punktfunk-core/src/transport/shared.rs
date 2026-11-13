//! One UDP socket for QUIC and `punktfunk/2` media. A packet whose first byte has its two top
//! bits clear is media; anything else is QUIC (RFC 9443). The client's endpoint does not offer
//! QUIC bit greasing, so the host never clears those bits on a QUIC packet toward it.
//!
//! Client: [`client_socket`] starts a native thread that owns every read. QUIC packets go to
//! quinn through the [`ClientSocket`] it returns; media from the host's address goes to the
//! session through [`ClientMedia`], a bounded queue. Video never enters an async runtime.
//! On Apple the socket connects to the host once it is known ([`ClientMedia::set_host`]), and
//! again after a second of silence or a path change ([`ClientMedia::reconnector`]).
//!
//! Host: [`MediaSender`] sends media on a clone of the endpoint's socket, toward the
//! connection's current address and from the address the client dialed. quinn moves that
//! address on the first authenticated packet from a new one and validates it afterwards. Media
//! follows a port-only rebind at once and a new IP once validation has had time to finish.

use super::{BufPool, RecvBatch, Transport};
use quinn::udp::{RecvMeta, Transmit, UdpSocketState};
use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// Whether a datagram on the shared socket is media rather than QUIC.
pub fn is_media(first_byte: u8) -> bool {
    first_byte & 0xC0 == 0
}

/// Media batches the client queues for the session; past this it drops the newest, as a full
/// socket buffer would.
const MEDIA_QUEUE: usize = 8192;
/// QUIC packets queued for quinn; quinn drains them on every wake.
const QUIC_QUEUE: usize = 1024;
/// Datagrams one read takes, and the buffer each needs with GRO.
const BATCH: usize = 32;
const READ_BUF: usize = 64 * 1024;
/// Receive buffers out at once, read and queued, in bytes: 512 with GRO, ~3600 datagrams without.
const POOL_BYTES: usize = 32 * 1024 * 1024;
/// How long the reader waits before it looks at its stop flag again.
const WAIT: Duration = Duration::from_millis(100);
/// Empty waits in a row that read as a dead path: one second. A live host sends media, audio
/// or a keep-alive well inside it.
const SILENT_WAITS: u32 = 10;

/// The client socket's counters, and the flag that stops its reader.
#[derive(Debug, Default)]
pub struct SharedStats {
    stop: AtomicBool,
    host: Mutex<Option<SocketAddr>>,
    /// Media packets handed to the session.
    pub media: AtomicU64,
    /// Media packets dropped because the session's queue was full.
    pub media_dropped: AtomicU64,
    /// Media-shaped packets from an address that is not the host's.
    pub foreign: AtomicU64,
    /// QUIC packets handed to quinn.
    pub quic: AtomicU64,
    /// QUIC packets dropped because quinn's queue was full.
    pub quic_dropped: AtomicU64,
    // The calls the socket refused, by kind.
    send_failures: Failures,
    read_failures: Failures,
    wait_failures: Failures,
    connect_failures: Failures,
}

impl SharedStats {
    /// Accept media from `host` only. Until it is set, every media packet is foreign.
    fn set_host(&self, host: SocketAddr) {
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) = Some(canonical(host));
    }

    fn is_host(&self, from: SocketAddr) -> bool {
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) == Some(canonical(from))
    }

    fn host_known(&self) -> bool {
        self.host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// One line per silent second. Failed calls beside it mean a dead socket; none means
    /// nothing reached it.
    fn log_silence(&self, silent_s: u32) {
        tracing::warn!(
            silent_s,
            send_failures = self.send_failures.total(),
            read_failures = self.read_failures.total(),
            wait_failures = self.wait_failures.total(),
            connect_failures = self.connect_failures.total(),
            quic = self.quic.load(Ordering::Relaxed),
            media = self.media.load(Ordering::Relaxed),
            "shared socket silent"
        );
    }
}

/// One kind of call the socket refused. The first refusal logs, then at most one line a
/// second, so a socket that refuses every call cannot flood the log.
#[derive(Debug, Default)]
struct Failures {
    total: AtomicU64,
    /// When the last line went out, and the total it reported.
    logged: Mutex<Option<(Instant, u64)>>,
}

impl Failures {
    fn note(&self, call: &'static str, e: &std::io::Error) {
        let total = self.total.fetch_add(1, Ordering::Relaxed) + 1;
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if logged.is_some_and(|(at, _)| now.duration_since(at) < Duration::from_secs(1)) {
            return;
        }
        let since_last = total - logged.map_or(0, |(_, n)| n);
        *logged = Some((now, total));
        tracing::warn!(call, error = %e, since_last, total, "shared socket call failed");
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

/// IPv4 as itself, whether the socket reported it plain or v4-mapped.
fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

/// `a` as a socket of the other family sends to it: IPv4 v4-mapped on an IPv6 socket.
fn mapped(a: SocketAddr, v6_socket: bool) -> SocketAddr {
    match a {
        SocketAddr::V4(v4) if v6_socket => {
            SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
        }
        other => other,
    }
}

struct QuicQueue {
    packets: VecDeque<(Vec<u8>, RecvMeta)>,
    waker: Option<Waker>,
}

/// The host the socket is connected to. A connected socket refuses an addressed send.
type Peer = Arc<Mutex<Option<SocketAddr>>>;

/// The quinn side of the client's shared socket.
pub struct ClientSocket {
    socket: Arc<UdpSocket>,
    state: Arc<UdpSocketState>,
    quic: Arc<Mutex<QuicQueue>>,
    stats: Arc<SharedStats>,
    peer: Peer,
}

impl std::fmt::Debug for ClientSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientSocket")
            .field("local", &self.socket.local_addr().ok())
            .finish()
    }
}

impl Drop for ClientSocket {
    fn drop(&mut self) {
        self.stats.stop.store(true, Ordering::Relaxed);
    }
}

/// The session side of the client's shared socket: media packets, in arrival order.
pub struct ClientMedia {
    /// One reader, the session's pump; the lock is never contended.
    rx: Mutex<Receiver<RecvBatch>>,
    stats: Arc<SharedStats>,
    socket: Arc<UdpSocket>,
    peer: Peer,
    /// The uplink's QoS mark, held for the host's lifetime (a dropped Windows flow unmarks).
    qos: Mutex<Option<super::QosFlow>>,
}

/// Connect the socket to its host again, so XNU picks the source address afresh after a roam;
/// a connected socket otherwise keeps the old one until a send fails. A no-op before the host
/// is known, and off Apple, where the socket stays unconnected and routes every send. A
/// refusal is logged.
fn reconnect(socket: &UdpSocket, peer: &Peer, stats: &SharedStats) {
    let peer = peer.lock().unwrap_or_else(|e| e.into_inner());
    if let (true, Some(host)) = (cfg!(target_vendor = "apple"), *peer) {
        if let Err(e) = socket.connect(host) {
            stats.connect_failures.note("connect", &e);
        }
    }
}

impl ClientMedia {
    pub fn stats(&self) -> &Arc<SharedStats> {
        &self.stats
    }

    /// Accept media from `host` only, and mark what goes up to it (input, acks, the mic) as
    /// audio-class QoS, the same rule as the host's own marks: private peers, unless forced.
    /// On Apple the socket also connects to it: XNU sheds an unconnected UDP socket's datagrams
    /// past ~150k a second (1.7 Gbit/s of video), uncounted.
    pub fn set_host(&self, host: SocketAddr) {
        let host = canonical(host);
        self.stats.set_host(host);
        *self.qos.lock().unwrap_or_else(|e| e.into_inner()) =
            super::set_media_qos_to(&self.socket, host, super::MediaClass::Audio);
        #[cfg(target_vendor = "apple")]
        {
            let mut peer = self.peer.lock().unwrap_or_else(|e| e.into_inner());
            match self.socket.connect(host) {
                Ok(()) => *peer = Some(host),
                Err(e) => tracing::warn!(error = %e, "shared socket stays unconnected"),
            }
        }
    }

    /// Re-picks the socket's source address, for a caller that learns the path changed.
    pub fn reconnector(&self) -> impl Fn() + Send + Sync + 'static {
        let (socket, peer, stats) = (self.socket.clone(), self.peer.clone(), self.stats.clone());
        move || reconnect(&socket, &peer, &stats)
    }

    /// The shared socket, for its buffer and drop figures. Never read from it: the demux
    /// thread owns its receive side.
    pub fn try_clone_socket(&self) -> std::io::Result<UdpSocket> {
        self.socket.try_clone()
    }
}

/// Bind the client's shared socket and start its reader. Hand the [`ClientSocket`] to
/// `quinn::Endpoint::new_with_abstract_socket`; the [`ClientMedia`] becomes the session's
/// transport once the host speaks `punktfunk/2`.
pub fn client_socket(bind: SocketAddr) -> std::io::Result<(Arc<ClientSocket>, ClientMedia)> {
    let socket = Arc::new(UdpSocket::bind(bind)?);
    let state = Arc::new(UdpSocketState::new((&*socket).into())?);
    super::grow_socket_buffers(&socket);
    let quic = Arc::new(Mutex::new(QuicQueue {
        packets: VecDeque::new(),
        waker: None,
    }));
    let stats = Arc::new(SharedStats::default());
    let (tx, rx) = sync_channel(MEDIA_QUEUE);
    // Without GRO a read is one datagram, so the buffer is one datagram's worth. Windows
    // reports 64 segments whether or not URO is on, and nothing turns URO on.
    let coalesced = !cfg!(windows) && state.gro_segments() > 1;
    let size = if coalesced {
        READ_BUF
    } else {
        crate::packet::MAX_DATAGRAM_BYTES + 1
    };
    let pool = BufPool::new(size, (POOL_BYTES / size).max(2 * BATCH));
    let peer = Peer::default();
    let reader = Reader {
        socket: socket.clone(),
        state: state.clone(),
        quic: quic.clone(),
        media: tx,
        pool,
        stats: stats.clone(),
        peer: peer.clone(),
    };
    // As urgent as the pump it feeds: a descheduled reader is socket drops the link never had.
    std::thread::Builder::new()
        .name("punktfunk-demux".into())
        .spawn(move || {
            crate::client::pin_thread_user_interactive();
            reader.run()
        })?;
    Ok((
        Arc::new(ClientSocket {
            socket: socket.clone(),
            state,
            quic,
            stats: stats.clone(),
            peer: peer.clone(),
        }),
        ClientMedia {
            rx: Mutex::new(rx),
            stats,
            socket,
            peer,
            qos: Mutex::new(None),
        },
    ))
}

struct Reader {
    socket: Arc<UdpSocket>,
    state: Arc<UdpSocketState>,
    quic: Arc<Mutex<QuicQueue>>,
    media: SyncSender<RecvBatch>,
    pool: Arc<BufPool>,
    stats: Arc<SharedStats>,
    peer: Peer,
}

impl Reader {
    /// Reads until stopped. A second of silence from a known host reconnects the socket,
    /// since on Apple an interface that stays up but carries nothing never fails a send.
    /// Each silent second and each refused call is logged.
    fn run(self) {
        let mut bufs: Vec<Vec<u8>> = (0..BATCH)
            .map(|_| {
                self.pool
                    .take()
                    .expect("the pool holds at least two batches")
            })
            .collect();
        let mut meta = [RecvMeta::default(); BATCH];
        let mut silent = 0u32;
        let mut silent_s = 0u32;
        while !self.stats.stop.load(Ordering::Relaxed) {
            match super::udp::wait_readable(&self.socket, WAIT) {
                Ok(true) => {
                    if silent_s > 0 {
                        tracing::info!(silent_s, "shared socket readable again");
                    }
                    silent = 0;
                    silent_s = 0;
                }
                Ok(false) => {
                    silent += 1;
                    if silent == SILENT_WAITS {
                        reconnect(&self.socket, &self.peer, &self.stats);
                        silent = 0;
                        if self.stats.host_known() {
                            silent_s += 1;
                            self.stats.log_silence(silent_s);
                        }
                    }
                    continue;
                }
                Err(e) => {
                    self.stats.wait_failures.note("wait", &e);
                    std::thread::sleep(WAIT);
                    continue;
                }
            }
            loop {
                let n = {
                    let mut next = bufs.iter_mut();
                    let mut slices: [IoSliceMut<'_>; BATCH] = std::array::from_fn(|_| {
                        IoSliceMut::new(next.next().expect("BATCH buffers"))
                    });
                    match self
                        .state
                        .recv((&*self.socket).into(), &mut slices, &mut meta)
                    {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            self.stats.read_failures.note("read", &e);
                            break;
                        }
                    }
                };
                self.route(&mut bufs[..n], &meta[..n]);
                // A short batch emptied the socket: the wait above returns at once if more
                // arrived meanwhile, so the read that would only say `WouldBlock` is skipped.
                if n < quinn::udp::BATCH_SIZE.min(BATCH) {
                    break;
                }
            }
        }
    }

    /// Each datagram (each GRO segment) by its first byte. A buffer holding only the host's
    /// media goes to the session whole, as read; QUIC and a mixed buffer's media are copied.
    fn route(&self, bufs: &mut [Vec<u8>], meta: &[RecvMeta]) {
        let mut to_quinn = Vec::new();
        for (buf, m) in bufs.iter_mut().zip(meta) {
            let stride = m.stride.max(1);
            let segs = &buf[..m.len];
            let total = segs.chunks(stride).count();
            let media = segs.chunks(stride).filter(|s| is_media(s[0])).count();
            for seg in segs.chunks(stride).filter(|s| !is_media(s[0])) {
                to_quinn.push((
                    seg.to_vec(),
                    RecvMeta {
                        len: seg.len(),
                        stride: seg.len(),
                        ..*m
                    },
                ));
            }
            if media == 0 {
                continue;
            }
            if !self.stats.is_host(m.addr) {
                self.stats
                    .foreign
                    .fetch_add(media as u64, Ordering::Relaxed);
                continue;
            }
            if media < total {
                for seg in segs.chunks(stride).filter(|s| is_media(s[0])) {
                    self.hand_over(RecvBatch::single(seg.to_vec()), 1);
                }
                continue;
            }
            match self.pool.take() {
                Some(fresh) => {
                    let full = std::mem::replace(buf, fresh);
                    self.hand_over(self.pool.lend(full, m.len, stride), media);
                }
                // Every buffer is read or queued: the session is behind, as with a full queue.
                None => {
                    self.stats
                        .media_dropped
                        .fetch_add(media as u64, Ordering::Relaxed);
                }
            }
        }
        if to_quinn.is_empty() {
            return;
        }
        let mut q = self.quic.lock().unwrap_or_else(|e| e.into_inner());
        for p in to_quinn {
            if q.packets.len() == QUIC_QUEUE {
                self.stats.quic_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            q.packets.push_back(p);
            self.stats.quic.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(w) = q.waker.take() {
            w.wake();
        }
    }

    /// Queue `batch` (`datagrams` of media) for the session, or count them dropped.
    fn hand_over(&self, batch: RecvBatch, datagrams: usize) {
        let counter = match self.media.try_send(batch) {
            Ok(()) => &self.stats.media,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                &self.stats.media_dropped
            }
        };
        counter.fetch_add(datagrams as u64, Ordering::Relaxed);
    }
}

/// Retries a send after a millisecond: the client's QUIC traffic is light and fills its
/// socket buffer only behind a stalled link.
#[derive(Debug)]
struct RetrySoon(Option<Pin<Box<tokio::time::Sleep>>>);

impl quinn::UdpPoller for RetrySoon {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        let sleep = self
            .0
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(1))));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.0 = None;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

use std::future::Future;

/// `transmit` on a socket connected to `peer`: unaddressed, one datagram per segment, no ECN
/// mark (quinn's validation then stops asking for one). Any error but `WouldBlock` is a lost
/// packet, logged, and connects again, which picks the source address afresh after a roam.
fn send_connected(
    socket: &UdpSocket,
    peer: SocketAddr,
    t: &Transmit,
    stats: &SharedStats,
) -> std::io::Result<()> {
    let segment = t.segment_size.unwrap_or(t.contents.len()).max(1);
    for datagram in t.contents.chunks(segment) {
        match socket.send(datagram) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Err(e),
            Err(e) => {
                stats.send_failures.note("send", &e);
                if let Err(e) = socket.connect(peer) {
                    stats.connect_failures.note("connect", &e);
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Sends `t` with `class`'s DSCP in its own TOS byte toward a peer the mark applies to
/// ([`super::set_media_qos_to`]). quinn-udp gives every packet a TOS of its ECN bits, which
/// Linux reads over the socket's mark. Unmarked or refused, `t` goes through `quinn_send`,
/// which owns `WouldBlock`, GSO and error handling.
fn send_marked(
    socket: &UdpSocket,
    t: &Transmit,
    class: super::MediaClass,
    quinn_send: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if super::qos::dscp_enabled_for(Some(t.destination)) {
        let tos = (class.dscp() << 2) as u8 | t.ecn.map_or(0, |e| e as u8);
        if super::udp::send_with_tos(socket, t, tos).is_ok() {
            return Ok(());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = (socket, t, class);
    quinn_send()
}

/// The host endpoint's socket: quinn's own, its packets marked as video ([`send_marked`]).
#[derive(Debug)]
struct HostSocket {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    /// The same socket, for the marked send.
    socket: UdpSocket,
}

/// quinn's socket for the host endpoint, wrapped by `runtime`, with marked sends.
pub(crate) fn host_socket(
    socket: UdpSocket,
    runtime: &dyn quinn::Runtime,
) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    let marked = socket.try_clone()?;
    Ok(Arc::new(HostSocket {
        inner: runtime.wrap_udp_socket(socket)?,
        socket: marked,
    }))
}

impl quinn::AsyncUdpSocket for HostSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    /// A refused marked send goes through quinn's own, which clears the readiness a
    /// `WouldBlock` leaves set.
    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        send_marked(&self.socket, transmit, super::MediaClass::Video, || {
            self.inner.try_send(transmit)
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

impl quinn::AsyncUdpSocket for ClientSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(RetrySoon(None))
    }

    /// Held across the send: [`ClientMedia::set_host`] connects under the same lock. An
    /// unconnected send carries the audio mark (`send_marked`).
    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        let peer = self.peer.lock().unwrap_or_else(|e| e.into_inner());
        match *peer {
            Some(p) if canonical(transmit.destination) == p => {
                send_connected(&self.socket, p, transmit, &self.stats)
            }
            _ => send_marked(&self.socket, transmit, super::MediaClass::Audio, || {
                self.state.send((&*self.socket).into(), transmit)
            }),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        let mut q = self.quic.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        while n < bufs.len().min(meta.len()) {
            let Some((pkt, m)) = q.packets.pop_front() else {
                break;
            };
            let len = pkt.len().min(bufs[n].len());
            bufs[n][..len].copy_from_slice(&pkt[..len]);
            meta[n] = RecvMeta {
                len,
                stride: len,
                ..m
            };
            n += 1;
        }
        if n == 0 {
            q.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(Ok(n))
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}

impl Transport for ClientMedia {
    fn send(&self, _packet: &[u8]) -> std::io::Result<bool> {
        Err(std::io::Error::other("punktfunk/2 clients send no media"))
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        Err(std::io::Error::other(
            "punktfunk/2 media is read in batches (recv_owned)",
        ))
    }

    fn recv_owned(&self) -> std::io::Result<Option<RecvBatch>> {
        Ok(self
            .rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv()
            .ok())
    }

    fn wait_owned(&self, timeout: std::time::Duration) -> std::io::Result<Option<RecvBatch>> {
        use std::sync::mpsc::RecvTimeoutError as E;
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        match rx.recv_timeout(timeout) {
            Ok(batch) => Ok(Some(batch)),
            Err(E::Timeout) => Ok(None),
            // The demux is gone: a bare return here would spin the pump until it notices.
            Err(E::Disconnected) => {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }
    }
}

/// `PUNKTFUNK_GSO` set to anything but `0`: the offload is pinned on for an A/B, and a loss
/// guard must stand down or it downshifts the run it is measuring.
pub fn offload_pinned() -> bool {
    std::env::var_os("PUNKTFUNK_GSO").is_some_and(|v| v != "0")
}

/// `PUNKTFUNK_GSO=0`: the operator's off switch; GSO is on wherever the path has it.
fn gso_allowed() -> bool {
    static ALLOWED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALLOWED.get_or_init(|| {
        let off = crate::env_on("PUNKTFUNK_GSO") == Some(false);
        if off {
            tracing::info!("PUNKTFUNK_GSO=0: media leaves one packet per send");
        }
        !off
    })
}

/// The host's media sender on a connection's shared socket.
pub struct MediaSender {
    /// Before `socket`: a Windows flow leaves before it closes.
    route: Mutex<Route>,
    socket: UdpSocket,
    state: UdpSocketState,
    conn: quinn::Connection,
    v6_socket: bool,
    staging: Mutex<Vec<u8>>,
    /// Send offload for this session; the host's loss guard clears it for good
    /// ([`Self::set_offload_switch`]).
    offload: Arc<AtomicBool>,
}

/// Where media goes, and the move it is waiting out.
struct Route {
    to: SocketAddr,
    /// The address quinn moved to, when media first saw it, and how long media waits there.
    /// The wait is fixed at that first sight: `conn.rtt()` moves with the new path's samples.
    moving: Option<(SocketAddr, std::time::Instant, Duration)>,
    /// `conn.rtt()` of the path to `to`, which quinn leaves on a move. Read every
    /// [`RTT_EVERY`]th send: the read takes the connection's lock.
    rtt: Duration,
    sends: u8,
    /// The video mark toward `to`. A Windows flow is bound to one destination.
    qos: Option<super::QosFlow>,
}

/// Whether media to a client that moved `from` → `to`, `waited` ago, is still inside `hold`.
/// Only a new IP waits: a port-only NAT rebind has no third party to hit.
fn still_validating(from: SocketAddr, to: SocketAddr, waited: Duration, hold: Duration) -> bool {
    from.ip() != to.ip() && waited < hold
}

/// The RTT the host's quinn gives a path it has no sample for, set on its server config. It
/// sizes [`validation_hold`]: 100 ms holds media about a second after a real IP change, and
/// costs one early handshake retransmit on a link slower than 300 ms.
pub(crate) const HOST_INITIAL_RTT: Duration = Duration::from_millis(100);

/// Sends between two reads of the stable path's RTT. A smoothed RTT moves an eighth per sample.
const RTT_EVERY: u8 = 16;

/// The `max_ack_delay` every quinn endpoint advertises. A modified client may advertise up to
/// 16 s, which stretches quinn's timer past the hold; quinn does not expose the peer's value.
const PEER_MAX_ACK_DELAY: Duration = Duration::from_millis(25);

/// An upper bound of quinn's path validation timer for a move to a new IP. quinn arms
/// `3 × max(PTO of the new path, PTO of the old one)`, `PTO = rtt + max(4 × rttvar, 1 ms) +
/// max_ack_delay`. The new path starts at [`HOST_INITIAL_RTT`] with `rttvar` half of it. quinn's
/// `rttvar` is not exposed, but it stays within `2 × rtt` (each sample moves both toward it),
/// so `old_rtt`, the old path's RTT, bounds the old PTO at `9 × rtt`. The margin covers timer
/// granularity. Counted from the first sight of the move, which is no earlier than quinn's own
/// start, the hold outlasts the timer for the RTT media last read. A client that inflates its
/// RTT between two reads stretches quinn's timer past it.
fn validation_hold(old_rtt: Duration) -> Duration {
    let pto = |rtt: Duration, var: Duration| {
        rtt + (4 * var).max(Duration::from_millis(1)) + PEER_MAX_ACK_DELAY
    };
    let new_path = pto(HOST_INITIAL_RTT, HOST_INITIAL_RTT / 2);
    let old_path = pto(old_rtt, 2 * old_rtt);
    3 * new_path.max(old_path) + Duration::from_millis(100)
}

impl MediaSender {
    /// `socket` is the endpoint's own (a clone of what quinn reads); `conn` names the peer,
    /// which picks the video mark ([`super::set_media_qos_to`]).
    pub fn new(socket: &UdpSocket, conn: quinn::Connection) -> std::io::Result<MediaSender> {
        let socket = socket.try_clone()?;
        let state = UdpSocketState::new((&socket).into())?;
        let v6_socket = socket.local_addr()?.is_ipv6();
        let to = mapped(canonical(conn.remote_address()), v6_socket);
        let qos = super::set_media_qos_to(&socket, to, super::MediaClass::Video);
        Ok(MediaSender {
            route: Mutex::new(Route {
                to,
                moving: None,
                rtt: conn.rtt(),
                sends: 0,
                qos,
            }),
            socket,
            state,
            conn,
            v6_socket,
            staging: Mutex::new(Vec::new()),
            offload: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Share the offload switch: `false` sends every packet on its own, as `PUNKTFUNK_GSO=0`
    /// does, for the rest of the session.
    pub fn set_offload_switch(&mut self, switch: Arc<AtomicBool>) {
        self.offload = switch;
    }

    /// Where the next packet goes and from which local address, or `None` while it is held.
    /// quinn moves `remote_address()` on the first authenticated packet from a new address and
    /// validates it afterwards; its own sends are capped against amplification and media is
    /// not. Media follows a new IP once [`validation_hold`] has passed since it was first seen,
    /// by when quinn has validated it or moved back. The mark moves with the route.
    fn path(&self) -> Option<(SocketAddr, Option<IpAddr>)> {
        let seen = mapped(canonical(self.conn.remote_address()), self.v6_socket);
        let mut r = self.route.lock().unwrap_or_else(|e| e.into_inner());
        if seen == r.to {
            r.moving = None;
            r.sends = r.sends.wrapping_add(1);
            if r.sends % RTT_EVERY == 0 {
                r.rtt = self.conn.rtt();
            }
        } else {
            let now = std::time::Instant::now();
            let (since, hold) = match r.moving {
                Some((addr, since, hold)) if addr == seen => (since, hold),
                _ => {
                    let hold = validation_hold(r.rtt);
                    r.moving = Some((seen, now, hold));
                    (now, hold)
                }
            };
            if still_validating(r.to, seen, now.duration_since(since), hold) {
                return None;
            }
            r.moving = None;
            r.qos = None;
            r.qos = super::set_media_qos_to(&self.socket, seen, super::MediaClass::Video);
            r.to = seen;
        }
        Some((r.to, self.conn.local_ip()))
    }

    /// One send of the packets in `run`, cut at `segment`, marked as video as [`send_marked`]
    /// marks. Where the mark applies the kernel gathers the run from the packets themselves.
    /// quinn-udp's send takes one buffer, so a run of more than one is copied into `staging`.
    fn transmit(
        &self,
        to: SocketAddr,
        from: Option<IpAddr>,
        run: &[&[u8]],
        segment: Option<usize>,
    ) -> std::io::Result<()> {
        // Whether the gathered send went out, where the mark applies.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let marked =
            (run.len() <= GATHER_MAX && super::qos::dscp_enabled_for(Some(to))).then(|| {
                let mut iov = [std::io::IoSlice::new(&[]); GATHER_MAX];
                for (slot, p) in iov.iter_mut().zip(run) {
                    *slot = std::io::IoSlice::new(p);
                }
                let tos = (super::MediaClass::Video.dscp() << 2) as u8;
                let iov = &iov[..run.len()];
                super::udp::send_gathered_with_tos(&self.socket, to, from, iov, segment, tos)
                    .is_ok()
            });
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let marked = None;
        if marked == Some(true) {
            return Ok(());
        }
        let mut staging = None;
        let contents = match run {
            [p] => *p,
            _ => {
                let s = staging.insert(self.staging.lock().unwrap_or_else(|e| e.into_inner()));
                s.clear();
                for p in run {
                    s.extend_from_slice(p);
                }
                &s[..]
            }
        };
        let t = Transmit {
            destination: to,
            ecn: None,
            contents,
            segment_size: segment,
            src_ip: from,
        };
        let quinn = || self.state.send((&self.socket).into(), &t);
        match marked {
            // Refused marked, the run leaves through quinn-udp as `send_marked` falls back.
            Some(_) => quinn(),
            None => send_marked(&self.socket, &t, super::MediaClass::Video, quinn),
        }
    }
}

/// Packets one gathered send takes at most: quinn-udp's GSO segment limit, far under the
/// kernel's 1024 buffers a call (`UIO_MAXIOV`).
#[cfg(any(target_os = "linux", target_os = "android"))]
const GATHER_MAX: usize = 64;

/// A held packet counts as sent: it is lost on the way, as on a link that dropped it.
impl Transport for MediaSender {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        let Some((to, from)) = self.path() else {
            return Ok(true);
        };
        match self.transmit(to, from, &[packet], None) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        let Some((to, from)) = self.path() else {
            return Ok(packets.len());
        };
        for (i, p) in packets.iter().enumerate() {
            match self.transmit(to, from, std::slice::from_ref(p), None) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(i),
                Err(e) => return Err(e),
            }
        }
        Ok(packets.len())
    }

    /// Runs of equal-size packets leave as one offloaded send ([`Self::transmit`]), each run
    /// within the platform's segment limit and under the UDP payload bound of either IP family;
    /// the last packet of a run may be shorter. An oversize run is `EMSGSIZE`, which quinn-udp
    /// reports as sent. With `PUNKTFUNK_GSO=0`, or once the session's loss guard cleared the
    /// switch, packets leave one by one.
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        const GSO_MAX_PAYLOAD: usize = 65535 - 40 - 8;
        let max = self.state.max_gso_segments();
        if !gso_allowed() || !self.offload.load(Ordering::Relaxed) || max <= 1 {
            return self.send_batch(packets);
        }
        let Some((to, from)) = self.path() else {
            return Ok(packets.len());
        };
        let mut sent = 0;
        while sent < packets.len() {
            let size = packets[sent].len();
            let cap = (GSO_MAX_PAYLOAD / size.max(1)).clamp(1, max);
            let mut end = sent + 1;
            while end < packets.len()
                && end - sent < cap
                && packets[end].len() <= size
                && packets[end - 1].len() == size
            {
                end += 1;
            }
            let segment = (end - sent > 1).then_some(size);
            match self.transmit(to, from, &packets[sent..end], segment) {
                Ok(()) => sent = end,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(sent),
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_call_logs_once_then_waits_a_second() {
        let f = Failures::default();
        let e = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        for _ in 0..3 {
            f.note("send", &e);
        }
        assert_eq!(f.total(), 3);
        let logged = f.logged.lock().unwrap().map(|(_, n)| n);
        assert_eq!(logged, Some(1), "only the first refusal went out");
    }

    #[test]
    fn first_byte_splits_media_from_quic() {
        for b in 0x00..=0x3Fu8 {
            assert!(is_media(b));
        }
        for b in 0x40..=0xFFu8 {
            assert!(!is_media(b), "{b:#x} is a QUIC first byte");
        }
        assert_eq!(
            canonical("[::ffff:10.0.0.2]:9777".parse().unwrap()),
            "10.0.0.2:9777".parse().unwrap()
        );
    }

    type Pair = (
        quinn::Endpoint,
        std::net::UdpSocket,
        quinn::Endpoint,
        ClientMedia,
        quinn::Connection,
        quinn::Connection,
    );

    async fn pair() -> Pair {
        pair_with(|host| host).await
    }

    /// A host and a client that dials `dial(host)`: the host itself, or a proxy in front of it.
    async fn pair_with(dial: impl FnOnce(SocketAddr) -> SocketAddr) -> Pair {
        use crate::quic::endpoint;
        let (cert, key) = endpoint::generate_identity().unwrap();
        let (server, socket) = endpoint::server_shared(
            "127.0.0.1:0".parse().unwrap(),
            &cert,
            &key,
            Duration::from_secs(8),
            &[crate::quic::v2::registry::ALPN, b"pkf1"],
        )
        .unwrap();
        let addr = dial(server.local_addr().unwrap());
        let (client, _) = endpoint::client_shared(
            None,
            None,
            &[crate::quic::v2::registry::ALPN, b"pkf1"],
            None,
        );
        let (client, media) = client.unwrap();
        let accept = tokio::spawn({
            let server = server.clone();
            async move { server.accept().await.unwrap().await.unwrap() }
        });
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let host_conn = accept.await.unwrap();
        media.set_host(addr);
        (server, socket, client, media, host_conn, client_conn)
    }

    /// Media and QUIC cross one socket at once, and neither lands in the other's queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn media_and_quic_share_one_socket() {
        let (_server, socket, _client, media, host_conn, client_conn) = pair().await;
        assert_eq!(
            crate::quic::endpoint::negotiated_alpn(&host_conn).as_deref(),
            Some(crate::quic::v2::registry::ALPN)
        );
        let sender = MediaSender::new(&socket, host_conn.clone()).unwrap();
        const N: usize = 3000;
        let packets: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut p = vec![(i % 16) as u8; 1100];
                p[1..9].copy_from_slice(&(i as u64).to_le_bytes());
                p
            })
            .collect();
        let quic = tokio::spawn({
            let host_conn = host_conn.clone();
            async move {
                for i in 0..300u32 {
                    let _ = host_conn.send_datagram(i.to_le_bytes().to_vec().into());
                    tokio::time::sleep(Duration::from_micros(200)).await;
                }
            }
        });
        let send = std::thread::spawn(move || {
            // 64 × 1100 B passes the 64 KiB a single offloaded send may carry.
            for chunk in packets.chunks(64) {
                let refs: Vec<&[u8]> = chunk.iter().map(|p| p.as_slice()).collect();
                let mut off = 0;
                while off < refs.len() {
                    off += sender.send_gso(&refs[off..]).unwrap();
                    if off < refs.len() {
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        });
        let mut seen = vec![false; N];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = 0;
        while got < N && std::time::Instant::now() < deadline {
            let Some(batch) = media.recv_owned().unwrap() else {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            let mut off = 0;
            while let Some(r) = batch.at(off) {
                off = r.end;
                let p = batch.bytes(r);
                assert!(is_media(p[0]), "a QUIC packet reached the media queue");
                assert_eq!(p.len(), 1100);
                let idx = u64::from_le_bytes(p[1..9].try_into().unwrap()) as usize;
                assert_eq!(p[0] as usize, idx % 16);
                if !std::mem::replace(&mut seen[idx], true) {
                    got += 1;
                }
            }
        }
        send.join().unwrap();
        quic.await.unwrap();
        let mut datagrams = 0;
        while let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(200), client_conn.read_datagram()).await
        {
            datagrams += 1;
        }
        client_conn.send_datagram(vec![7u8; 8].into()).unwrap();
        let up = tokio::time::timeout(Duration::from_secs(2), host_conn.read_datagram()).await;
        let st = media.stats();
        assert_eq!(got, N, "every media packet arrives on loopback");
        assert!(datagrams > 250, "QUIC datagrams kept flowing: {datagrams}");
        assert!(
            matches!(up, Ok(Ok(_))),
            "QUIC leaves the client after set_host"
        );
        assert_eq!(st.foreign.load(Ordering::Relaxed), 0);
        assert_eq!(st.media_dropped.load(Ordering::Relaxed), 0);
        assert!(st.quic.load(Ordering::Relaxed) > 0);
    }

    /// Media-shaped bytes from any address but the host's never reach the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn media_from_a_stranger_is_dropped() {
        let (_server, _socket, client, media, _host_conn, _client_conn) = pair().await;
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = SocketAddr::new(
            "127.0.0.1".parse().unwrap(),
            client.local_addr().unwrap().port(),
        );
        for _ in 0..10 {
            stranger.send_to(&[0x01; 200], to).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(media.recv_owned().unwrap().is_none());
        // A connected socket's kernel drops them before the demux counts them.
        let counted = if cfg!(target_vendor = "apple") { 0 } else { 10 };
        assert_eq!(media.stats().foreign.load(Ordering::Relaxed), counted);
    }

    /// A NAT between client and host whose outside address changes on [`Self::rebind`]. The
    /// client always talks to `front`; the host sees `backs[0]`, then `backs[1]`. Once rebound,
    /// what the host sends to the old address is dropped, as by a NAT that forgot it.
    struct RebindingProxy {
        front: SocketAddr,
        backs: [SocketAddr; 2],
        rebound: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    }

    impl RebindingProxy {
        fn start(host: SocketAddr) -> RebindingProxy {
            let bind = |a: &str| {
                let s = UdpSocket::bind(a).unwrap();
                s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
                super::super::grow_socket_buffers(&s);
                Arc::new(s)
            };
            let front = bind("127.0.0.1:0");
            // A new IP where the OS has one (Linux answers on all of 127/8): quinn then restarts
            // MTU discovery. Elsewhere a new port, which quinn treats as a NAT rebind.
            let second = UdpSocket::bind("127.0.0.2:0")
                .map(|_| "127.0.0.2:0")
                .unwrap_or("127.0.0.1:0");
            let backs = [bind("127.0.0.1:0"), bind(second)];
            let rebound = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
            let mut buf = vec![0u8; 65536];
            let (f, b, r, s, c) = (
                front.clone(),
                backs.clone(),
                rebound.clone(),
                stop.clone(),
                client.clone(),
            );
            std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = f.recv_from(&mut buf) {
                        *c.lock().unwrap() = Some(from);
                        let out = &b[r.load(Ordering::Relaxed) as usize];
                        let _ = out.send_to(&buf[..n], host);
                    }
                }
            });
            for (i, back) in backs.iter().enumerate() {
                let (f, back, r, s, c) = (
                    front.clone(),
                    back.clone(),
                    rebound.clone(),
                    stop.clone(),
                    client.clone(),
                );
                let mut buf = vec![0u8; 65536];
                std::thread::spawn(move || {
                    while !s.load(Ordering::Relaxed) {
                        let Ok((n, _)) = back.recv_from(&mut buf) else {
                            continue;
                        };
                        let live = r.load(Ordering::Relaxed) as usize == i;
                        if let (true, Some(to)) = (live, *c.lock().unwrap()) {
                            let _ = f.send_to(&buf[..n], to);
                        }
                    }
                });
            }
            RebindingProxy {
                front: front.local_addr().unwrap(),
                backs: backs.map(|s| s.local_addr().unwrap()),
                rebound,
                stop,
            }
        }

        fn rebind(&self) {
            self.rebound.store(true, Ordering::Relaxed);
        }
    }

    impl Drop for RebindingProxy {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Media sent to the client, `count` packets tagged from `first`, until each one left.
    /// Paced at 20 packets a millisecond: the proxy's socket buffer is a default one.
    fn send_media(sender: &MediaSender, first: usize, count: usize) {
        for i in first..first + count {
            let mut p = vec![0x01u8; 1000];
            p[1..9].copy_from_slice(&(i as u64).to_le_bytes());
            while !sender.send(&p).unwrap() {
                std::thread::sleep(Duration::from_micros(100));
            }
            if i % 20 == 19 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Tags of the media that reaches the client within `wait`, stopping at `want`.
    async fn receive_media(media: &ClientMedia, want: usize, wait: Duration) -> Vec<usize> {
        let deadline = std::time::Instant::now() + wait;
        let mut tags = Vec::new();
        while tags.len() < want && std::time::Instant::now() < deadline {
            let Some(batch) = media.recv_owned().unwrap() else {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            };
            let mut off = 0;
            while let Some(r) = batch.at(off) {
                off = r.end;
                let p = batch.bytes(r);
                tags.push(u64::from_le_bytes(p[1..9].try_into().unwrap()) as usize);
            }
        }
        tags
    }

    /// A client whose address moves mid-session keeps its stream: the host learns the new
    /// address from the client's next packet, media follows it with no reconnect, and the
    /// client counts nothing as foreign.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn media_follows_a_client_that_moves() {
        let mut proxy = None;
        let (_server, socket, _client, media, host_conn, client_conn) = pair_with(|host| {
            let p = RebindingProxy::start(host);
            let front = p.front;
            proxy = Some(p);
            front
        })
        .await;
        let proxy = proxy.unwrap();
        let sender = MediaSender::new(&socket, host_conn.clone()).unwrap();
        assert_eq!(canonical(sender.path().unwrap().0), proxy.backs[0]);
        send_media(&sender, 0, 100);
        assert_eq!(
            receive_media(&media, 100, Duration::from_secs(5))
                .await
                .len(),
            100
        );

        proxy.rebind();
        client_conn.send_datagram(vec![7u8; 8].into()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while canonical(host_conn.remote_address()) != proxy.backs[1] {
            assert!(
                std::time::Instant::now() < deadline,
                "the host never saw the move"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // A new IP holds media for the validation window; a port-only rebind does not.
        let held = sender.path().is_none();
        assert_eq!(held, proxy.backs[0].ip() != proxy.backs[1].ip());
        let follow_by = std::time::Instant::now() + validation_hold(Duration::ZERO) * 2;
        while sender.path().is_none() {
            assert!(
                std::time::Instant::now() < follow_by,
                "media never followed"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(canonical(sender.path().unwrap().0), proxy.backs[1]);

        send_media(&sender, 100, 300);
        let tags = receive_media(&media, 300, Duration::from_secs(5)).await;
        assert_eq!(tags.len(), 300, "every packet after the move arrives");
        assert!(tags.iter().all(|&t| (100..400).contains(&t)));
        assert_eq!(media.stats().foreign.load(Ordering::Relaxed), 0);
        host_conn.send_datagram(vec![9u8; 8].into()).unwrap();
        let down = tokio::time::timeout(Duration::from_secs(2), client_conn.read_datagram()).await;
        assert!(
            matches!(down, Ok(Ok(_))),
            "QUIC still crosses after the move"
        );
        assert!(host_conn.close_reason().is_none() && client_conn.close_reason().is_none());
    }

    /// Only a new IP waits for validation, and only for the hold.
    #[test]
    fn only_a_new_ip_waits_for_validation() {
        let a: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let rebound: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let moved: SocketAddr = "10.0.1.7:5000".parse().unwrap();
        let hold = validation_hold(Duration::from_millis(10));
        // 3 × (100 + 4 × 50 + 25) ms from the fresh path, plus the margin.
        assert_eq!(hold, Duration::from_millis(1075));
        assert!(!still_validating(a, rebound, Duration::ZERO, hold));
        assert!(still_validating(a, moved, Duration::from_millis(100), hold));
        assert!(!still_validating(a, moved, hold, hold));
    }

    /// quinn-proto 0.11.19 `RttEstimator::update` and `pto_base`, which the hold must outlast.
    fn quinn_rtt(samples: &[u64]) -> (Duration, Duration) {
        let (mut smoothed, mut var) = (None::<Duration>, HOST_INITIAL_RTT / 2);
        for &ms in samples {
            let x = Duration::from_millis(ms);
            (smoothed, var) = match smoothed {
                None => (Some(x), x / 2),
                Some(s) => (Some((7 * s + x) / 8), (3 * var + s.abs_diff(x)) / 4),
            };
        }
        (smoothed.unwrap_or(HOST_INITIAL_RTT), var)
    }

    fn quinn_pto(rtt: Duration, var: Duration) -> Duration {
        rtt + (4 * var).max(Duration::from_millis(1)) + Duration::from_millis(25)
    }

    /// The hold is never shorter than the timer quinn arms on a move to a new IP:
    /// `3 × max(PTO of a fresh path, PTO of the old path)`.
    #[test]
    fn the_hold_outlasts_quinns_validation_timer() {
        // The old path is fresh: no sample yet, the RTT is the initial one.
        let fresh = 3 * quinn_pto(HOST_INITIAL_RTT, HOST_INITIAL_RTT / 2);
        assert_eq!(fresh, Duration::from_millis(975));
        assert!(validation_hold(HOST_INITIAL_RTT) >= fresh);
        // Old paths at their worst: steady, slow, a spike after calm, a flapping link.
        let mut runs: Vec<Vec<u64>> = vec![
            vec![800; 20],
            [vec![50; 20], vec![5000]].concat(),
            [vec![10; 20], vec![9000, 10, 9000]].concat(),
            (0..40).map(|i| if i % 2 == 0 { 5000 } else { 5 }).collect(),
            (1..30).map(|i| i * i).collect(),
        ];
        runs.push((0..40).map(|i| 1 + (i * 7919) % 6000).collect());
        for run in &runs {
            for n in 1..=run.len() {
                let (rtt, var) = quinn_rtt(&run[..n]);
                assert!(var <= 2 * rtt, "rttvar stays within twice the RTT");
                let quinn =
                    3 * quinn_pto(HOST_INITIAL_RTT, HOST_INITIAL_RTT / 2).max(quinn_pto(rtt, var));
                assert!(validation_hold(rtt) >= quinn, "{:?}", &run[..n]);
            }
        }
    }

    /// A lent buffer comes back on drop, and the pool never allocates past its bound.
    #[test]
    fn lent_buffers_return_and_stay_bounded() {
        let pool = BufPool::new(16, 2);
        let a = pool.take().unwrap();
        let b = pool.take().unwrap();
        assert!(pool.take().is_none(), "two buffers is the bound");
        let batch = pool.lend(a, 10, 4);
        assert_eq!(
            (batch.at(0), batch.at(8), batch.at(10)),
            (Some(0..4), Some(8..10), None)
        );
        assert_eq!(batch.remaining(4), 2);
        drop(batch);
        assert_eq!(
            pool.take().map(|v| v.len()),
            Some(16),
            "the dropped batch's buffer"
        );
        drop(b);
        assert!(
            pool.take().is_none(),
            "a buffer dropped outside a batch is gone for good"
        );
    }

    /// The DSCP mark as it arrives, read where Linux reports it.
    #[cfg(target_os = "linux")]
    mod marks {
        use super::*;
        use crate::transport::{qos::dscp_enabled_for, udp::send_with_tos, MediaClass};
        use quinn::udp::EcnCodepoint;

        /// One datagram from `socket`, which has `IP_RECVTOS` or `IPV6_RECVTCLASS` on: its
        /// length and the TOS byte or traffic class the kernel reports beside it.
        #[allow(unsafe_code)]
        fn recv_tos(socket: &UdpSocket, buf: &mut [u8]) -> std::io::Result<(usize, u8)> {
            use std::os::fd::AsRawFd;
            let mut ctrl = [0u8; 64];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut msg = libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: ctrl.as_mut_ptr().cast(),
                msg_controllen: ctrl.len() as _,
                msg_flags: 0,
            };
            // SAFETY: `msg` names `iov`, `buf` and `ctrl` with their real lengths, all three
            // outlive the call, and `socket` keeps the fd open across it.
            let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // One control message: a word of length, level and type, then the value.
            let word = std::mem::size_of::<usize>();
            let head = std::mem::size_of::<libc::cmsghdr>();
            let len = usize::from_ne_bytes(ctrl[..word].try_into().unwrap());
            let value = match &ctrl[head..len] {
                [tos] => *tos,
                class => i32::from_ne_bytes(class.try_into().unwrap()) as u8,
            };
            Ok((n as usize, value))
        }

        /// Each datagram one way through a [`TosProxy`]: first byte, length, TOS.
        type Seen = Arc<Mutex<Vec<(u8, usize, u8)>>>;

        /// A relay between client and host that notes every datagram both ways.
        struct TosProxy {
            front: SocketAddr,
            up: Seen,
            down: Seen,
            stop: Arc<AtomicBool>,
        }

        impl TosProxy {
            fn start(host: SocketAddr) -> TosProxy {
                let bind = || {
                    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
                    s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
                    socket2::SockRef::from(&s).set_recv_tos_v4(true).unwrap();
                    Arc::new(s)
                };
                let (front, back) = (bind(), bind());
                let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
                let (up, down) = (Seen::default(), Seen::default());
                let stop = Arc::new(AtomicBool::new(false));
                let (f, b, seen, s, c) = (
                    front.clone(),
                    back.clone(),
                    up.clone(),
                    stop.clone(),
                    client.clone(),
                );
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 65536];
                    while !s.load(Ordering::Relaxed) {
                        let Ok((_, from)) = f.peek_from(&mut buf) else {
                            continue;
                        };
                        let Ok((n, tos)) = recv_tos(&f, &mut buf) else {
                            continue;
                        };
                        *c.lock().unwrap() = Some(from);
                        seen.lock().unwrap().push((buf[0], n, tos));
                        let _ = b.send_to(&buf[..n], host);
                    }
                });
                let (f, b, seen, s) = (front.clone(), back, down.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 65536];
                    while !s.load(Ordering::Relaxed) {
                        let Ok((n, tos)) = recv_tos(&b, &mut buf) else {
                            continue;
                        };
                        seen.lock().unwrap().push((buf[0], n, tos));
                        if let Some(to) = *client.lock().unwrap() {
                            let _ = f.send_to(&buf[..n], to);
                        }
                    }
                });
                TosProxy {
                    front: front.local_addr().unwrap(),
                    up,
                    down,
                    stop,
                }
            }
        }

        impl Drop for TosProxy {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
            }
        }

        /// Toward a private peer, media and the host's QUIC leave marked CS5 and the client's
        /// QUIC CS6, with their ECN bits kept. A GSO group arrives as the datagrams it was
        /// built from, in order. With DSCP off for the peer every packet leaves bare
        /// ([`the_kill_switch_unmarks_the_wire`]).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn marks_reach_the_wire() {
            let mut proxy = None;
            let (_server, socket, _client, media, host_conn, client_conn) = pair_with(|host| {
                let p = TosProxy::start(host);
                let front = p.front;
                proxy = Some(p);
                front
            })
            .await;
            let proxy = proxy.unwrap();
            let on = dscp_enabled_for(Some(proxy.front));
            let (video, uplink) = if on { (0xA0, 0xC0) } else { (0, 0) };
            let sender = MediaSender::new(&socket, host_conn.clone()).unwrap();
            let singles: Vec<Vec<u8>> = (1..=3).map(|i| vec![i; 1100]).collect();
            let group: Vec<Vec<u8>> = (4..=13)
                .map(|i| vec![i; if i == 13 { 700 } else { 1100 }])
                .collect();
            for p in &singles {
                assert!(sender.send(p).unwrap());
            }
            let refs: Vec<&[u8]> = group.iter().map(Vec::as_slice).collect();
            assert_eq!(sender.send_gso(&refs).unwrap(), group.len());
            client_conn.send_datagram(vec![7u8; 8].into()).unwrap();
            host_conn.send_datagram(vec![9u8; 8].into()).unwrap();

            let sent: Vec<Vec<u8>> = singles.into_iter().chain(group).collect();
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while got.len() < sent.len() && Instant::now() < deadline {
                let Some(batch) = media.recv_owned().unwrap() else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                };
                let mut off = 0;
                while let Some(r) = batch.at(off) {
                    off = r.end;
                    got.push(batch.bytes(r).to_vec());
                }
            }
            assert_eq!(got, sent, "the client reads what the host sent");
            let wait = Duration::from_secs(2);
            assert!(tokio::time::timeout(wait, host_conn.read_datagram())
                .await
                .is_ok());
            assert!(tokio::time::timeout(wait, client_conn.read_datagram())
                .await
                .is_ok());

            let down = proxy.down.lock().unwrap().clone();
            let up = proxy.up.lock().unwrap().clone();
            let media_down: Vec<_> = down.iter().filter(|d| is_media(d.0)).copied().collect();
            let expect: Vec<_> = sent.iter().map(|p| (p[0], p.len(), video)).collect();
            assert_eq!(media_down, expect, "one datagram each, TOS {video:#x}");
            for (who, seen, mark) in [("host", &down, video), ("client", &up, uplink)] {
                let quic: Vec<u8> = seen
                    .iter()
                    .filter(|d| !is_media(d.0))
                    .map(|d| d.2)
                    .collect();
                assert!(
                    quic.iter().all(|tos| tos & 0xFC == mark),
                    "{who} QUIC carries {mark:#x}: {quic:x?}"
                );
                assert!(
                    quic.iter().any(|tos| tos & 3 == EcnCodepoint::Ect0 as u8),
                    "{who} QUIC keeps ECT(0): {quic:x?}"
                );
            }
        }

        /// `PUNKTFUNK_DSCP=0` leaves every packet bare: [`marks_reach_the_wire`] in a process
        /// started with it.
        #[test]
        fn the_kill_switch_unmarks_the_wire() {
            if std::env::var_os("PUNKTFUNK_DSCP").is_some() {
                return;
            }
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "transport::shared::tests::marks::marks_reach_the_wire",
                ])
                .env("PUNKTFUNK_DSCP", "0")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        /// The marked send's TOS (IPv4) and traffic class (IPv6) arrive as sent, from the
        /// source it names, singly and as a GSO group cut at its segment size.
        #[test]
        fn a_marked_send_keeps_its_tos_on_both_families() {
            let mut buf = vec![0u8; 65536];
            let mut families = 0;
            for ip in ["127.0.0.1", "::1"] {
                // A box without IPv6 loopback checks IPv4 only.
                let Ok(rx) = UdpSocket::bind((ip, 0)) else {
                    continue;
                };
                let to = rx.local_addr().unwrap();
                let opt = socket2::SockRef::from(&rx);
                if to.is_ipv4() {
                    opt.set_recv_tos_v4(true).unwrap();
                } else {
                    opt.set_recv_tclass_v6(true).unwrap();
                }
                rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let tx = UdpSocket::bind((ip, 0)).unwrap();
                let payload = [0x01u8; 2500];
                for (segment, lens) in [(None, &[2500][..]), (Some(1000), &[1000, 1000, 500])] {
                    let t = Transmit {
                        destination: to,
                        ecn: Some(EcnCodepoint::Ect0),
                        contents: &payload,
                        segment_size: segment,
                        src_ip: Some(to.ip()),
                    };
                    send_with_tos(&tx, &t, 0xA2).unwrap();
                    for &len in lens {
                        let got = recv_tos(&rx, &mut buf).unwrap();
                        assert_eq!(got, (len, 0xA2), "{ip}, segment {segment:?}");
                    }
                }
                families += 1;
            }
            assert!(families > 0);
        }

        /// A gathered send leaves the datagrams the send of the same packets in one buffer
        /// does, byte for byte: a run with a short last packet, a run of equal packets, and a
        /// single packet, on both families.
        #[test]
        fn a_gathered_send_matches_the_contiguous_one() {
            use crate::transport::udp::send_gathered_with_tos;
            let mut buf = vec![0u8; 65536];
            let mut families = 0;
            for ip in ["127.0.0.1", "::1"] {
                let Ok(rx) = UdpSocket::bind((ip, 0)) else {
                    continue;
                };
                let to = rx.local_addr().unwrap();
                let opt = socket2::SockRef::from(&rx);
                if to.is_ipv4() {
                    opt.set_recv_tos_v4(true).unwrap();
                } else {
                    opt.set_recv_tclass_v6(true).unwrap();
                }
                rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let tx = UdpSocket::bind((ip, 0)).unwrap();
                let mut recv = |n: usize| -> Vec<(Vec<u8>, u8)> {
                    (0..n)
                        .map(|_| {
                            let (len, tos) = recv_tos(&rx, &mut buf).unwrap();
                            (buf[..len].to_vec(), tos)
                        })
                        .collect()
                };
                for lens in [&[1472, 1472, 1472, 300][..], &[1472; 4], &[1472]] {
                    let packets: Vec<Vec<u8>> = lens
                        .iter()
                        .enumerate()
                        .map(|(i, &n)| (0..n).map(|b| (i * 31 + b) as u8).collect())
                        .collect();
                    let segment = (packets.len() > 1).then_some(1472);
                    let whole = packets.concat();
                    let t = Transmit {
                        destination: to,
                        ecn: None,
                        contents: &whole,
                        segment_size: segment,
                        src_ip: Some(to.ip()),
                    };
                    send_with_tos(&tx, &t, 0xA0).unwrap();
                    let contiguous = recv(packets.len());
                    let iov: Vec<std::io::IoSlice> =
                        packets.iter().map(|p| std::io::IoSlice::new(p)).collect();
                    send_gathered_with_tos(&tx, to, Some(to.ip()), &iov, segment, 0xA0).unwrap();
                    let gathered = recv(packets.len());
                    let sent: Vec<_> = packets.into_iter().map(|p| (p, 0xA0)).collect();
                    assert_eq!(contiguous, sent, "{ip}, {lens:?}: one datagram a packet");
                    assert_eq!(gathered, contiguous, "{ip}, {lens:?}");
                }
                rx.set_nonblocking(true).unwrap();
                assert!(
                    rx.recv(&mut buf).is_err(),
                    "{ip}: no datagram beyond the packets"
                );
                families += 1;
            }
            assert!(families > 0);
        }

        /// ns per GSO group of 11 and 44 × 1472 B packets on loopback: the run copied into one
        /// buffer and sent by quinn-udp or marked, against the marked send the kernel gathers
        /// from the packets: `cargo test -p punktfunk-core --features quic --release
        /// gso_send_cost -- --ignored --nocapture`.
        #[test]
        #[ignore = "a timing that only prints"]
        fn gso_send_cost() {
            use crate::transport::udp::send_gathered_with_tos;
            use std::io::IoSlice;
            let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let state = UdpSocketState::new((&tx).into()).unwrap();
            let to = sink.local_addr().unwrap();
            const N: u32 = 20_000;
            let time = |send: &mut dyn FnMut()| {
                let start = Instant::now();
                for _ in 0..N {
                    send();
                }
                start.elapsed().as_nanos() / u128::from(N)
            };
            for n in [11, 44] {
                let packets = vec![vec![0x01u8; 1472]; n];
                let mut staging = Vec::new();
                let mut copied = |marked: bool| {
                    staging.clear();
                    for p in &packets {
                        staging.extend_from_slice(p);
                    }
                    let t = Transmit {
                        destination: to,
                        ecn: None,
                        contents: &staging,
                        segment_size: Some(1472),
                        src_ip: Some(to.ip()),
                    };
                    if marked {
                        send_marked(&tx, &t, MediaClass::Video, || {
                            unreachable!("a loopback peer is marked")
                        })
                        .unwrap()
                    } else {
                        state.send((&tx).into(), &t).unwrap()
                    }
                };
                let mut gathered = || {
                    let mut iov = [IoSlice::new(&[]); 64];
                    for (slot, p) in iov.iter_mut().zip(&packets) {
                        *slot = IoSlice::new(p);
                    }
                    send_gathered_with_tos(&tx, to, Some(to.ip()), &iov[..n], Some(1472), 0xA0)
                        .unwrap()
                };
                for _ in 0..3 {
                    let quinn = time(&mut || copied(false));
                    let marked = time(&mut || copied(true));
                    let gathered = time(&mut gathered);
                    println!(
                        "{n} x 1472 B, ns per GSO group: copied quinn-udp {quinn}, \
                         copied marked {marked}, gathered marked {gathered}"
                    );
                }
            }
        }
    }
}
