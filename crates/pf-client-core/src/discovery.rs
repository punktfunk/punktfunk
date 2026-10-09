//! mDNS browse of `_punktfunk._udp` (TXT `fp`/`pair`/`id`; host crate
//! `discovery.rs`). A worker streams [`DiscoveryEvent`]; it exits when the
//! receiver is dropped, polled so an empty LAN still stops.

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// DNS-SD type hosts advertise. See host crate `punktfunk_host::hostsys::discovery`.
const SERVICE_TYPE: &str = "_punktfunk._udp.local.";

pub use punktfunk_core::discovery::DiscoveredHost;

/// Is this advert that saved host? Two known fingerprints settle it on their own — falling
/// back to the address there would let whoever inherits a sleeping host's DHCP lease be
/// treated AS that host, and reads the other OS of a dual-boot box (one lease, one MAC, a
/// certificate each) as the OS already saved, so it never reaches the discovered shelf.
pub fn same_host(k: &crate::trust::KnownHost, d: &DiscoveredHost) -> bool {
    if !k.fp_hex.is_empty() && !d.fp_hex.is_empty() {
        return k.fp_hex.eq_ignore_ascii_case(&d.fp_hex);
    }
    k.addr == d.addr && k.port == d.port
}

pub enum DiscoveryEvent {
    /// Appeared or refreshed (new address, pairing, …).
    Resolved(DiscoveredHost),
    Removed {
        fullname: String,
    },
}

/// Cheap-to-clone flag: force an immediate re-query. A request after the
/// browse ends is never read.
///
/// `mdns-sd` re-queries on a doubling backoff (1s, 2s, 4s … cap 1h), so a
/// long-lived browse is passive. Re-querying resets that clock.
#[derive(Clone, Debug)]
pub struct Rescan(Arc<AtomicBool>);

impl Rescan {
    /// Set the flag; the query follows within a tick. Coalesces: many requests, one query.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A browse watched for one host while it wakes. It owns the one advert match rule the
/// wake-and-wait loops share, and their rescan cadence.
pub struct AdvertWatch {
    rx: async_channel::Receiver<DiscoveryEvent>,
    rescan: Rescan,
    polls: u32,
}

impl AdvertWatch {
    pub fn start() -> AdvertWatch {
        let (rx, rescan) = browse();
        AdvertWatch {
            rx,
            rescan,
            polls: 0,
        }
    }

    /// Where the host advertised from since the last poll, if it did. A pinned host
    /// (`fp` non-empty) is matched by its pin alone, an unpinned one by `addr:port`.
    ///
    /// Every fifth poll re-queries: `mdns-sd`'s backoff has doubled past a minute by the
    /// time a cold box finishes booting.
    pub fn poll(&mut self, fp: Option<&str>, addr: &str, port: u16) -> Option<(String, u16)> {
        let mut seen = None;
        while let Ok(ev) = self.rx.try_recv() {
            let DiscoveryEvent::Resolved(h) = ev else {
                continue;
            };
            let matched = match fp.filter(|f| !f.is_empty()) {
                Some(fp) => h.fp_hex == fp,
                None => h.addr == addr && h.port == port,
            };
            if matched {
                seen = Some((h.addr, h.port));
            }
        }
        self.polls += 1;
        if self.polls % 5 == 0 {
            self.rescan.request();
        }
        seen
    }
}

/// Continuous browse plus [`Rescan`]. A browse that does not start yields a closed receiver.
pub fn browse() -> (async_channel::Receiver<DiscoveryEvent>, Rescan) {
    try_browse().unwrap_or_else(|| (async_channel::unbounded().1, Rescan(Arc::default())))
}

/// [`browse`], or `None` when the daemon, its browse or its worker does not start. The
/// worker exits when the receiver is dropped or the daemon dies — polled on a 250 ms tick,
/// so an empty LAN still stops — and shuts the daemon down on its way out.
pub fn try_browse() -> Option<(async_channel::Receiver<DiscoveryEvent>, Rescan)> {
    let daemon = ServiceDaemon::new()
        .inspect_err(|e| tracing::warn!(error = %e, "mDNS daemon failed — discovery disabled"))
        .ok()?;
    let mut receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "mDNS browse failed — discovery disabled");
            let _ = daemon.shutdown();
            return None;
        }
    };
    let (tx, rx) = async_channel::unbounded();
    let flag = Arc::new(AtomicBool::new(false));
    let requested = flag.clone();
    let worker = daemon.clone();
    let spawned = std::thread::Builder::new()
        .name("punktfunk-mdns".into())
        .spawn(move || {
            // What the last SearchStarted named: the one line in a bug report that says
            // whether the browse has any interface at all.
            let mut announced = String::new();
            // Poll, do not `recv()`: no adverts is the empty-LAN case and
            // ignored events never touch `tx`. A blocking recv would leak this
            // thread, the daemon, and :5353 on every `discover_for` call.
            loop {
                // Before the `continue` arms that never send (ignored events,
                // no IPv4). Those would otherwise keep the thread with no consumer.
                if tx.is_closed() {
                    break;
                }
                // Same placement: every `continue` below would skip the swap.
                if requested.swap(false, Ordering::Relaxed) {
                    // Re-browse REPLACES the listener: replays the cache, puts a
                    // PTR on the wire now, and resets the `Rescan` backoff.
                    match worker.browse(SERVICE_TYPE) {
                        Ok(r) => receiver = r,
                        Err(e) => tracing::warn!(error = %e, "mDNS rescan failed"),
                    }
                }
                let event = match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => event,
                    Err(_) if receiver.is_disconnected() => break,
                    Err(_) => continue,
                };
                let update = match event {
                    ServiceEvent::SearchStarted(what) => {
                        if what != announced {
                            tracing::info!("mDNS browse on {what}");
                            announced = what;
                        }
                        continue;
                    }
                    ServiceEvent::ServiceResolved(info) => {
                        let props = info.get_properties();
                        let v4: Vec<std::net::Ipv4Addr> =
                            info.get_addresses_v4().into_iter().collect();
                        let Some(host) = punktfunk_core::discovery::advert_from_txt(
                            info.get_fullname(),
                            info.get_port(),
                            &v4,
                            |k| props.get_property_val_str(k),
                        ) else {
                            continue;
                        };
                        DiscoveryEvent::Resolved(host)
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        DiscoveryEvent::Removed { fullname }
                    }
                    _ => continue,
                };
                if tx.send_blocking(update).is_err() {
                    break;
                }
            }
            let _ = worker.shutdown();
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "mDNS worker did not start — discovery disabled");
        let _ = daemon.shutdown();
        return None;
    }
    Some((rx, Rescan(flag)))
}

