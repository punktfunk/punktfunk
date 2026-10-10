//! In-process transport for unit tests and the C ABI harness. Two cross-wired
//! [`LoopbackTransport`]s form a host↔client link, with optional deterministic loss so
//! tests can exercise FEC recovery without a real network.

use super::Transport;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Channel {
    queue: Mutex<VecDeque<Vec<u8>>>,
    /// Drop one of every `drop_period` packets (0 = lossless).
    drop_period: u32,
    /// Lose this many packets at every frame's start or end ([`loopback_drop_head`]).
    edge: Option<(Edge, u32)>,
    frame: Mutex<EdgeFrame>,
    sent: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Head,
    Tail,
}

/// The frame an edge drop is in: its index, its packets so far, and a tail drop's held ones.
#[derive(Default)]
struct EdgeFrame {
    index: Option<u32>,
    seen: u32,
    held: Vec<Vec<u8>>,
}

impl Channel {
    fn new(drop_period: u32) -> Arc<Channel> {
        Channel::with(drop_period, None)
    }

    fn with(drop_period: u32, edge: Option<(Edge, u32)>) -> Arc<Channel> {
        Arc::new(Channel {
            queue: Mutex::new(VecDeque::new()),
            drop_period,
            edge,
            frame: Mutex::default(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    /// One packet under an edge drop. A tail drop holds a frame until the next one starts,
    /// when its last packets are known.
    fn send_edge(&self, packet: &[u8], (edge, n): (Edge, u32)) {
        let index = packet
            .get(8..12)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let mut f = self.frame.lock().unwrap();
        let mut queue = self.queue.lock().unwrap();
        if f.index != index {
            let held = std::mem::take(&mut f.held);
            let keep = held.len().saturating_sub(n as usize);
            self.dropped
                .fetch_add((held.len() - keep) as u64, Ordering::Relaxed);
            queue.extend(held.into_iter().take(keep));
            (f.index, f.seen) = (index, 0);
        }
        f.seen += 1;
        match edge {
            Edge::Head if f.seen <= n => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Edge::Head => queue.push_back(packet.to_vec()),
            Edge::Tail => f.held.push(packet.to_vec()),
        }
    }
}

/// Cross-wired pair half, created by [`loopback_pair`].
pub struct LoopbackTransport {
    tx: Arc<Channel>,
    rx: Arc<Channel>,
}

impl LoopbackTransport {
    pub fn dropped(&self) -> u64 {
        self.tx.dropped.load(Ordering::Relaxed)
    }
}

/// Create a connected `(host, client)` pair. `host_drop_period` injects loss on the
/// host→client (video) path; `client_drop_period` on the reverse (input) path.
pub fn loopback_pair(
    host_drop_period: u32,
    client_drop_period: u32,
) -> (LoopbackTransport, LoopbackTransport) {
    let h2c = Channel::new(host_drop_period);
    let c2h = Channel::new(client_drop_period);
    let host = LoopbackTransport {
        tx: h2c.clone(),
        rx: c2h.clone(),
    };
    let client = LoopbackTransport { tx: c2h, rx: h2c };
    (host, client)
}

/// A pair whose video path loses the first `n` packets of every frame: a receiver whose
/// adapter wakes late. Frames are told apart by the unsealed `punktfunk/2` header.
pub fn loopback_drop_head(n: u32) -> (LoopbackTransport, LoopbackTransport) {
    edge_pair(Edge::Head, n)
}

/// A pair whose video path loses the last `n` packets of every frame: a queue that
/// tail-drops each burst. A frame is delivered when the next one starts.
pub fn loopback_drop_tail(n: u32) -> (LoopbackTransport, LoopbackTransport) {
    edge_pair(Edge::Tail, n)
}

fn edge_pair(edge: Edge, n: u32) -> (LoopbackTransport, LoopbackTransport) {
    let h2c = Channel::with(0, Some((edge, n)));
    let c2h = Channel::new(0);
    let host = LoopbackTransport {
        tx: h2c.clone(),
        rx: c2h.clone(),
    };
    (host, LoopbackTransport { tx: c2h, rx: h2c })
}

impl Transport for LoopbackTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        if let Some(edge) = self.tx.edge {
            self.tx.sent.fetch_add(1, Ordering::Relaxed);
            self.tx.send_edge(packet, edge);
            return Ok(true);
        }
        let n = self.tx.sent.fetch_add(1, Ordering::Relaxed);
        if self.tx.drop_period != 0 && (n % self.tx.drop_period as u64) == 0 {
            // Network loss: the packet left the sender, then vanished. Report `Ok(true)`
            // so recv/FEC handle it. `Ok(false)` is reserved for WouldBlock send-buffer overflow.
            self.tx.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        self.tx.queue.lock().unwrap().push_back(packet.to_vec());
        Ok(true)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.rx.queue.lock().unwrap().pop_front())
    }
}
