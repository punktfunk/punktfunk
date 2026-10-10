//! The shared-path governor: sessions from one client address divide what the path
//! carries, read across the registry under its lock.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use super::{registry, LiveSession};

/// Report age after which a session no longer contributes to a shared path.
/// Four 750 ms windows leave one discarded report fresh; a legacy client's
/// startup-only sample expires.
const DELIVERY_STALE: std::time::Duration = std::time::Duration::from_secs(3);

/// A sibling silent this long has left the path as far as the governor can
/// tell, and the survivor is handed it. Shorter would read a client's own
/// stall as a departure.
const GROUP_DISSOLVE: std::time::Duration = std::time::Duration::from_secs(12);

/// What the shared-path governor reads across the sessions of one client
/// address, published by each session's control task on its own report cadence.
///
/// Here rather than in the control task because the governor divides a path
/// between sessions, and no task can see its siblings' locals. Relaxed atomics
/// carry policy inputs; the report timestamp has its own mutex.
#[derive(Default)]
pub struct AbrShare {
    /// Automatic bitrate, so a share may move it. A fixed rate and a PyroWave
    /// pin are never touched.
    automatic: AtomicBool,
    /// Wire rate the host put out for this session over the last window.
    offered_kbps: AtomicU32,
    /// What the client's last report window came to. `0` = none yet.
    delivered_kbps: AtomicU32,
    /// The session was already streaming when that window opened, so the two
    /// rates above are a reading of the path (`governor::Member::streaming`).
    streaming: AtomicBool,
    /// Non-zero delivery samples seen on the report cadence. Two distinguish a
    /// current client from the legacy one-shot startup report.
    delivery_samples: AtomicU32,
    /// Arrival time of the last report. A wedged or legacy control plane must
    /// not leave a permanent path reading.
    last_delivery: Mutex<Option<std::time::Instant>>,
    /// The share this session was last told (`0` = none), the most its group
    /// has been seen to carry between them, and whether it had a group at all.
    share_kbps: AtomicU32,
    path_kbps: AtomicU32,
    grouped: AtomicBool,
}

impl AbrShare {
    /// This session's own view, as its control task takes it.
    pub fn publish(
        &self,
        now: std::time::Instant,
        automatic: bool,
        offered_kbps: u32,
        delivered_kbps: u32,
        streaming: bool,
    ) {
        let mut last = self.last_delivery.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|at| now.saturating_duration_since(at) > DELIVERY_STALE) {
            self.delivery_samples.store(0, Ordering::Relaxed);
        }
        *last = Some(now);
        if delivered_kbps > 0 {
            self.delivery_samples.fetch_add(1, Ordering::Relaxed);
        }
        self.automatic.store(automatic, Ordering::Relaxed);
        self.offered_kbps.store(offered_kbps, Ordering::Relaxed);
        self.delivered_kbps.store(delivered_kbps, Ordering::Relaxed);
        self.streaming.store(streaming, Ordering::Relaxed);
    }

    /// Whether repeated, recent reports make this a current path member.
    fn delivery_ready(&self, now: std::time::Instant) -> bool {
        self.delivery_samples.load(Ordering::Relaxed) >= 2
            && self
                .last_delivery
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|at| now.saturating_duration_since(at) <= DELIVERY_STALE)
    }

    /// Whether the last report is older than `after`, or never came.
    fn silent_for(&self, now: std::time::Instant, after: std::time::Duration) -> bool {
        self.last_delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|at| now.saturating_duration_since(at) >= after)
    }

    /// The ceiling this session is running under. `0` = none.
    pub fn share_kbps(&self) -> u32 {
        self.share_kbps.load(Ordering::Relaxed)
    }

    fn note_share(&self, kbps: u32) {
        self.share_kbps.store(kbps, Ordering::Relaxed);
    }
}

/// This session's ceiling on the path it shares, if the governor has one to
/// send. `None` leaves the standing share alone.
///
/// Every member computes the whole group and applies only its own, so a share
/// is only ever sent by the task that owns the control stream it goes down.
/// Only sessions with repeated, fresh delivery reports form the group; one
/// capable reporter beside a legacy or stale client is left alone. A fixed or
/// pinned session is never told anything. A survivor is handed the path once
/// and holds no share afterwards: [`AbrShare::share_kbps`] reads `0`.
pub fn share_for(id: u64, clocks: punktfunk_core::abr::governor::Clocks) -> Option<u32> {
    share_for_at(id, std::time::Instant::now(), clocks)
}