/// Folded advert map, keyed by [`DiscoveredHost::key`]: one entry per host, however many
/// instance names it advertises under.
pub type Adverts = BTreeMap<String, DiscoveredHost>;

/// Refresh wins (newer address). Removal drops by mDNS fullname, not `key`.
pub fn fold(adverts: &mut Adverts, event: DiscoveryEvent) {
    match event {
        DiscoveryEvent::Resolved(host) => {
            adverts.insert(host.key.clone(), host);
        }
        DiscoveryEvent::Removed { fullname } => {
            adverts.retain(|_, h| h.fullname != fullname);
        }
    }
}

/// Blocking one-shot: browse `timeout`, return deduped-by-`key`, address-sorted.
/// Live UIs want [`browse`]; this is for CLI `discover` and plugin backends.
pub fn discover_for(timeout: Duration) -> Vec<DiscoveredHost> {
    let (rx, _rescan) = browse();
    let deadline = Instant::now() + timeout;
    let mut adverts = Adverts::new();
    while Instant::now() < deadline {
        while let Ok(event) = rx.try_recv() {
            fold(&mut adverts, event);
        }
        // Tick, not blocking recv: `async_channel` has no timeout and this call is bounded.
        std::thread::sleep(Duration::from_millis(50).min(timeout));
    }
    while let Ok(event) = rx.try_recv() {
        fold(&mut adverts, event);
    }
    // Dropping `rx` is what stops the worker (it polls `tx.is_closed()`).
    // Without this, a one-shot leaks a browse per call even on an empty LAN.
    drop(rx);
    sorted(adverts)
}

