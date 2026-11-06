//! MTU clamp and jumbo grow for the video data plane.
//!
//! Video datagrams are sealed at a per-session `shard_payload` sized for a
//! 1500-byte MTU. A smaller hop still carries QUIC control while every video
//! datagram dies to fragmentation or `WSAEMSGSIZE`; the client reports
//! `loss_ppm=0`. Control discovery probes up to
//! [`video_datagram_udp_ceiling`], so a settled MTU below that is the path
//! verdict.
//!
//! Pin with `PUNKTFUNK_WIRE_MTU=<bytes>`. Watch records a below-ceiling
//! settle; the next handshake clamps. Jumbo follows this host's adapter toward
//! the client and grows only to what the path proved. It starts the next session
//! at that size only after a live re-proof — PyroWave cannot re-key mid-stream.
//! Evidence: [`jumbo_session_start`]. The watch lives as long as the connection:
//! a client that moves to a new address is re-measured.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use punktfunk_core::config::{
    jumbo_shard_payload_for, jumbo_wire_mtu_toward, mtu1500_shard_payload_for,
    sealed_datagram_bytes, shard_payload_for_udp_budget, shard_payload_for_wire_mtu,
    video_datagram_udp_ceiling,
};
use punktfunk_core::packet::WIRE_OVERHEAD;
use punktfunk_core::quic::PATH_SHRANK_CLOSE_CODE;

/// Mid-session re-key driver. `None` at [`spawn_watch`] observes and learns,
/// and ends a session a move left too big. Built only when the client advertised per-frame geometry and the
/// wire is not chunk-aligned — PyroWave pins `Welcome` once over the C ABI,
/// so a mid-stream re-key would corrupt its parse.
pub(super) struct ShardReneg {
    /// Sole writer of the control stream: `ShardPayloadChanged{n}`.
    pub change_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    /// Client `ShardPayloadAck`s — the grow does not apply until one matches.
    pub ack_rx: tokio::sync::mpsc::Receiver<u16>,
    /// Encode loop: [`Session::set_shard_payload`] between AUs, next to `bitrate_rx`.
    pub apply_tx: std::sync::mpsc::Sender<usize>,
}

/// Per-peer UDP-payload budget from a live below-ceiling settle. In-memory:
/// a restart re-learns in one session; a later ceiling-hit erases.
fn learned() -> &'static Mutex<HashMap<IpAddr, u16>> {
    static LEARNED: OnceLock<Mutex<HashMap<IpAddr, u16>>> = OnceLock::new();
    LEARNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Jumbo-verdict key: a path, not a peer.
///
/// The clamp is keyed by peer IP; a stale clamp only shrinks datagrams. A
/// stale grow is the opposite — one oversized datagram is silently dropped.
/// A verdict earned on one host NIC must not apply to the same peer over
/// another route.
///
/// `local` is `Connection::local_ip()`; `None` when the platform cannot
/// report it. That degrades to the clamp's key — the live re-proof in
/// [`jumbo_session_start`] is what actually authorises a grow.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct PathKey {
    local: Option<IpAddr>,
    peer: IpAddr,
}

/// A completed MTU-discovery search that settled at a jumbo shard.
#[derive(Clone, Copy, Debug)]
struct JumboVerdict {
    udp_budget: u16,
    /// Jumbo target at proof time. A later change (adapter MTU or an override)
    /// invalidates rather than reinterpreting.
    target_wire_mtu: usize,
    /// Instant of the proof; see [`JUMBO_VERDICT_TTL`].
    at: std::time::Instant,
}

/// Bound on an unrefreshed verdict (path changed with no session running).
/// Contrary evidence erases long before this; the TTL is not the safety gate.
const JUMBO_VERDICT_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Cap on waiting for this connection's MTU discovery to re-prove jumbo
/// before `Welcome`. Discovery restarts from ~1200 bytes and needs an
/// acked probe per binary-search step; without a wait the grow never
/// fires. Entered only for a path a previous session already proved.
/// A miss is self-limiting: the watcher erases the verdict.
const JUMBO_PROOF_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
const JUMBO_PROOF_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Proven-jumbo paths. Same lifetime as [`learned`]: in-memory, re-earned
/// after a host restart.
fn jumbo_verdicts() -> &'static Mutex<HashMap<PathKey, JumboVerdict>> {
    static JUMBO: OnceLock<Mutex<HashMap<PathKey, JumboVerdict>>> = OnceLock::new();
    JUMBO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn path_key(conn: &super::link::SessionLink) -> PathKey {
    PathKey {
        local: conn.local_ip(),
        peer: conn.remote_address().ip(),
    }
}

