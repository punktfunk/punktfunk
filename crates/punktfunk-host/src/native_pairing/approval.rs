//! Pending-knock queue and delegated operator approval.
//!
//! Owns the pending-knock [`Mutex`] and the change [`Notify`] that wakes a QUIC
//! connection parked in [`ApprovalQueue::wait_for_decision`] when an operator
//! acts.
//!
//! Blind to the trust store: whether a fingerprint is paired is injected into
//! [`ApprovalQueue::wait_for_decision`] as an `is_paired` closure. The facade
//! persists the pairing; this queue records the admitted knock generation and
//! clears the entry ([`ApprovalQueue::admit_and_clear`]).

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Unpaired identified knock held for console approval.
/// In-memory only: a restart drops it and the device knocks again; entries
/// expire after [`PENDING_TTL`].
struct Pending {
    id: u32,
    name: String,
    fp_hex: String,
    requested_at: Instant,
    /// QUIC-validated knock source, used for the per-source cap. `None` if unknown.
    src_ip: Option<IpAddr>,
    /// [`classify_source`] of `src_ip`, taken when the knock arrived.
    source: KnockSource,
    /// True while [`ApprovalQueue::wait_for_decision`] holds this knock open.
    /// Eviction skips a parked entry unless every candidate is parked.
    parked: bool,
    /// Generation of the most recent knock for this fingerprint. A re-knock
    /// bumps it so a stale parked waiter resolves [`PairingDecision::Superseded`]
    /// — one Approve admits exactly one session.
    knock_seq: u32,
    /// The profile the knock asked for, as sent. Untrusted.
    profile: Option<String>,
}

#[derive(Default)]
struct PendingState {
    next_id: u32,
    items: Vec<Pending>,
    /// Fingerprint → admitted knock generation, kept after
    /// [`ApprovalQueue::admit_and_clear`] clears the pending entry. A superseded
    /// waiter that polls after the entry is gone uses this to resolve `Superseded`
    /// instead of a second `Approved`. Pruned on the pending TTL.
    admitted: Vec<(String, u32, Instant)>,
}

/// Where a knock came from. The host cannot tell a stranger on the internet from a friend on
/// the couch by name — the name is whatever the device sent — so admission rules hang off the
/// address instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnockSource {
    /// Loopback, RFC 1918, link-local, IPv6 unique-local, or a Tailscale tailnet. Reaching any
    /// of these means the peer is already on a network the operator let it onto.
    Lan,
    /// Routable from the internet, or an address we could not read. Fails closed: every rule
    /// that treats a source as untrusted must land here rather than on a guess.
    Wan,
}

/// Classify a knock's source. `None` is [`KnockSource::Wan`] — an admission rule reading an
/// unknown address must refuse, not admit.
pub fn classify_source(ip: Option<IpAddr>) -> KnockSource {
    classify_with(ip, routes_over_tailnet)
}

/// [`classify_source`] with the tailnet test injected. `over_tailnet` is asked only about a
/// 100.64/10 peer: that range is carrier-grade NAT space too, so the address alone proves nothing.
pub(super) fn classify_with(
    ip: Option<IpAddr>,
    over_tailnet: impl Fn(Ipv4Addr) -> bool,
) -> KnockSource {
    let Some(ip) = ip else {
        return KnockSource::Wan;
    };
    let v4 = match ip {
        IpAddr::V4(v4) => Some(v4),
        // A v4-mapped v6 source is the v4 address wearing a hat; classify what it really is.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // Loopback (::1), link-local fe80::/10, unique local fc00::/7.
            if v6.is_loopback() || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xfe00) == 0xfc00 {
                return KnockSource::Lan;
            }
            v6.to_ipv4_mapped()
        }
    };
    match v4 {
        Some(v4)
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (is_cgnat(v4) && over_tailnet(v4)) =>
        {
            KnockSource::Lan
        }
        _ => KnockSource::Wan,
    }
}

/// RFC 6598 shared space, 100.64/10: Tailscale's addresses and a CGNAT ISP's subscribers alike.
fn is_cgnat(v4: Ipv4Addr) -> bool {
    v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])
}

