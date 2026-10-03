//! One UDP socket for QUIC and `punktfunk/2` media. A packet whose first byte has its two top
//! bits clear is media; anything else is QUIC (RFC 9443). The client's endpoint does not offer
//! QUIC bit greasing, so the host never clears those bits on a QUIC packet toward it.
//!
//! Client: [`client_socket`] starts a native thread that owns every read. QUIC packets go to
//! quinn through the [`ClientSocket`] it returns; media from the host's address goes to the
//! session through [`ClientMedia`], a bounded queue. Video never enters an async runtime.
//!
//! Host: [`MediaSender`] sends media on a clone of the endpoint's socket, toward the
//! connection's current address and from the address the client dialed, so a client that
//! roams keeps its video once QUIC has validated the new path.

use super::Transport;
use quinn::udp::{RecvMeta, Transmit, UdpSocketState};
use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Whether a datagram on the shared socket is media rather than QUIC.
pub fn is_media(first_byte: u8) -> bool {
    first_byte & 0xC0 == 0
}

/// Media packets the client queues for the session; past this it drops the newest, as a full
/// socket buffer would.
const MEDIA_QUEUE: usize = 8192;
/// QUIC packets queued for quinn; quinn drains them on every wake.
const QUIC_QUEUE: usize = 1024;
/// Datagrams one read takes, and the buffer each needs with GRO.
const BATCH: usize = 32;
const READ_BUF: usize = 64 * 1024;
/// How long the reader waits before it looks at its stop flag again.
const WAIT: Duration = Duration::from_millis(100);

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
}

impl SharedStats {
    /// Accept media from `host` only. Until it is set, every media packet is foreign.
    pub fn set_host(&self, host: SocketAddr) {
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) = Some(canonical(host));
    }

    fn is_host(&self, from: SocketAddr) -> bool {
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) == Some(canonical(from))
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

struct QuicQueue {
    packets: VecDeque<(Vec<u8>, RecvMeta)>,
    waker: Option<Waker>,
}

/// The quinn side of the client's shared socket.
pub struct ClientSocket {
    socket: Arc<UdpSocket>,
    state: Arc<UdpSocketState>,
    quic: Arc<Mutex<QuicQueue>>,
    stats: Arc<SharedStats>,
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
    rx: Mutex<Receiver<Vec<u8>>>,
    stats: Arc<SharedStats>,
}

impl ClientMedia {
    pub fn stats(&self) -> &Arc<SharedStats> {
        &self.stats
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
    let reader = Reader {
        socket: socket.clone(),
        state: state.clone(),
        quic: quic.clone(),
        media: tx,
        stats: stats.clone(),
    };
    std::thread::Builder::new()
        .name("punktfunk-demux".into())
        .spawn(move || reader.run())?;
    Ok((
        Arc::new(ClientSocket {
            socket,
            state,
            quic,
            stats: stats.clone(),
        }),
        ClientMedia {
            rx: Mutex::new(rx),
            stats,
        },
    ))
}

struct Reader {
    socket: Arc<UdpSocket>,
    state: Arc<UdpSocketState>,
    quic: Arc<Mutex<QuicQueue>>,
    media: SyncSender<Vec<u8>>,
    stats: Arc<SharedStats>,
}

impl Reader {
    fn run(self) {
        let mut bufs = vec![vec![0u8; READ_BUF]; BATCH];
        let mut meta = [RecvMeta::default(); BATCH];
        while !self.stats.stop.load(Ordering::Relaxed) {
            match super::udp::wait_readable(&self.socket, WAIT) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::debug!(error = %e, "shared socket wait");
                    std::thread::sleep(WAIT);
                    continue;
                }
            }
            loop {
                let mut slices: Vec<IoSliceMut<'_>> =
                    bufs.iter_mut().map(|b| IoSliceMut::new(b)).collect();
                let n = match self
                    .state
                    .recv((&*self.socket).into(), &mut slices, &mut meta)
                {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "shared socket read");
                        break;
                    }
                };
                drop(slices);
                self.route(&bufs[..n], &meta[..n]);
            }
        }
    }

    /// Each datagram (each GRO segment) by its first byte.
    fn route(&self, bufs: &[Vec<u8>], meta: &[RecvMeta]) {
        let mut to_quinn = Vec::new();
        for (buf, m) in bufs.iter().zip(meta) {
            for seg in buf[..m.len].chunks(m.stride.max(1)) {
                if !is_media(seg[0]) {
                    to_quinn.push((
                        seg.to_vec(),
                        RecvMeta {
                            len: seg.len(),
                            stride: seg.len(),
                            ..*m
                        },
                    ));
                } else if !self.stats.is_host(m.addr) {
                    self.stats.foreign.fetch_add(1, Ordering::Relaxed);
                } else {
                    match self.media.try_send(seg.to_vec()) {
                        Ok(()) => self.stats.media.fetch_add(1, Ordering::Relaxed),
                        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                            self.stats.media_dropped.fetch_add(1, Ordering::Relaxed)
                        }
                    };
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

impl quinn::AsyncUdpSocket for ClientSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(RetrySoon(None))
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        self.state.send((&*self.socket).into(), transmit)
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
        Ok(self
            .rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv()
            .ok())
    }

    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0;
        while n < out.len().min(lens.len()) {
            let Ok(pkt) = rx.try_recv() else {
                break;
            };
            // A packet longer than the slot reads as full, which the session drops as oversized.
            let len = pkt.len().min(out[n].len());
            out[n][..len].copy_from_slice(&pkt[..len]);
            lens[n] = if pkt.len() > out[n].len() {
                out[n].len()
            } else {
                len
            };
            n += 1;
        }
        Ok(n)
    }
}