/// Inputs to the session-start jumbo decision. Every field but
/// `proven_udp_budget` is observed on this connection during this handshake.
#[derive(Clone, Copy, Debug)]
struct JumboStart {
    /// Jumbo target for this path ([`jumbo_wire_mtu_toward`]); `None` = never jumbo.
    target_wire_mtu: Option<usize>,
    /// `Hello::max_shard_payload`. 0 = legacy client: never a geometry it
    /// did not advertise.
    client_ceiling: u16,
    /// Largest UDP payload quinn has had acked on this connection.
    live_udp_mtu: u16,
    /// Prior session over this [`PathKey`], if any.
    proven_udp_budget: Option<u16>,
    /// [`learned`] clamp for this peer. Contradictory evidence vetoes the
    /// grow — the two memories are keyed differently and the safe one wins.
    clamped_udp_budget: Option<u16>,
}

/// Jumbo shard for `peer` that seals within `udp_budget`, or `None` when there is nothing to
/// gain. The budget is a settle above the 1500 wire; `u16::MAX` asks for the target alone. Ends
/// whose adapters differ settle below the larger one, so the settle, not the target, sizes it.
/// Shared by the decision, the wait, and the watcher so all three use one number.
fn jumbo_target(
    target_wire_mtu: Option<usize>,
    client_ceiling: u16,
    udp_budget: u16,
    peer: IpAddr,
) -> Option<usize> {
    let mtu = target_wire_mtu?;
    if udp_budget as usize <= video_datagram_udp_ceiling() {
        return None;
    }
    let t = jumbo_shard_payload_for(mtu, peer)
        .min(client_ceiling as usize)
        .min((udp_budget as usize).saturating_sub(WIRE_OVERHEAD));
    let t = t - t % 2; // FEC requires even shards
    (t > mtu1500_shard_payload_for(peer)).then_some(t)
}

/// Session-start jumbo shard, or `None`: what a remembered verdict proved, once
/// `live_udp_mtu` — an acked datagram of that size on this connection — re-proves
/// it. PyroWave cannot re-key mid-stream, so a stale memory must never seal a
/// jumbo `Welcome`.
fn jumbo_session_start(i: JumboStart, peer: IpAddr) -> Option<usize> {
    let target = jumbo_target(
        i.target_wire_mtu,
        i.client_ceiling,
        i.proven_udp_budget?,
        peer,
    )?;
    let sealed = sealed_datagram_bytes(target);
    if let Some(clamp) = i.clamped_udp_budget {
        if (clamp as usize) < sealed {
            return None;
        }
    }
    if (i.live_udp_mtu as usize) < sealed {
        return None;
    }
    Some(target)
}

/// Shard payload for a new session on `conn`, under the session's `PUNKTFUNK_WIRE_MTU`
/// (`env`). Async only for the bounded [`JUMBO_PROOF_WAIT`], entered only on a path a
/// previous session already proved jumbo.
pub(super) async fn negotiated_shard_payload(
    conn: &super::link::SessionLink,
    client_ceiling: u16,
    env: Option<usize>,
) -> usize {
    let peer = conn.remote_address().ip();
    let learned_budget = learned().lock().unwrap().get(&peer).copied();
    let target_wire_mtu = jumbo_wire_mtu_toward(peer);
    let proven_udp_budget = fresh_verdict(path_key(conn), target_wire_mtu);
    let mut jumbo = JumboStart {
        target_wire_mtu,
        client_ceiling,
        live_udp_mtu: conn.current_mtu(),
        proven_udp_budget,
        clamped_udp_budget: learned_budget,
    };
    let awaited_proof = proven_udp_budget
        .and_then(|b| jumbo_target(target_wire_mtu, client_ceiling, b, peer))
        .map(|t| sealed_datagram_bytes(t) as u16);
    if let Some(sealed) = awaited_proof {
        if jumbo.live_udp_mtu < sealed {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < JUMBO_PROOF_WAIT {
                tokio::time::sleep(JUMBO_PROOF_POLL).await;
                jumbo.live_udp_mtu = conn.current_mtu();
                if jumbo.live_udp_mtu >= sealed {
                    break;
                }
            }
            tracing::debug!(
                peer = %peer,
                waited_ms = t0.elapsed().as_millis() as u64,
                live_udp_mtu = jumbo.live_udp_mtu,
                needed = sealed,
                "wire MTU: waited for this connection to re-prove its jumbo path"
            );
        }
    }
    resolve(env, learned_budget, jumbo, peer)
}