/// The host routes its replies to `peer` out of its Tailscale interface. A handshake needs those
/// replies, so a CGNAT neighbour that borrows a tailnet address never completes one. A UDP
/// `connect` looks the route up without sending; any failure reads as not the tailnet.
fn routes_over_tailnet(peer: Ipv4Addr) -> bool {
    let Ok(src) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|s| {
        s.connect((peer, 9))?;
        s.local_addr()
    }) else {
        return false;
    };
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .any(|i| i.ip() == src.ip() && is_tailscale_iface(&i.name, i.ip()))
}

/// Tailscale's interface: `tailscale0` on Linux, `Tailscale` on Windows, a `utun` on macOS. No ISP
/// puts a 100.64/10 address on any of those names.
pub(super) fn is_tailscale_iface(name: &str, addr: IpAddr) -> bool {
    let name = name.to_ascii_lowercase();
    (name.starts_with("tailscale") || name.starts_with("utun"))
        && matches!(addr, IpAddr::V4(v4) if is_cgnat(v4))
}

/// Pending-approval snapshot for the management API.
pub struct PendingRequest {
    /// Per-process id for approve/deny; stable for this entry's lifetime.
    pub id: u32,
    /// Client `Hello` name, or fingerprint-derived if missing.
    pub name: String,
    /// Hex SHA-256 of the knocking client's certificate; approval pins this.
    pub fingerprint: String,
    pub age_secs: u64,
    /// Where the knock arrived from. A bare approve is refused for [`KnockSource::Wan`].
    pub source: KnockSource,
    /// The profile id the knock named, unchecked.
    pub profile: Option<String>,
}

/// Outcome of a `wait_for_decision` park on an unpaired knock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingDecision {
    /// Fingerprint is now paired; admit the session.
    Approved,
    /// Operator denied, or the pending entry dropped without pairing.
    Denied,
    /// Wait window elapsed; the device can knock again.
    TimedOut,
    /// A newer knock from the same fingerprint replaced this one; close this
    /// connection. Approval admits only the newest parked waiter.
    Superseded,
}

/// Drop pending knocks older than this; a stale entry must not stay approvable.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on the pending list so a LAN scanner cannot grow it unboundedly.
pub(super) const PENDING_CAP: usize = 32;
/// Max pending knocks one source IP may occupy so one host cannot fill the queue.
/// QUIC address-validates the source, so this is not off-path spoofable.
pub(super) const MAX_PENDING_PER_IP: usize = 4;
/// Ceiling on the test-only ready wait. A spawn that panics before parking must fail
/// the test, not stall the job until CI times out.
#[cfg(test)]
const WAITER_READY_LIMIT: Duration = Duration::from_secs(5);

pub(super) struct ApprovalQueue {
    pending: Mutex<PendingState>,
    /// Fired when a fingerprint is paired or a pending knock is denied/dropped.
    changed: Notify,
    #[cfg(test)]
    waiter_ready_generation: AtomicU64,
    #[cfg(test)]
    waiter_ready_changed: Notify,
}