/// Time-injected [`share_for`] for cadence and expiry tests.
fn share_for_at(
    id: u64,
    now: std::time::Instant,
    clocks: punktfunk_core::abr::governor::Clocks,
) -> Option<u32> {
    use punktfunk_core::abr::governor;
    let reg = registry();
    let me = reg.iter().find(|s| s.id == id)?;
    if !me.counters.share.automatic.load(Ordering::Relaxed)
        || !me.counters.share.delivery_ready(now)
    {
        return None;
    }
    let peer = me.peer?;
    let peers: Vec<&LiveSession> = reg.iter().filter(|s| s.peer == Some(peer)).collect();
    let group: Vec<&LiveSession> = peers
        .iter()
        .copied()
        .filter(|s| s.counters.share.delivery_ready(now))
        .collect();
    let mine = group.iter().position(|s| s.id == id)?;
    let share = &me.counters.share;
    if group.len() < 2 {
        // A sibling that stopped reporting may still be using the path: leave
        // the standing share until every one of them has been silent long
        // enough to have gone.
        let silent = peers
            .iter()
            .filter(|s| s.id != id)
            .all(|s| s.counters.share.silent_for(now, GROUP_DISSOLVE));
        if !silent {
            return None;
        }
        // Alone on the path: hand over the whole of what the group proved it
        // carried, once, as a ceiling. The wall this session measured beside
        // them was their residual, and nobody but this host knows they have gone.
        let path = share.path_kbps.swap(0, Ordering::Relaxed);
        if !share.grouped.swap(false, Ordering::Relaxed) || path == 0 {
            return None;
        }
        share.note_share(0);
        tracing::info!(
            session = id,
            peer = %peer,
            path_kbps = path,
            "adaptive bitrate: alone on this path again — all of it is this session's"
        );
        return Some(path);
    }
    share.grouped.store(true, Ordering::Relaxed);
    let members: Vec<governor::Member> = group.iter().map(|s| member(s)).collect();
    // What the path has carried for this group, kept per session because each
    // asks on its own clock: the most it has been seen to carry, until the group
    // is short of what it offers, which is the path being re-measured (L1). A
    // member yet to report leaves it unmeasured, and that is never remembered.
    let now = governor::path_kbps(&members)?;
    let path = if governor::crowded(&members) {
        share.path_kbps.store(now, Ordering::Relaxed);
        now
    } else {
        share.path_kbps.fetch_max(now, Ordering::Relaxed).max(now)
    };
    let share = governor::shares(&members, path, clocks)[mine]?;
    me.counters.share.note_share(share);
    tracing::info!(
        session = id,
        peer = %peer,
        share_kbps = share,
        rate_kbps = me.bitrate_kbps.load(Ordering::Relaxed),
        others = group.len() - 1,
        "adaptive bitrate: this session's share of a path it is not alone on"
    );
    Some(share)
}