/// Redeemable jumbo verdict: same operator target, inside the TTL. A miss
/// is dropped here rather than left stale.
fn fresh_verdict(key: PathKey, target_wire_mtu: Option<usize>) -> Option<u16> {
    let target = target_wire_mtu?;
    let mut map = jumbo_verdicts().lock().unwrap();
    let v = *map.get(&key)?;
    if v.target_wire_mtu != target || v.at.elapsed() > JUMBO_VERDICT_TTL {
        map.remove(&key);
        return None;
    }
    Some(v.udp_budget)
}

/// Proven jumbo, else an env pin below 1500, else learned budget, else family default.
/// Tested core of [`negotiated_shard_payload`].
fn resolve(
    env_wire_mtu: Option<usize>,
    learned_udp_budget: Option<u16>,
    jumbo: JumboStart,
    peer: IpAddr,
) -> usize {
    let default = mtu1500_shard_payload_for(peer);
    // Jumbo first: a jumbo target is only above 1500, and only a proven,
    // re-proven path starts there.
    if let Some(p) = jumbo_session_start(jumbo, peer) {
        tracing::info!(
            peer = %peer,
            shard_payload = p,
            default,
            live_udp_mtu = jumbo.live_udp_mtu,
            proven_udp_budget = jumbo.proven_udp_budget,
            "wire MTU: session starts at the JUMBO shard — this path proved it in a previous \
             session AND re-proved it live on this connection (~6× fewer datagrams per frame)"
        );
        return p;
    }
    // Below 1500 the variable pins a smaller wire; 1500 and up is the jumbo opt-in, which
    // leaves a small path to its learned clamp.
    if let Some(mtu) = env_wire_mtu.filter(|&m| m < 1500) {
        let p = shard_payload_for_wire_mtu(mtu, peer);
        if p != default {
            tracing::info!(
                wire_mtu = mtu,
                shard_payload = p,
                default,
                "wire MTU: shard payload set from PUNKTFUNK_WIRE_MTU"
            );
        }
        return p;
    }
    if let Some(budget) = learned_udp_budget {
        let p = shard_payload_for_udp_budget(budget as usize, peer);
        if p != default {
            tracing::info!(
                peer = %peer,
                udp_budget = budget,
                shard_payload = p,
                default,
                "wire MTU: shard payload clamped to this peer's measured path MTU (learned \
                 from a prior session's QUIC MTU discovery) — video datagrams now fit the \
                 constrained hop"
            );
            return p;
        }
    }
    default
}

/// How often the watch looks at the connection: how soon a client's move is seen.
const TICK: Duration = Duration::from_millis(250);
/// The jumbo revert reads `current_mtu` once per this many ticks (5 s).
const REVERT_TICKS: u32 = 20;
/// After a move, how long a session that cannot re-key gives discovery to finish climbing
/// before a path that is too small ends it.
const SHRANK_CONFIRM: Duration = Duration::from_secs(5);

/// The shard to drop to when the path carries `udp_budget`, or `None` while `current` fits.
fn shrink_to(current: usize, udp_budget: u16, peer: IpAddr) -> Option<usize> {
    if sealed_datagram_bytes(current) <= udp_budget as usize {
        return None;
    }
    let to = shard_payload_for_udp_budget(udp_budget as usize, peer);
    (to < current).then_some(to)
}

/// The shard a settle at `settled` grows to: back toward `start` after a move, or a `jumbo`
/// target the settle proves. `None` when that is no bigger than `current`.
fn grow_to(
    settled: u16,
    start: usize,
    current: usize,
    jumbo: Option<usize>,
    peer: IpAddr,
) -> Option<usize> {
    let back = shard_payload_for_udp_budget(settled as usize, peer).min(start);
    let jumbo = jumbo.filter(|&t| settled as usize >= sealed_datagram_bytes(t));
    let to = back.max(jumbo.unwrap_or(0));
    (to > current).then_some(to)
}