impl ApprovalQueue {
    pub(super) fn new() -> ApprovalQueue {
        ApprovalQueue {
            pending: Mutex::new(PendingState::default()),
            changed: Notify::new(),
            #[cfg(test)]
            waiter_ready_generation: AtomicU64::new(0),
            #[cfg(test)]
            waiter_ready_changed: Notify::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn waiter_ready_generation(&self) -> u64 {
        self.waiter_ready_generation.load(Ordering::Acquire)
    }

    /// Park until a [`Self::wait_for_decision`] newer than `previous` is armed on the
    /// notifier, so a test can approve without racing the waiter. A waiter that never
    /// arms fails the test rather than hanging the job.
    #[cfg(test)]
    pub(super) async fn wait_for_waiter_ready(&self, previous: u64) {
        let armed = async {
            loop {
                let notified = self.waiter_ready_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.waiter_ready_generation.load(Ordering::Acquire) > previous {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(WAITER_READY_LIMIT, armed)
            .await
            .expect("no knock parked on the approval queue");
    }

    /// Admitted-generation markers share the pending TTL; they only matter while
    /// a superseded waiter could still be parked.
    fn expire_pending(pending: &mut PendingState) {
        pending
            .items
            .retain(|p| p.requested_at.elapsed() < PENDING_TTL);
        pending
            .admitted
            .retain(|(_, _, at)| at.elapsed() < PENDING_TTL);
    }

    /// Index of the entry to evict, optionally restricted to one source IP:
    /// least-recently-active non-parked, else oldest parked. `None` if empty.
    fn evict_index(items: &[Pending], only_ip: Option<IpAddr>) -> Option<usize> {
        let pick = |allow_parked: bool| {
            items
                .iter()
                .enumerate()
                .filter(|(_, p)| only_ip.is_none_or(|ip| p.src_ip == Some(ip)))
                .filter(|(_, p)| allow_parked || !p.parked)
                .min_by_key(|(_, p)| p.requested_at)
                .map(|(i, _)| i)
        };
        pick(false).or_else(|| pick(true))
    }

    /// Record an unpaired knock. Same fingerprint refreshes in place and bumps
    /// generation so older parked waiters resolve `Superseded`. Bounded by
    /// [`MAX_PENDING_PER_IP`] then [`PENDING_CAP`]; the name is untrusted.
    pub(super) fn note_pending(
        &self,
        name: &str,
        fp_hex: &str,
        src_ip: Option<IpAddr>,
        profile: Option<&str>,
    ) -> u32 {
        let name = super::sanitize_device_name(name, fp_hex);
        let source = classify_source(src_ip);
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        if let Some(p) = pending
            .items
            .iter_mut()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
        {
            p.requested_at = Instant::now();
            p.name = name;
            // This knock's address, not the first one's. A device that knocked from the couch and
            // came back from the internet inside the TTL would otherwise still read as LAN, and a
            // bare approve would admit it.
            p.src_ip = src_ip;
            p.source = source;
            p.profile = profile.map(str::to_string);
            p.knock_seq = p.knock_seq.wrapping_add(1);
            let seq = p.knock_seq;
            drop(pending);
            // Wake the previous parked waiter so it sees Superseded now, not at timeout.
            self.changed.notify_waiters();
            return seq;
        }
        // Drop a leftover admitted-generation marker from a prior pair→unpair of this fp.
        pending
            .admitted
            .retain(|(fp, _, _)| !fp.eq_ignore_ascii_case(fp_hex));
        if let Some(ip) = src_ip {
            if pending
                .items
                .iter()
                .filter(|p| p.src_ip == Some(ip))
                .count()
                >= MAX_PENDING_PER_IP
            {
                if let Some(i) = Self::evict_index(&pending.items, Some(ip)) {
                    pending.items.remove(i);
                }
            }
        }
        // Vec order is not recency after in-place refreshes; pick explicitly.
        if pending.items.len() >= PENDING_CAP {
            if let Some(i) = Self::evict_index(&pending.items, None) {
                pending.items.remove(i);
            }
        }
        let id = pending.next_id;
        pending.next_id = pending.next_id.wrapping_add(1);
        pending.items.push(Pending {
            id,
            name,
            fp_hex: fp_hex.to_string(),
            requested_at: Instant::now(),
            src_ip,
            source,
            parked: false,
            knock_seq: 0,
            profile: profile.map(str::to_string),
        });
        0
    }

    /// Gated on `knock_seq` so a superseded waiter's Drop cannot unmark the newer waiter.
    pub(super) fn set_parked(&self, fp_hex: &str, knock_seq: u32, parked: bool) {
        let mut pending = self.pending.lock().unwrap();
        if let Some(p) = pending
            .items
            .iter_mut()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex) && p.knock_seq == knock_seq)
        {
            p.parked = parked;
        }
    }

    fn knock_seq_of(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .items
            .iter()
            .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
            .map(|p| p.knock_seq)
    }

    fn admitted_seq(&self, fp_hex: &str) -> Option<u32> {
        let pending = self.pending.lock().unwrap();
        pending
            .admitted
            .iter()
            .find(|(fp, _, _)| fp.eq_ignore_ascii_case(fp_hex))
            .map(|(_, seq, _)| *seq)
    }

    pub(super) fn pending(&self) -> Vec<PendingRequest> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .map(|p| PendingRequest {
                id: p.id,
                name: p.name.clone(),
                fingerprint: p.fp_hex.clone(),
                age_secs: p.requested_at.elapsed().as_secs(),
                source: p.source,
                profile: p.profile.clone(),
            })
            .collect()
    }

    /// Expires stale entries first, so this also reports a parked knock as live.
    pub(super) fn pending_contains(&self, fp_hex: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .any(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
    }

    /// Where pending `id` knocked from, or `None` if the entry is gone. Read before
    /// admitting: a bare approve must refuse a knock from the internet.
    pub(super) fn source_of(&self, id: u32) -> Option<KnockSource> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending.items.iter().find(|p| p.id == id).map(|p| p.source)
    }

    /// `(name, fingerprint)` of pending `id` without removing it. `None` if missing
    /// or expired. The facade must pair in the trust store before
    /// [`Self::admit_and_clear`]; removing here first would let a waiter observe
    /// "neither pending nor paired" and treat approval as denial.
    pub(super) fn read_entry(&self, id: u32) -> Option<(String, String)> {
        let mut pending = self.pending.lock().unwrap();
        Self::expire_pending(&mut pending);
        pending
            .items
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.name.clone(), p.fp_hex.clone()))
    }