/// The host's media sender on a connection's shared socket.
pub struct MediaSender {
    socket: UdpSocket,
    state: UdpSocketState,
    conn: quinn::Connection,
    v6_socket: bool,
    gso: AtomicBool,
    staging: Mutex<Vec<u8>>,
}

impl MediaSender {
    /// `socket` is the endpoint's own (a clone of what quinn reads); `conn` names the peer.
    pub fn new(socket: &UdpSocket, conn: quinn::Connection) -> std::io::Result<MediaSender> {
        let socket = socket.try_clone()?;
        let state = UdpSocketState::new((&socket).into())?;
        let v6_socket = socket.local_addr()?.is_ipv6();
        Ok(MediaSender {
            socket,
            state,
            conn,
            v6_socket,
            gso: AtomicBool::new(true),
            staging: Mutex::new(Vec::new()),
        })
    }

    /// Where the next packet goes and from which local address: the connection's current path.
    fn path(&self) -> (SocketAddr, Option<IpAddr>) {
        let to = canonical(self.conn.remote_address());
        let to = match to {
            SocketAddr::V4(v4) if self.v6_socket => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            other => other,
        };
        (to, self.conn.local_ip())
    }

    fn transmit(
        &self,
        to: SocketAddr,
        from: Option<IpAddr>,
        contents: &[u8],
        segment: Option<usize>,
    ) -> std::io::Result<()> {
        self.state.send(
            (&self.socket).into(),
            &Transmit {
                destination: to,
                ecn: None,
                contents,
                segment_size: segment,
                src_ip: from,
            },
        )
    }
}

impl Transport for MediaSender {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        let (to, from) = self.path();
        match self.transmit(to, from, packet, None) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        let (to, from) = self.path();
        for (i, p) in packets.iter().enumerate() {
            match self.transmit(to, from, p, None) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(i),
                Err(e) => return Err(e),
            }
        }
        Ok(packets.len())
    }

    /// Runs of equal-size packets leave as one offloaded send, each run within the platform's
    /// segment limit and the IPv6+UDP payload bound; the last packet of a run may be shorter.
    /// An oversize run is `EMSGSIZE`, which quinn-udp reports as sent.
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        const GSO_MAX_PAYLOAD: usize = 65535 - 40 - 8;
        let max = self.state.max_gso_segments();
        if !self.gso.load(Ordering::Relaxed) || max <= 1 {
            return self.send_batch(packets);
        }
        let (to, from) = self.path();
        let mut staging = self.staging.lock().unwrap_or_else(|e| e.into_inner());
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
            staging.clear();
            for p in &packets[sent..end] {
                staging.extend_from_slice(p);
            }
            let segment = (end - sent > 1).then_some(size);
            match self.transmit(to, from, &staging, segment) {
                Ok(()) => sent = end,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(sent),
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
    }

    fn set_gso(&self, on: bool) {
        self.gso.store(on, Ordering::Relaxed);
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    async fn pair() -> (
        quinn::Endpoint,
        std::net::UdpSocket,
        quinn::Endpoint,
        ClientMedia,
        quinn::Connection,
        quinn::Connection,
    ) {
        use crate::quic::endpoint;
        let (cert, key) = endpoint::generate_identity().unwrap();
        let (server, socket) = endpoint::server_shared(
            "127.0.0.1:0".parse().unwrap(),
            &cert,
            &key,
            Duration::from_secs(8),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        let (client, _) =
            endpoint::client_shared(None, None, &[crate::quic::v2::registry::ALPN, b"pkf1"]);
        let (client, media) = client.unwrap();
        let accept = tokio::spawn({
            let server = server.clone();
            async move { server.accept().await.unwrap().await.unwrap() }
        });
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let host_conn = accept.await.unwrap();
        media.stats().set_host(addr);
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
        let mut out = vec![vec![0u8; 9217]; 64];
        let mut lens = vec![0usize; 64];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = 0;
        while got < N && std::time::Instant::now() < deadline {
            let n = media.recv_batch(&mut out, &mut lens).unwrap();
            if n == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            for i in 0..n {
                let p = &out[i][..lens[i]];
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
        let st = media.stats();
        assert_eq!(got, N, "every media packet arrives on loopback");
        assert!(datagrams > 250, "QUIC datagrams kept flowing: {datagrams}");
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
        let mut out = vec![vec![0u8; 9217]; 16];
        let mut lens = vec![0usize; 16];
        assert_eq!(media.recv_batch(&mut out, &mut lens).unwrap(), 0);
        assert_eq!(media.stats().foreign.load(Ordering::Relaxed), 10);
    }
}