/// Watch the path MTU for the connection's life.
///
/// Each settle files a path verdict for the next session and, with a [`ShardReneg`] driver,
/// shrinks the live wire to fit or grows it (ack-gated). A client move that restarts quinn's
/// discovery shrinks video to the restart floor at once; the next settle grows it back toward
/// the session's start shard. A session without a driver whose video does not fit the path
/// closes with [`PATH_SHRANK_CLOSE_CODE`], and the client redials at a size that fits.
pub(super) fn spawn_watch(
    conn: super::link::SessionLink,
    session_shard_payload: usize,
    client_ceiling: u16,
    reneg: Option<ShardReneg>,
) {
    let target_wire_mtu = jumbo_wire_mtu_toward(conn.remote_address().ip());
    let watch = Watch {
        conn,
        start: session_shard_payload,
        current: session_shard_payload,
        client_ceiling,
        reneg,
        target_wire_mtu,
    };
    tokio::spawn(watch.run());
}

/// [`spawn_watch`]'s state.
struct Watch {
    conn: super::link::SessionLink,
    /// The shard `Welcome` sealed. A grow back after a move stops here.
    start: usize,
    /// The shard the encoder seals now.
    current: usize,
    client_ceiling: u16,
    reneg: Option<ShardReneg>,
    /// Read once: a filed jumbo verdict stores this same target.
    target_wire_mtu: Option<usize>,
}

impl Watch {
    async fn run(mut self) {
        let mut addr = self.conn.remote_address();
        let mut moved = false;
        loop {
            let settled = match self.settle(addr).await {
                Ok(settled) => settled,
                Err(next) => {
                    (addr, moved) = (next, true);
                    self.on_move(addr);
                    continue;
                }
            };
            let peer = addr.ip();
            self.file_jumbo_verdict(peer, settled);
            self.learn(peer, settled);
            if self.conn.close_reason().is_some() || !self.fit(addr, settled, moved).await {
                return;
            }
            self.grow(peer, settled).await;
            let Some(next) = self.hold(addr).await else {
                return;
            };
            (addr, moved) = (next, true);
            self.on_move(addr);
        }
    }

    /// Discovery's settle: the larger of samples at 3 s and 7 s. LAN takes a handful of RTTs;
    /// a constrained path needs a loss timeout per failed probe. `Err` carries the client's new
    /// address when it moves first.
    async fn settle(&self, addr: SocketAddr) -> Result<u16, SocketAddr> {
        // Jumbo-or-ceiling, so a jumbo search is not cut off at 1500.
        let goal = jumbo_target(
            self.target_wire_mtu,
            self.client_ceiling,
            u16::MAX,
            addr.ip(),
        )
        .map_or(0, sealed_datagram_bytes)
        .max(video_datagram_udp_ceiling()) as u16;
        let t0 = tokio::time::Instant::now();
        let mut settled = 0u16;
        for mark in [3u64, 7] {
            while t0.elapsed() < Duration::from_secs(mark) {
                tokio::time::sleep(TICK).await;
                let now = self.conn.remote_address();
                if now != addr {
                    return Err(now);
                }
            }
            settled = settled.max(self.conn.current_mtu());
            if settled >= goal {
                break;
            }
        }
        Ok(settled)
    }

    /// File or erase the next-session jumbo verdict for this path. Only a live connection whose
    /// settle proves a jumbo shard files one; a 1500 settle or a death mid-window drops it.
    /// A live miss is logged either way: it is the host's only sign that jumbo is on.
    fn file_jumbo_verdict(&self, peer: IpAddr, settled: u16) {
        let (target, ceiling) = (self.target_wire_mtu, self.client_ceiling);
        let Some(goal) = jumbo_target(target, ceiling, u16::MAX, peer).map(sealed_datagram_bytes)
        else {
            return;
        };
        let key = path_key(&self.conn);
        let proven = jumbo_target(target, ceiling, settled, peer).is_some();
        if proven && self.conn.close_reason().is_none() {
            jumbo_verdicts().lock().unwrap().insert(
                key,
                JumboVerdict {
                    udp_budget: settled,
                    target_wire_mtu: self.target_wire_mtu.unwrap_or_default(),
                    at: std::time::Instant::now(),
                },
            );
            tracing::info!(peer = %peer, discovered_udp_mtu = settled, goal,
                "wire MTU: this path carries JUMBO video datagrams — the next session over \
                 it starts at the big shard (it still has to re-prove the path live)");
        } else if jumbo_verdicts().lock().unwrap().remove(&key).is_some() {
            tracing::info!(peer = %peer, discovered_udp_mtu = settled, goal,
                "wire MTU: jumbo verdict cleared — this path no longer proves it");
        } else if self.conn.close_reason().is_none() {
            // 1472 is also what a client without a jumbo adapter, or an older one, accepts.
            tracing::info!(peer = %peer, discovered_udp_mtu = settled, goal,
                "wire MTU: jumbo not proven on this path — the client's adapter or a switch \
                 on the path stops at MTU 1500, or the client predates automatic jumbo");
        }
    }