/// Address then port. IPv4 numeric: lexical sort puts `.10` before `.9`.
fn sorted(adverts: Adverts) -> Vec<DiscoveredHost> {
    let mut hosts: Vec<DiscoveredHost> = adverts.into_values().collect();
    hosts.sort_by_key(|h| {
        (
            h.addr.parse::<std::net::Ipv4Addr>().ok().map(u32::from),
            h.addr.clone(),
            h.port,
        )
    });
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str, fullname: &str, addr: &str) -> DiscoveredHost {
        DiscoveredHost {
            key: key.into(),
            fullname: fullname.into(),
            name: fullname.split('.').next().unwrap_or("?").into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: "aa".into(),
            pair: "required".into(),
            mgmt_port: Some(47990),
            mac: vec![],
            os: String::new(),
            wire: Vec::new(),
        }
    }

    /// A dual-boot box: one lease, one MAC, a certificate per OS. The advert of the OS
    /// that is up is not the saved record of the other one, so it still reaches the
    /// discovered shelf and can be added.
    #[test]
    fn a_second_os_at_one_address_is_not_the_saved_host() {
        let saved = crate::trust::KnownHost {
            name: "Desk (Windows)".into(),
            addr: "192.168.1.9".into(),
            port: 9777,
            fp_hex: "bb".into(),
            ..Default::default()
        };
        let other_os = host("id-2", "desk-linux._punktfunk._udp.local.", "192.168.1.9");
        assert!(
            !same_host(&saved, &other_os),
            "a different pin is a different host"
        );

        // The same host, on a lease it has moved to, still matches on its pin alone.
        let mut moved = other_os.clone();
        moved.fp_hex = "BB".into();
        moved.addr = "192.168.1.20".into();
        assert!(same_host(&saved, &moved));

        // Neither side pinned: the address is all there is to go on.
        let placeholder = crate::trust::KnownHost {
            addr: "192.168.1.9".into(),
            port: 9777,
            ..Default::default()
        };
        let mut unpinned = other_os.clone();
        unpinned.fp_hex = String::new();
        assert!(same_host(&placeholder, &unpinned));
    }

    /// A woken host's advert counts only if it is that host: a pinned target is matched by
    /// its pin wherever it came back, never by an fp-less advert at its old address.
    #[test]
    fn a_pinned_wake_target_is_matched_by_its_pin_alone() {
        let (tx, rx) = async_channel::unbounded();
        let mut w = AdvertWatch {
            rx,
            rescan: Rescan(Arc::default()),
            polls: 0,
        };
        let mut fp_less = host("id-1", "desk._punktfunk._udp.local.", "192.168.1.9");
        fp_less.fp_hex = String::new();
        let mut moved = host("id-2", "desk._punktfunk._udp.local.", "192.168.1.20");
        moved.fp_hex = "bb".into();

        tx.try_send(DiscoveryEvent::Resolved(fp_less.clone()))
            .unwrap();
        assert_eq!(w.poll(Some("bb"), "192.168.1.9", 9777), None);
        tx.try_send(DiscoveryEvent::Resolved(moved)).unwrap();
        assert_eq!(
            w.poll(Some("bb"), "192.168.1.9", 9777),
            Some(("192.168.1.20".into(), 9777))
        );
        // A card saved without a pin has only its address to go on.
        tx.try_send(DiscoveryEvent::Resolved(fp_less)).unwrap();
        assert_eq!(
            w.poll(Some(""), "192.168.1.9", 9777),
            Some(("192.168.1.9".into(), 9777))
        );
        assert!(!w.rescan.0.load(Ordering::Relaxed));
        (0..2).for_each(|_| drop(w.poll(None, "192.168.1.9", 9777)));
        assert!(
            w.rescan.0.load(Ordering::Relaxed),
            "the fifth poll re-queries"
        );
    }

    #[test]
    fn refreshed_advert_supersedes_the_earlier_one() {
        let mut adverts = Adverts::new();
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.9")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.20")),
        );
        // The same host under a second instance name (an mDNS conflict suffix) that sorts first.
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host(
                "id-1",
                "desk (2)._punktfunk._udp.local.",
                "192.168.1.30",
            )),
        );
        let out = sorted(adverts);
        assert_eq!(out.len(), 1, "same key must not render twice");
        assert_eq!(out[0].addr, "192.168.1.30", "the newest advert wins");
    }

    #[test]
    fn removal_drops_the_advert_it_names() {
        let mut adverts = Adverts::new();
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.9")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-2", "tv._punktfunk._udp.local.", "192.168.1.10")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Removed {
                fullname: "desk._punktfunk._udp.local.".into(),
            },
        );
        let out = sorted(adverts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "id-2");
    }

    /// No `id` TXT → keyed by fullname; `advertised_id` must still be `""`,
    /// or a launch would target a nonexistent reference.
    #[test]
    fn advertised_id_is_empty_without_the_txt() {
        let named = host("id-1", "desk._punktfunk._udp.local.", "10.0.0.1");
        assert_eq!(named.advertised_id(), "id-1");
        let anonymous = host(
            "desk._punktfunk._udp.local.",
            "desk._punktfunk._udp.local.",
            "10.0.0.1",
        );
        assert_eq!(anonymous.advertised_id(), "");
    }

    #[test]
    fn addresses_sort_numerically() {
        let mut adverts = Adverts::new();
        for (i, addr) in ["192.168.1.20", "192.168.1.9", "192.168.1.100"]
            .into_iter()
            .enumerate()
        {
            fold(
                &mut adverts,
                DiscoveryEvent::Resolved(host(&format!("id-{i}"), &format!("h{i}."), addr)),
            );
        }
        let out = sorted(adverts);
        let addrs: Vec<&str> = out.iter().map(|h| h.addr.as_str()).collect();
        assert_eq!(addrs, ["192.168.1.9", "192.168.1.20", "192.168.1.100"]);
    }
}