/// One live session as the governor reads it.
fn member(s: &LiveSession) -> punktfunk_core::abr::governor::Member {
    let share = &s.counters.share;
    punktfunk_core::abr::governor::Member {
        automatic: share.automatic.load(Ordering::Relaxed),
        current_kbps: s.bitrate_kbps.load(Ordering::Relaxed),
        offered_kbps: share.offered_kbps.load(Ordering::Relaxed),
        // `0` is "no report yet": the client has not told this host anything
        // about what is arriving, which is not the same as nothing arriving.
        delivered_kbps: match share.delivered_kbps.load(Ordering::Relaxed) {
            0 => None,
            kbps => Some(kbps),
        },
        // The capturer's own verdict that the source has nothing new, so the
        // host is repeating the last picture rather than encoding motion.
        idle: s
            .capture_health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|h| h.class == "idle"),
        share_kbps: match share.share_kbps.load(Ordering::Relaxed) {
            0 => None,
            kbps => Some(kbps),
        },
        streaming: share.streaming.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_status::tests::{fake_member, registry_lock};
    use crate::session_status::{LiveSessionGuard, SessionCounters};
    use std::sync::Arc;

    /// One live session at `peer` per rate in `kbps`, registered in that order.
    fn group<const N: usize>(
        peer: &str,
        kbps: [u32; N],
    ) -> [(LiveSessionGuard, Arc<SessionCounters>, Arc<AtomicU32>); N] {
        let peer = peer.parse().unwrap();
        kbps.map(|k| fake_member("member", peer, k))
    }

    fn both_clocks() -> punktfunk_core::abr::governor::Clocks {
        punktfunk_core::abr::governor::Clocks {
            room: true,
            lift: true,
        }
    }

    /// Publish two non-zero cadence samples, which makes a member current.
    fn publish_ready(
        counters: &SessionCounters,
        now: std::time::Instant,
        automatic: bool,
        offered_kbps: u32,
        delivered_kbps: u32,
    ) -> std::time::Instant {
        counters
            .share
            .publish(now, automatic, offered_kbps, delivered_kbps, true);
        let ready = now + std::time::Duration::from_millis(750);
        counters
            .share
            .publish(ready, automatic, offered_kbps, delivered_kbps, true);
        ready
    }

    /// Two Automatic sessions from one address, each asking an 18 Mbps path
    /// for 12: each is told half of what is actually arriving.
    #[test]
    fn two_automatic_sessions_on_one_address_take_equal_shares() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (b, bc, _br)] = group("203.0.113.90", [12_000; 2]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&ac, now, true, 12_000, 9_000);
        publish_ready(&bc, now, true, 12_000, 9_000);
        assert_eq!(share_for_at(a.id, ready, both_clocks()), Some(9_000));
        assert_eq!(share_for_at(b.id, ready, both_clocks()), Some(9_000));
        assert_eq!(ac.share.share_kbps(), 9_000, "and it is remembered");
    }

    /// One startup sample does not prove a report cadence; the second fresh
    /// sample admits both members and produces the ordinary equal split.
    #[test]
    fn two_fresh_samples_are_required_before_a_group_is_governed() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (_b, bc, _br)] = group("203.0.113.96", [12_000; 2]);
        let now = std::time::Instant::now();
        for c in [&ac, &bc] {
            c.share.publish(now, true, 12_000, 9_000, true);
        }
        assert_eq!(share_for_at(a.id, now, both_clocks()), None);
        let ready = now + std::time::Duration::from_millis(750);
        for c in [&ac, &bc] {
            c.share.publish(ready, true, 12_000, 9_000, true);
        }
        assert_eq!(share_for_at(a.id, ready, both_clocks()), Some(9_000));
    }

    /// A new client beside a legacy startup-only reporter is one capable
    /// member, not a group with a permanent stale second path reading.
    #[test]
    fn a_mixed_old_and_new_pair_is_left_ungoverned() {
        let _registry = registry_lock();
        let [(_old, old_c, _or), (new, new_c, _nr)] = group("203.0.113.97", [12_000; 2]);
        let now = std::time::Instant::now();
        old_c.share.publish(now, true, 12_000, 9_000, true);
        let ready = publish_ready(&new_c, now, true, 12_000, 9_000);
        assert_eq!(share_for_at(new.id, ready, both_clocks()), None);
        assert_eq!(old_c.share.share_kbps(), 0);
        assert_eq!(new_c.share.share_kbps(), 0);
    }

    /// A sibling whose reports stop is excluded without handing its share to
    /// the current member: the stale session is still live and may consume it.
    #[test]
    fn a_stale_sibling_neither_governs_nor_triggers_a_handback() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (_b, bc, _br)] = group("203.0.113.98", [12_000; 2]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&ac, now, true, 12_000, 9_000);
        publish_ready(&bc, now, true, 12_000, 9_000);
        assert_eq!(share_for_at(a.id, ready, both_clocks()), Some(9_000));
        let stale = ready + DELIVERY_STALE + std::time::Duration::from_millis(1);
        publish_ready(
            &ac,
            stale - std::time::Duration::from_millis(750),
            true,
            12_000,
            9_000,
        );
        assert!(!bc.share.delivery_ready(stale));
        assert_eq!(share_for_at(a.id, stale, both_clocks()), None);
        assert_eq!(ac.share.share_kbps(), 9_000, "no stale-sibling handback");
    }

    /// What the control task asks the governor with: registration stamps the
    /// session's id onto the counter block its side threads already hold, and
    /// that id is what `share_for` takes. A source that never registers leaves
    /// it `0`, which the control task reads as "nothing to share with".
    #[test]
    fn registration_latches_the_id_the_governor_is_asked_for() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (b, bc, _br)] = group("203.0.113.95", [12_000; 2]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&ac, now, true, 12_000, 9_000);
        publish_ready(&bc, now, true, 12_000, 9_000);
        assert_eq!(ac.link.session_id(), a.id, "the id the link lines carry");
        assert_eq!(bc.link.session_id(), b.id);
        assert_ne!(ac.link.session_id(), 0);
        assert_eq!(
            share_for_at(ac.link.session_id(), ready, both_clocks()),
            Some(9_000)
        );
        assert_eq!(
            share_for_at(bc.link.session_id(), ready, both_clocks()),
            Some(9_000)
        );
    }

    /// A fixed-rate session takes what it is set to off the top and is never
    /// told anything; the Automatic one gets what is left.
    #[test]
    fn a_fixed_rate_session_is_never_told_a_share() {
        let _registry = registry_lock();
        let [(auto, auto_c, _ar), (fixed, fixed_c, _fr)] = group("203.0.113.91", [14_000, 8_000]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&auto_c, now, true, 14_000, 10_000);
        publish_ready(&fixed_c, now, false, 8_000, 8_000);
        assert_eq!(share_for_at(fixed.id, ready, both_clocks()), None);
        assert_eq!(fixed_c.share.share_kbps(), 0, "nothing was written either");
        assert_eq!(
            share_for_at(auto.id, ready, both_clocks()),
            Some(10_000),
            "18 Mbps arriving, less the fixed 8"
        );
    }

    /// One session is not a group, whatever it reports.
    #[test]
    fn a_session_alone_on_its_address_is_never_governed() {
        let _registry = registry_lock();
        let [(only, c, _r)] = group("203.0.113.92", [20_000]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&c, now, true, 20_000, 9_000);
        assert_eq!(share_for_at(only.id, ready, both_clocks()), None);
    }

    /// A path that has degraded since the group's best window: the moment they
    /// are short of what they offer, the group has re-measured it, and the
    /// survivor is handed what it carries now rather than what it once did.
    #[test]
    fn a_path_that_shrank_is_not_handed_over_at_its_old_figure() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (b, bc, _br)] = group("203.0.113.94", [15_000; 2]);
        let now = std::time::Instant::now();
        // Both clean at 15 Mbps: the pair has carried 30 between them.
        let ready = publish_ready(&ac, now, true, 15_000, 15_000);
        publish_ready(&bc, now, true, 15_000, 15_000);
        assert_eq!(
            share_for_at(a.id, ready, both_clocks()),
            None,
            "nothing to divide"
        );
        assert_eq!(ac.share.path_kbps.load(Ordering::Relaxed), 30_000);
        // The path halves. Both are short, so what it carried before is gone.
        let short_at = ready + std::time::Duration::from_millis(750);
        for c in [&ac, &bc] {
            c.share.publish(short_at, true, 15_000, 6_000, true);
        }
        assert_eq!(
            share_for_at(a.id, short_at, both_clocks()),
            Some(6_000),
            "half of 12"
        );
        drop(b);
        assert_eq!(
            share_for_at(a.id, short_at, both_clocks()),
            Some(12_000),
            "the path as it is now, not the 30 000 the pair once carried"
        );
    }

    /// The sibling leaves: the survivor is handed the whole of what the pair
    /// proved the path carried, because the wall it measured beside them was
    /// their residual. Once, and then never again.
    #[test]
    fn a_survivor_is_handed_the_path_its_group_proved() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (b, bc, _br)] = group("203.0.113.93", [12_000; 2]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&ac, now, true, 12_000, 9_000);
        publish_ready(&bc, now, true, 12_000, 9_000);
        assert_eq!(share_for_at(a.id, ready, both_clocks()), Some(9_000));
        drop(b);
        assert_eq!(
            share_for_at(a.id, ready, both_clocks()),
            Some(18_000),
            "all of what the two of them were carrying"
        );
        assert_eq!(ac.share.share_kbps(), 0, "as a ceiling: nothing binds it");
        assert_eq!(
            share_for_at(a.id, ready, both_clocks()),
            None,
            "and only the once"
        );
    }

    /// A fixed-rate survivor is never handed the path: nothing may move a rate
    /// the player set, or a PyroWave pin.
    #[test]
    fn a_fixed_rate_survivor_is_never_handed_the_path() {
        let _registry = registry_lock();
        let [(auto, auto_c, _ar), (fixed, fixed_c, _fr)] = group("203.0.113.89", [14_000, 8_000]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&auto_c, now, true, 14_000, 10_000);
        publish_ready(&fixed_c, now, false, 8_000, 8_000);
        assert_eq!(share_for_at(fixed.id, ready, both_clocks()), None);
        assert_eq!(share_for_at(auto.id, ready, both_clocks()), Some(10_000));
        drop(auto);
        assert_eq!(share_for_at(fixed.id, ready, both_clocks()), None);
        assert_eq!(fixed_c.share.share_kbps(), 0);
    }

    /// A sibling silent for [`GROUP_DISSOLVE`] has gone as far as the governor
    /// can tell: the survivor is handed the path, and the share it held goes.
    #[test]
    fn a_long_silent_sibling_hands_the_path_over() {
        let _registry = registry_lock();
        let [(a, ac, _ar), (_b, bc, _br)] = group("203.0.113.88", [12_000; 2]);
        let now = std::time::Instant::now();
        let ready = publish_ready(&ac, now, true, 12_000, 9_000);
        publish_ready(&bc, now, true, 12_000, 9_000);
        assert_eq!(share_for_at(a.id, ready, both_clocks()), Some(9_000));
        let gone = ready + GROUP_DISSOLVE;
        publish_ready(
            &ac,
            gone - std::time::Duration::from_millis(750),
            true,
            12_000,
            9_000,
        );
        assert_eq!(share_for_at(a.id, gone, both_clocks()), Some(18_000));
        assert_eq!(ac.share.share_kbps(), 0);
    }
}
