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
//! settle; the next handshake clamps. Jumbo grow starts the next session at
//! the sealed jumbo size only after a live re-proof — PyroWave cannot re-key
//! mid-stream. Evidence: [`jumbo_session_start`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};

use punktfunk_core::config::{
    jumbo_shard_payload_for, jumbo_wire_mtu, mtu1500_shard_payload_for, sealed_datagram_bytes,
    shard_payload_for_udp_budget, shard_payload_for_wire_mtu, video_datagram_udp_ceiling,
};

/// Mid-session re-key driver. `None` at [`spawn_watch`] is observe-and-learn
/// only. Built only when the client advertised per-frame geometry and the
/// wire is not chunk-aligned — PyroWave pins `Welcome` once over the C ABI,
/// so a mid-stream re-key would corrupt its parse.
pub(super) struct ShardReneg {
    /// Client receive ceiling in shard bytes; > 0 by construction.
    pub client_ceiling: u16,
    /// Sole writer of the control stream: `ShardPayloadChanged{n}`.
    pub change_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    /// Client `ShardPayloadAck`s — the grow does not apply until one matches.
    pub ack_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
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

/// A completed MTU-discovery search that reached the sealed jumbo size.
#[derive(Clone, Copy, Debug)]
struct JumboVerdict {
    udp_budget: u16,
    /// Operator jumbo target at proof time. A later `PUNKTFUNK_JUMBO` /
    /// `PUNKTFUNK_WIRE_MTU` change invalidates rather than reinterpreting.
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
    /// Operator jumbo opt-in ([`jumbo_wire_mtu`]); `None` = never jumbo.
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

/// Jumbo shard for `peer`, or `None` when there is nothing to gain. Shared
/// by the decision, the wait, and the watcher so all three use one number.
fn jumbo_target(
    target_wire_mtu: Option<usize>,
    client_ceiling: u16,
    peer: IpAddr,
) -> Option<usize> {
    let mtu = target_wire_mtu?;
    let t = jumbo_shard_payload_for(mtu, peer).min(client_ceiling as usize);
    let t = t - t % 2; // FEC requires even shards
    (t > mtu1500_shard_payload_for(peer)).then_some(t)
}

/// Session-start jumbo shard, or `None`. A remembered verdict only decides
/// whether to wait; `live_udp_mtu` is the authorisation — an acked datagram
/// of that size on this connection. PyroWave cannot re-key mid-stream, so a
/// stale memory must never seal a jumbo `Welcome`.
fn jumbo_session_start(i: JumboStart, peer: IpAddr) -> Option<usize> {
    let target = jumbo_target(i.target_wire_mtu, i.client_ceiling, peer)?;
    let sealed = sealed_datagram_bytes(target);
    if let Some(clamp) = i.clamped_udp_budget {
        if (clamp as usize) < sealed {
            return None;
        }
    }
    if (i.proven_udp_budget? as usize) < sealed {
        return None;
    }
    if (i.live_udp_mtu as usize) < sealed {
        return None;
    }
    Some(target)
}

/// Shard payload for a new session on `conn`. Async only for the bounded
/// [`JUMBO_PROOF_WAIT`], entered only on a path a previous session already
/// proved jumbo.
pub(super) async fn negotiated_shard_payload(
    conn: &super::link::SessionLink,
    client_ceiling: u16,
) -> usize {
    let peer = conn.remote_address().ip();
    let env = match std::env::var("PUNKTFUNK_WIRE_MTU") {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(mtu) => Some(mtu),
            Err(_) => {
                tracing::warn!(value = %v, "PUNKTFUNK_WIRE_MTU is not a number — ignoring it");
                None
            }
        },
        Err(_) => None,
    };
    let learned_budget = learned().lock().unwrap().get(&peer).copied();
    let target_wire_mtu = jumbo_wire_mtu();
    let proven_udp_budget = fresh_verdict(path_key(conn), target_wire_mtu);
    let mut jumbo = JumboStart {
        target_wire_mtu,
        client_ceiling,
        live_udp_mtu: conn.current_mtu(),
        proven_udp_budget,
        clamped_udp_budget: learned_budget,
    };
    let awaited_proof = proven_udp_budget
        .and_then(|_| jumbo_target(target_wire_mtu, client_ceiling, peer))
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

/// Proven jumbo, else env override, else learned budget, else family default.
/// Tested core of [`negotiated_shard_payload`].
fn resolve(
    env_wire_mtu: Option<usize>,
    learned_udp_budget: Option<u16>,
    jumbo: JumboStart,
    peer: IpAddr,
) -> usize {
    let default = mtu1500_shard_payload_for(peer);
    // Jumbo first: `jumbo_wire_mtu()` is only above 1500, and the env branch
    // below clamps to the family default, so `PUNKTFUNK_WIRE_MTU=9000` would
    // otherwise start at 1408.
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
    if let Some(mtu) = env_wire_mtu {
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

/// After discovery settles, record a path verdict. With a [`ShardReneg`]
/// driver, shrink or (ack-gated) grow the live wire. A settled-at-jumbo
/// reading also files the next-session verdict. Without a grow the task
/// ends after the last sample (~10 s, a `Connection` handle); after a grow
/// or a jumbo start it stays as the revert guard until the connection closes.
pub(super) fn spawn_watch(
    conn: super::link::SessionLink,
    session_shard_payload: usize,
    client_ceiling: u16,
    reneg: Option<ShardReneg>,
) {
    tokio::spawn(async move {
        let peer = conn.remote_address().ip();
        let ceiling = video_datagram_udp_ceiling() as u16;
        // Sealed jumbo size for this path, or `None` without opt-in and
        // client headroom. Read once: the verdict stores that same target.
        let target_wire_mtu = jumbo_wire_mtu();
        let jumbo_proof =
            jumbo_target(target_wire_mtu, client_ceiling, peer).map(sealed_datagram_bytes);
        // Two samples: LAN is a handful of RTTs; a constrained path needs a
        // loss timeout per failed probe. `max`, because discovery only raises
        // `current_mtu` here. Goal is jumbo-or-ceiling so a jumbo search is
        // not cut off at 1500.
        let goal = jumbo_proof
            .unwrap_or(ceiling as usize)
            .max(ceiling as usize) as u16;
        let mut settled = 0u16;
        for wait_s in [3u64, 7] {
            tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
            settled = settled.max(conn.current_mtu());
            if settled >= goal {
                break;
            }
        }
        let mut current = session_shard_payload;
        let mut reneg = reneg;
        // File or erase the next-session jumbo verdict before any return.
        // Record only a live connection that reached the sealed target;
        // anything else (lower settle, died mid-window) drops it.
        if let Some(need) = jumbo_proof {
            let key = path_key(&conn);
            if settled as usize >= need && conn.close_reason().is_none() {
                jumbo_verdicts().lock().unwrap().insert(
                    key,
                    JumboVerdict {
                        udp_budget: settled,
                        target_wire_mtu: target_wire_mtu.unwrap_or_default(),
                        at: std::time::Instant::now(),
                    },
                );
                tracing::info!(peer = %peer, discovered_udp_mtu = settled, needed = need,
                    "wire MTU: this path carries JUMBO video datagrams — the next session over \
                     it starts at the big shard (it still has to re-prove the path live)");
            } else if jumbo_verdicts().lock().unwrap().remove(&key).is_some() {
                tracing::info!(peer = %peer, discovered_udp_mtu = settled, needed = need,
                    "wire MTU: jumbo verdict cleared — this path no longer proves it");
            }
        }
        if settled >= ceiling {
            if learned().lock().unwrap().remove(&peer).is_some() {
                tracing::info!(peer = %peer,
                    "wire MTU: path re-measured at full size — learned clamp cleared");
            }
            // `ceiling` is the 1500-byte video size. A jumbo start on a path
            // that no longer carries it is dropping every video datagram.
            // Verdict is already erased; re-key if this session can — PyroWave
            // cannot (`Welcome` parse window), so those wait for next connect.
            if sealed_datagram_bytes(current) > settled as usize {
                tracing::warn!(
                    peer = %peer,
                    discovered_udp_mtu = settled,
                    shard_payload = current,
                    "wire MTU: this session started at a JUMBO shard but the path does not \
                     carry it — video datagrams are oversized for a hop, which streams as a \
                     black screen with zero reported loss. The jumbo verdict for this path is \
                     cleared: the next connect starts at the standard 1500-byte wire."
                );
                if let Some(r) = reneg.as_ref() {
                    let back = shard_payload_for_udp_budget(settled as usize, peer);
                    if back < current
                        && r.change_tx.send(back as u16).is_ok()
                        && r.apply_tx.send(back).is_ok()
                    {
                        tracing::info!(peer = %peer, shard_payload = back, was = current,
                            "wire MTU: video re-keyed mid-session back to the standard wire");
                        current = back;
                    }
                }
            }
        } else {
            // A closed connection stops discovering. Learn only from one that
            // stayed alive through the whole window — a high-RTT path can
            // still be mid-search here.
            if conn.close_reason().is_some() {
                return;
            }
            learned().lock().unwrap().insert(peer, settled);
            if sealed_datagram_bytes(current) <= settled as usize {
                // Path still constrained; this session already fits, so no WARN.
                tracing::info!(peer = %peer, discovered_udp_mtu = settled,
                    "wire MTU: constrained path re-measured; this session's video is sized to fit");
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
                // Shrink this session now: smaller always fits, so ordering
                // does not matter and the ack is telemetry. The learned
                // record still starts session 2 at the right size.
                if let Some(r) = reneg.as_ref() {
                    let target = shard_payload_for_udp_budget(settled as usize, peer);
                    if target < current
                        && r.change_tx.send(target as u16).is_ok()
                        && r.apply_tx.send(target).is_ok()
                    {
                        tracing::info!(
                            peer = %peer,
                            shard_payload = target,
                            was = current,
                            "wire MTU: video re-keyed mid-session to fit the constrained path \
                             — the stream heals now instead of on the next connect"
                        );
                        current = target;
                    }
                }
            }
        }
        // Revert guard for a jumbo start with no re-key channel (PyroWave).
        // This session cannot recover; drop the verdict the moment quinn's
        // blackhole detection says the path shrank. One sample per 5 s, and
        // only while the session is still fitting.
        if current > mtu1500_shard_payload_for(peer)
            && reneg.is_none()
            && sealed_datagram_bytes(current) <= settled as usize
        {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if conn.close_reason().is_some() {
                    return;
                }
                let mtu_now = conn.current_mtu();
                if (mtu_now as usize) < sealed_datagram_bytes(current) {
                    jumbo_verdicts().lock().unwrap().remove(&path_key(&conn));
                    tracing::warn!(peer = %peer, discovered_udp_mtu = mtu_now,
                        shard_payload = current,
                        "wire MTU: the jumbo path this session started on stopped fitting — this \
                         session cannot be re-keyed (chunk-aligned client parse window), so it \
                         will not recover, but the verdict is cleared and the next connect \
                         starts at the standard wire");
                    return;
                }
            }
        }
        // Jumbo grow: operator opt-in, client headroom, settled-at-jumbo
        // proof. Ack-gated — no sealed datagram above the old size leaves
        // before the client's ack, even though its buffers are static.
        let Some(r) = reneg.as_mut() else {
            return;
        };
        let grow = target_wire_mtu.map(|mtu| {
            let target = jumbo_shard_payload_for(mtu, peer).min(r.client_ceiling as usize);
            (mtu, target - target % 2)
        });
        match grow {
            Some((mtu, target))
                if target > current && (settled as usize) >= sealed_datagram_bytes(target) =>
            {
                if r.change_tx.send(target as u16).is_err() {
                    return;
                }
                let acked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while let Some(v) = r.ack_rx.recv().await {
                        if v as usize == target {
                            return true;
                        }
                    }
                    false
                })
                .await
                .unwrap_or(false);
                if !acked {
                    tracing::warn!(peer = %peer, shard_payload = target,
                        "wire MTU: jumbo grow not acked — staying at the current wire");
                    return;
                }
                if r.apply_tx.send(target).is_err() {
                    return;
                }
                tracing::info!(
                    peer = %peer,
                    shard_payload = target,
                    was = current,
                    wire_mtu = mtu,
                    "wire MTU: jumbo grow acked and applied — packets-per-frame cut ~6×"
                );
                current = target;
            }
            // A jumbo start that can be re-keyed needs the same revert as a grow.
            _ if current > mtu1500_shard_payload_for(peer) => {}
            _ => return,
        }
        // Revert: quinn's PMTU blackhole detection lowers `current_mtu`
        // when the big packets vanish. Sample and shrink via the same
        // path the down-leg uses.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if conn.close_reason().is_some() {
                return;
            }
            let mtu_now = conn.current_mtu();
            if (mtu_now as usize) < sealed_datagram_bytes(current) {
                let back = shard_payload_for_udp_budget(mtu_now as usize, peer);
                tracing::warn!(peer = %peer, discovered_udp_mtu = mtu_now,
                    shard_payload = back, was = current,
                    "wire MTU: jumbo path stopped fitting — reverting the wire to match");
                if r.change_tx.send(back as u16).is_err() || r.apply_tx.send(back).is_err() {
                    return;
                }
                current = back;
            }
        }
    });
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
        // 1280 wire − 28 IP/UDP − 64 header/crypto = 1188.
        assert_eq!(resolve(Some(1280), Some(1472), NO_JUMBO, V4), 1188);
    }

    #[test]
    fn learned_budget_clamps() {
        // 1280-byte UDP budget − 64 header/crypto = 1216.
        assert_eq!(resolve(None, Some(1280), NO_JUMBO, V4), 1216);
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

    /// 9000 − 28 (IPv4) − 64 = 8908; 9000 − 48 − 64 = 8888.
    #[test]
    fn proven_and_reproven_path_starts_jumbo() {
        assert_eq!(jumbo_session_start(proven_jumbo(), V4), Some(8908));
        let mut v6 = proven_jumbo();
        v6.live_udp_mtu = 8952;
        v6.proven_udp_budget = Some(8952);
        assert_eq!(jumbo_session_start(v6, V6), Some(8888));
        // `resolve` must pick jumbo ahead of the env branch, which would
        // clamp a >1500 `PUNKTFUNK_WIRE_MTU` to the family default.
        assert_eq!(resolve(Some(9000), None, proven_jumbo(), V4), 8908);
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
        assert_eq!(jumbo_session_start(roomy, V4), Some(8908));
    }

    #[test]
    fn without_the_operator_opt_in_nothing_grows() {
        let mut no_optin = proven_jumbo();
        no_optin.target_wire_mtu = None;
        assert_eq!(jumbo_session_start(no_optin, V4), None);
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
                let Some(t) = jumbo_target(Some(mtu), u16::MAX, peer) else {
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
        assert_eq!(jumbo_target(Some(1500), u16::MAX, V4), None);
        assert_eq!(jumbo_target(None, u16::MAX, V4), None);
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
}