    /// Record which knock generation this pairing admits, clear the pending entry,
    /// then wake waiters. The caller must have pinned the fingerprint in the
    /// trust store first so a woken waiter sees paired=true and no longer pending.
    pub(super) fn admit_and_clear(&self, fp_hex: &str) {
        {
            let mut pending = self.pending.lock().unwrap();
            let admitted_seq = pending
                .items
                .iter()
                .find(|p| p.fp_hex.eq_ignore_ascii_case(fp_hex))
                .map(|p| p.knock_seq);
            if let Some(seq) = admitted_seq {
                pending
                    .admitted
                    .retain(|(fp, _, _)| !fp.eq_ignore_ascii_case(fp_hex));
                pending
                    .admitted
                    .push((fp_hex.to_string(), seq, Instant::now()));
            }
            pending
                .items
                .retain(|p| !p.fp_hex.eq_ignore_ascii_case(fp_hex));
        }
        // After pin + pending-clear so a waiter observes the settled state.
        self.changed.notify_waiters();
    }

    /// Drop a pending knock. The next knock re-creates an entry — not a blocklist.
    pub(super) fn deny_pending(&self, id: u32) -> bool {
        let removed = {
            let mut pending = self.pending.lock().unwrap();
            let before = pending.items.len();
            pending.items.retain(|p| p.id != id);
            pending.items.len() != before
        };
        if removed {
            // Wake a parked waiter so it returns Denied now, not at timeout.
            self.changed.notify_waiters();
        }
        removed
    }

    /// Park until an operator decides on `fp_hex`, up to `timeout`.
    ///
    /// `knock_seq` is the generation [`Self::note_pending`] returned for this
    /// connection. `is_paired` is injected by the facade. Holds no lock across
    /// the await.
    pub(super) async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
        is_paired: impl Fn(&str) -> bool,
    ) -> PairingDecision {
        self.set_parked(fp_hex, knock_seq, true);
        struct ParkGuard<'a> {
            q: &'a ApprovalQueue,
            fp: &'a str,
            seq: u32,
        }
        impl Drop for ParkGuard<'_> {
            fn drop(&mut self) {
                self.q.set_parked(self.fp, self.seq, false);
            }
        }
        let _park = ParkGuard {
            q: self,
            fp: fp_hex,
            seq: knock_seq,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Arm and enable before re-reading state so an approve/deny between
            // the check and the await cannot be lost.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            #[cfg(test)]
            {
                self.waiter_ready_generation.fetch_add(1, Ordering::Release);
                self.waiter_ready_changed.notify_waiters();
            }

            // Once a newer knock owns the fingerprint this connection must never
            // be admitted, even if approval lands before we wake.
            match self.knock_seq_of(fp_hex) {
                Some(cur) if cur != knock_seq => return PairingDecision::Superseded,
                _ => {}
            }
            if is_paired(fp_hex) {
                // Tie-break on the admitted marker: a superseded waiter that first
                // polls after pairing sees the same paired/no-entry state as the winner.
                match self.admitted_seq(fp_hex) {
                    Some(adm) if adm != knock_seq => return PairingDecision::Superseded,
                    _ => return PairingDecision::Approved,
                }
            }
            if !self.pending_contains(fp_hex) {
                // Cleared-pending can be a denial or the facade's pin-then-clear
                // gap. Re-check is_paired; the facade pins before it clears.
                if is_paired(fp_hex) {
                    match self.admitted_seq(fp_hex) {
                        Some(adm) if adm != knock_seq => return PairingDecision::Superseded,
                        _ => return PairingDecision::Approved,
                    }
                }
                return PairingDecision::Denied;
            }

            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return PairingDecision::TimedOut,
            }
        }
    }
}