    /// The clamp the next session from `peer` starts with. A full-size settle clears it; a
    /// lower one counts only from a connection alive through the window, since a closed one
    /// stops discovering and a high-RTT path can still be mid-search.
    fn learn(&self, peer: IpAddr, settled: u16) {
        if settled >= video_datagram_udp_ceiling() as u16 {
            if learned().lock().unwrap().remove(&peer).is_some() {
                tracing::info!(peer = %peer,
                    "wire MTU: path re-measured at full size — learned clamp cleared");
            }
        } else if self.conn.close_reason().is_none() {
            learned().lock().unwrap().insert(peer, settled);
        }
    }

    /// Shrink this session to fit `settled` if it does not. A session with no re-key driver
    /// that still does not fit after [`SHRANK_CONFIRM`] is ended, whether the client moved or
    /// the first settle found the path small: the learned clamp sizes its redial. `false` once
    /// the session is ended.
    async fn fit(&mut self, addr: SocketAddr, settled: u16, moved: bool) -> bool {
        let peer = addr.ip();
        let ceiling = video_datagram_udp_ceiling() as u16;
        if sealed_datagram_bytes(self.current) <= settled as usize {
            if settled < ceiling {
                tracing::info!(peer = %peer, discovered_udp_mtu = settled,
                    "wire MTU: constrained path re-measured; this session's video is sized to fit");
            }
            return true;
        }
        if settled >= ceiling {
            tracing::warn!(
                peer = %peer,
                discovered_udp_mtu = settled,
                shard_payload = self.current,
                "wire MTU: this session started at a JUMBO shard but the path does not \
                 carry it — video datagrams are oversized for a hop, which streams as a \
                 black screen with zero reported loss. The jumbo verdict for this path is \
                 cleared: the next connect starts at the standard 1500-byte wire."
            );
        } else {
            tracing::warn!(
                peer = %peer,
                discovered_udp_mtu = settled,
                needed_udp_mtu = ceiling,
                "wire MTU: this path CANNOT carry full-size video datagrams — the control \
                 plane works but every video packet is oversized for a hop, which streams as \
                 an endless black screen with zero reported loss. Typical cause: a VPN/overlay \
                 adapter (Tailscale / Cloudflare WARP / ZeroTier) claiming the LAN route, or a \
                 lowered NIC MTU — compare `ping <client> -f -l 1450` vs `-l 1200` and check \
                 `netsh interface ipv4 show subinterfaces` (Windows) / `ip link` (Linux). The \
                 measured budget is recorded: the NEXT session from this client sizes video to \
                 fit automatically. To pin it for all sessions set PUNKTFUNK_WIRE_MTU."
            );
        }
        if self.reneg.is_some() {
            self.shrink(settled, peer, "fit the measured path");
            return true;
        }
        tokio::time::sleep(SHRANK_CONFIRM).await;
        if self.conn.close_reason().is_some() {
            return false;
        }
        if self.conn.remote_address() != addr
            || sealed_datagram_bytes(self.current) <= self.conn.current_mtu() as usize
        {
            return true;
        }
        tracing::warn!(peer = %peer, discovered_udp_mtu = settled, shard_payload = self.current,
            moved,
            "wire MTU: this session's video does not fit the client's path, and this session \
             cannot be re-keyed — ending it so the client reconnects at a size that fits");
        let why = if moved {
            super::Said::new(
                "close_path_changed",
                "The network path changed and this stream can't be resized.",
            )
        } else {
            super::Said::new(
                "close_path_too_small",
                "This network path can't carry the stream at its packet size.",
            )
        };
        self.conn.refuse_said(PATH_SHRANK_CLOSE_CODE, &why).await;
        false
    }

    /// Re-key down to fit `udp_budget`. Smaller always fits, so the client's ack is telemetry.
    fn shrink(&mut self, udp_budget: u16, peer: IpAddr, why: &'static str) {
        let (Some(r), Some(to)) = (
            self.reneg.as_ref(),
            shrink_to(self.current, udp_budget, peer),
        ) else {
            return;
        };
        if r.change_tx.send(to as u16).is_ok() && r.apply_tx.send(to).is_ok() {
            tracing::info!(peer = %peer, shard_payload = to, was = self.current, udp_budget, why,
                "wire MTU: video re-keyed mid-session");
            self.current = to;
        }
    }

    /// Grow to what this settle proves (see [`grow_to`]). Ack-gated: no datagram above the old
    /// size leaves before the client's ack, even though its buffers are static. Only an ack
    /// that arrives after the ask counts.
    async fn grow(&mut self, peer: IpAddr, settled: u16) {
        let jumbo = jumbo_target(self.target_wire_mtu, self.client_ceiling, settled, peer);
        let Some(to) = grow_to(settled, self.start, self.current, jumbo, peer) else {
            return;
        };
        let Some(r) = self.reneg.as_mut() else {
            return;
        };
        while r.ack_rx.try_recv().is_ok() {}
        if r.change_tx.send(to as u16).is_err() {
            return;
        }
        let acked = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(v) = r.ack_rx.recv().await {
                if v as usize == to {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if !acked {
            tracing::warn!(peer = %peer, shard_payload = to,
                "wire MTU: grow not acked — staying at the current wire");
            return;
        }
        if r.apply_tx.send(to).is_err() {
            return;
        }
        tracing::info!(peer = %peer, shard_payload = to, was = self.current,
            jumbo = jumbo == Some(to), "wire MTU: grow acked and applied");
        self.current = to;
    }

    /// Run until the client moves (`Some` with its new address) or the connection ends. A
    /// jumbo session meanwhile follows quinn's blackhole detection: once the big packets
    /// vanish it re-keys down, or, unable to, clears the path's verdict.
    async fn hold(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let peer = addr.ip();
        let mut ticks = 0u32;
        loop {
            tokio::time::sleep(TICK).await;
            if self.conn.close_reason().is_some() {
                return None;
            }
            let now = self.conn.remote_address();
            if now != addr {
                return Some(now);
            }
            ticks += 1;
            if ticks % REVERT_TICKS != 0 || self.current <= mtu1500_shard_payload_for(peer) {
                continue;
            }
            let mtu_now = self.conn.current_mtu();
            if mtu_now as usize >= sealed_datagram_bytes(self.current) {
                continue;
            }
            if jumbo_verdicts()
                .lock()
                .unwrap()
                .remove(&path_key(&self.conn))
                .is_some()
            {
                tracing::warn!(peer = %peer, discovered_udp_mtu = mtu_now,
                    shard_payload = self.current,
                    "wire MTU: the jumbo path this session runs on stopped fitting — the \
                     verdict is cleared and the next connect starts at the standard wire");
            }
            self.shrink(mtu_now, peer, "jumbo path stopped fitting");
        }
    }

    /// The client's address moved. If quinn restarted discovery the new path is unproven, so
    /// video drops to the restart floor now and the next settle grows it back.
    fn on_move(&mut self, addr: SocketAddr) {
        let floor = self.conn.current_mtu();
        tracing::info!(peer = %addr, current_udp_mtu = floor, shard_payload = self.current,
            "wire MTU: client moved to a new address");
        self.shrink(
            floor,
            addr.ip(),
            "client moved; holding the floor until the path settles",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
    const V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    const NO_JUMBO: JumboStart = JumboStart {
        target_wire_mtu: None,
        client_ceiling: 0,
        live_udp_mtu: 0,
        proven_udp_budget: None,
        clamped_udp_budget: None,
    };
    fn proven_jumbo() -> JumboStart {
        JumboStart {
            target_wire_mtu: Some(9000),
            client_ceiling: punktfunk_core::config::max_shard_payload() as u16,
            live_udp_mtu: 8972,
            proven_udp_budget: Some(8972),
            clamped_udp_budget: None,
        }
    }

    #[test]
    fn default_when_nothing_known() {
        assert_eq!(
            resolve(None, None, NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(None, None, NO_JUMBO, V6),
            mtu1500_shard_payload_for(V6)
        );
    }

    #[test]
    fn env_override_beats_learned() {
        // 1280 wire − 28 IP/UDP − 46 header/tag = 1206.
        assert_eq!(resolve(Some(1280), Some(1472), NO_JUMBO, V4), 1206);
    }

    #[test]
    fn learned_budget_clamps() {
        // 1280-byte UDP budget − 46 header/tag = 1234.
        assert_eq!(resolve(None, Some(1280), NO_JUMBO, V4), 1234);
    }

    /// 1500 and up opts in to jumbo; it pins nothing, so a small path's verdict still binds.
    #[test]
    fn a_jumbo_opt_in_still_takes_the_learned_clamp() {
        assert_eq!(resolve(Some(9000), Some(1280), NO_JUMBO, V4), 1234);
        assert_eq!(resolve(Some(1500), Some(1280), NO_JUMBO, V4), 1234);
    }

    #[test]
    fn learned_at_or_above_ceiling_is_the_default_wire() {
        assert_eq!(
            resolve(None, Some(1472), NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(None, Some(2000), NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
    }

    #[test]
    fn env_full_mtu_is_the_default_wire_both_families() {
        assert_eq!(
            resolve(Some(1500), None, NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(Some(1500), None, NO_JUMBO, V6),
            mtu1500_shard_payload_for(V6)
        );
    }

    /// 9000 − 28 (IPv4) − 46 = 8926; 9000 − 48 − 46 = 8906.
    #[test]
    fn proven_and_reproven_path_starts_jumbo() {
        assert_eq!(jumbo_session_start(proven_jumbo(), V4), Some(8926));
        let mut v6 = proven_jumbo();
        v6.live_udp_mtu = 8952;
        v6.proven_udp_budget = Some(8952);
        assert_eq!(jumbo_session_start(v6, V6), Some(8906));
        // `resolve` must pick jumbo ahead of the env branch, which would
        // clamp a >1500 `PUNKTFUNK_WIRE_MTU` to the family default.
        assert_eq!(resolve(Some(9000), None, proven_jumbo(), V4), 8926);
    }

    #[test]
    fn a_remembered_verdict_never_grows_without_a_live_reproof() {
        let mut moved = proven_jumbo();
        moved.live_udp_mtu = 1472;
        assert_eq!(jumbo_session_start(moved, V4), None);
        assert_eq!(
            resolve(None, None, moved, V4),
            mtu1500_shard_payload_for(V4)
        );
        // 8971 is one byte short of the sealed jumbo target.
        let mut nearly = proven_jumbo();
        nearly.live_udp_mtu = 8971;
        assert_eq!(jumbo_session_start(nearly, V4), None);
    }

    /// Both halves required: a fluke on either side must not seal jumbo.
    #[test]
    fn a_live_proof_alone_does_not_grow() {
        let mut first_ever = proven_jumbo();
        first_ever.proven_udp_budget = None;
        assert_eq!(jumbo_session_start(first_ever, V4), None);
        let mut weak_memory = proven_jumbo();
        weak_memory.proven_udp_budget = Some(1472);
        assert_eq!(jumbo_session_start(weak_memory, V4), None);
    }

    /// Clamp is keyed by peer, verdict by route. On disagreement, small wins.
    #[test]
    fn a_constrained_path_clamp_vetoes_the_grow() {
        let mut contradicted = proven_jumbo();
        contradicted.clamped_udp_budget = Some(1280);
        assert_eq!(jumbo_session_start(contradicted, V4), None);
        // A clamp at or above the sealed target is not contrary evidence.
        let mut roomy = proven_jumbo();
        roomy.clamped_udp_budget = Some(8972);
        assert_eq!(jumbo_session_start(roomy, V4), Some(8926));
    }

    #[test]
    fn without_a_jumbo_target_nothing_grows() {
        let mut no_target = proven_jumbo();
        no_target.target_wire_mtu = None;
        assert_eq!(jumbo_session_start(no_target, V4), None);
    }

    /// Adapters that differ settle below the larger one, or a few bytes short where the search
    /// stopped: the proven budget sizes the shard, and a 1500 settle is never jumbo.
    #[test]
    fn the_proven_budget_sizes_the_jumbo_shard() {
        let mut bigger_host = proven_jumbo();
        bigger_host.target_wire_mtu = Some(9216);
        assert_eq!(jumbo_session_start(bigger_host, V4), Some(8926));
        let mut short = proven_jumbo();
        short.proven_udp_budget = Some(8960);
        short.live_udp_mtu = 8960;
        assert_eq!(jumbo_session_start(short, V4), Some(8914));
        assert_eq!(jumbo_target(Some(9000), u16::MAX, 1472, V4), None);
        assert_eq!(jumbo_target(Some(9000), u16::MAX, 2000, V4), Some(1954));
    }

    /// Legacy (`Hello::max_shard_payload` = 0) never gets a geometry it did
    /// not advertise. A ceiling under the family default is not a grow.
    #[test]
    fn the_client_ceiling_is_binding() {
        let mut legacy = proven_jumbo();
        legacy.client_ceiling = 0;
        assert_eq!(jumbo_session_start(legacy, V4), None);
        let mut small = proven_jumbo();
        small.client_ceiling = 1408;
        assert_eq!(jumbo_session_start(small, V4), None);
        // A mid-range ceiling caps the grow; the proof covers that smaller seal.
        let mut capped = proven_jumbo();
        capped.client_ceiling = 4000;
        assert_eq!(jumbo_session_start(capped, V4), Some(4000));
    }

    /// Grown shards stay even (Leopard FEC splits in halves) and inside the
    /// receive ceiling clients size buffers from.
    #[test]
    fn grown_shards_stay_even_and_inside_the_receive_ceiling() {
        for mtu in [2000usize, 4000, 4001, 9000, 9216, 64000] {
            for peer in [V4, V6] {
                let Some(t) = jumbo_target(Some(mtu), u16::MAX, u16::MAX, peer) else {
                    continue;
                };
                assert_eq!(t % 2, 0, "odd shard for mtu {mtu}");
                assert!(t <= punktfunk_core::config::max_shard_payload());
                assert!(t > mtu1500_shard_payload_for(peer));
                assert!(
                    sealed_datagram_bytes(t) <= punktfunk_core::packet::MAX_DATAGRAM_BYTES,
                    "sealed datagram overflows the receive ceiling at mtu {mtu}"
                );
            }
        }
        // A wire whose fit is the family default gains nothing.
        let default_wire = sealed_datagram_bytes(mtu1500_shard_payload_for(V4)) + 28;
        assert_eq!(
            jumbo_target(Some(default_wire), u16::MAX, u16::MAX, V4),
            None
        );
        assert_eq!(jumbo_target(None, u16::MAX, u16::MAX, V4), None);
    }

    /// Same peer over two host NICs is two routes, so two keys.
    #[test]
    fn the_verdict_key_separates_routes_to_the_same_peer() {
        let over_10g = PathKey {
            local: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            peer: V4,
        };
        let over_wifi = PathKey {
            local: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            peer: V4,
        };
        assert_ne!(over_10g, over_wifi);
        assert_ne!(
            over_10g,
            PathKey {
                local: None,
                peer: V4
            }
        );
    }

    /// A move that restarts discovery at quinn's 1200-byte floor drops a full-size session to
    /// the floor's shard; one that keeps the path's MTU leaves it alone.
    #[test]
    fn a_move_shrinks_only_what_no_longer_fits() {
        let full = mtu1500_shard_payload_for(V4);
        let floor = shard_payload_for_udp_budget(1200, V4);
        assert!(floor < full);
        assert_eq!(shrink_to(full, 1200, V4), Some(floor));
        assert_eq!(
            shrink_to(full, sealed_datagram_bytes(full) as u16, V4),
            None
        );
        assert_eq!(shrink_to(floor, 1200, V4), None);
    }

    /// After a move the next settle grows back toward the start shard, never past it, and
    /// only to what the settle proves. Without a move the start shard is already current.
    #[test]
    fn a_settle_grows_back_to_the_start_shard() {
        let full = mtu1500_shard_payload_for(V4);
        let floor = shard_payload_for_udp_budget(1200, V4);
        let ceiling = video_datagram_udp_ceiling() as u16;
        assert_eq!(grow_to(ceiling, full, floor, None, V4), Some(full));
        assert_eq!(
            grow_to(1280, full, floor, None, V4),
            Some(shard_payload_for_udp_budget(1280, V4))
        );
        assert_eq!(grow_to(ceiling, full, full, None, V4), None);
        assert_eq!(grow_to(1200, full, floor, None, V4), None);
        // A v4 session that moved to v6 grows to the v6 fit, not the v4 start.
        assert_eq!(
            grow_to(ceiling, full, floor, None, V6),
            Some(mtu1500_shard_payload_for(V6).min(full))
        );
    }

    /// The jumbo target wins only on a settle that carries it.
    #[test]
    fn a_settle_grows_to_jumbo_only_when_it_proves_it() {
        let full = mtu1500_shard_payload_for(V4);
        let jumbo = jumbo_target(Some(9000), u16::MAX, u16::MAX, V4);
        assert!(jumbo.is_some());
        assert_eq!(grow_to(8972, full, full, jumbo, V4), jumbo);
        let ceiling = video_datagram_udp_ceiling() as u16;
        assert_eq!(grow_to(ceiling, full, full, jumbo, V4), None);
    }
}
