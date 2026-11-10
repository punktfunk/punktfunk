//! Reachability probes: whether a saved host answers as itself, and where.

use super::{hex, rekey_addr, same_pin, KnownHost};
use punktfunk_core::client::NativeClient;
use std::time::Duration;

/// How long each probe of a shell's sweep waits. Presence is that sweep and nothing else: a
/// host reached only over Tailscale or a VPN never advertises on mDNS.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
/// How often a shell sweeps; a sleeping host shows Offline within one cycle.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(12);

/// Probe several hosts in parallel — wall-clock is ~one `timeout`, not the sum. Result
/// index matches `targets`, each `(addr, port, expected_fp_hex)`.
///
/// A target carrying a fingerprint is online only when THAT host answers. An address is
/// not an identity: a stranger who inherits a sleeping host's lease answers at it, and
/// counting that as the host lights the pip and shuts the wake gate (`!online`) against
/// the machine that needs waking. An empty fingerprint is a record saved by address alone,
/// which has nothing to compare — any answer is the host it names.
pub fn probe_reachable_many(
    targets: Vec<(String, u16, String)>,
    timeout: std::time::Duration,
) -> Vec<bool> {
    let handles: Vec<_> = targets
        .into_iter()
        .map(|(addr, port, want)| {
            std::thread::spawn(move || {
                answered_as_self(&want, NativeClient::probe_identity(&addr, port, timeout))
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(false))
        .collect()
}

/// [`probe_reachable_many`] for one host: whether it answers at `addr:port` as `fp_hex`.
pub fn probe_one(addr: &str, port: u16, fp_hex: &str, timeout: std::time::Duration) -> bool {
    probe_reachable_many(vec![(addr.to_string(), port, fp_hex.to_string())], timeout)
        .first()
        .copied()
        .unwrap_or(false)
}

/// Whether a probe's answer is the host that was asked for. `answered` is the fingerprint
/// that replied, or `None` when nothing did; `want` is the record's pin, compared by
/// [`same_pin`]. A record saved by address alone has none, so any answer is the host it names.
fn answered_as_self(want: &str, answered: Option<[u8; 32]>) -> bool {
    answered.is_some_and(|fp| same_pin(want, &hex(&fp)).unwrap_or(true))
}

/// Probe every saved host where it could be, in parallel: its address, then — only when
/// that is silent — the addresses it left (`prev_addrs`), all at once. A pinned host that
/// answers at one of those moves back there, so Tailscale takes over again once the LAN is
/// gone. On a Steam Frame a pinned host's adapter address is asked beside its address and
/// wins when it answers. A host saved by address alone is asked there only. Result index
/// matches `hosts`: `true` = online.
pub fn probe_known(hosts: &[KnownHost], timeout: std::time::Duration) -> Vec<bool> {
    let on_frame = punktfunk_core::discovery::hosts_frame_adapter();
    let found: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|h| s.spawn(move || where_answers(h, on_frame, timeout)))
            .collect();
        handles
            .into_iter()
            .map(|j| j.join().ok().flatten())
            .collect()
    });
    for (h, at) in hosts.iter().zip(&found) {
        if let Some(a) = at.as_deref().filter(|a| *a != h.addr) {
            rekey_addr(&h.fp_hex, a, h.port);
        }
    }
    found.iter().map(Option::is_some).collect()
}

/// Where `h` answers as itself: its adapter address on a Frame ([`frame_link_addr`]), else its
/// address, else the first of the addresses it left. The first two are asked at once.
fn where_answers(h: &KnownHost, on_frame: bool, timeout: std::time::Duration) -> Option<String> {
    let asks = |addr: &str| {
        answered_as_self(
            &h.fp_hex,
            NativeClient::probe_identity(addr, h.port, timeout),
        )
    };
    let asks = &asks;
    let adapter = frame_link_addr(h, on_frame);
    let (on_adapter, at_addr) = std::thread::scope(|s| {
        let probe = adapter.map(|a| s.spawn(move || asks(a)));
        let at_addr = asks(&h.addr);
        (probe.is_some_and(|j| j.join().unwrap_or(false)), at_addr)
    });
    if on_adapter {
        return adapter.map(str::to_string);
    }
    if at_addr {
        return Some(h.addr.clone());
    }
    if h.fp_hex.is_empty() {
        return None;
    }
    let hits: Vec<bool> = std::thread::scope(|s| {
        let handles: Vec<_> = h
            .prev_addrs
            .iter()
            .map(|a| s.spawn(move || asks(a.as_str())))
            .collect();
        handles
            .into_iter()
            .map(|j| j.join().unwrap_or(false))
            .collect()
    });
    h.prev_addrs
        .iter()
        .zip(hits)
        .find_map(|(a, hit)| hit.then(|| a.clone()))
}

/// The address a pinned host has on this Frame's adapter network, kept from its adverts, while
/// its saved address is elsewhere. `None` off a Frame.
fn frame_link_addr(h: &KnownHost, on_frame: bool) -> Option<&str> {
    use punktfunk_core::discovery::on_frame_adapter_net;
    let adapter = |a: &str| a.parse().is_ok_and(on_frame_adapter_net);
    if !on_frame || h.fp_hex.is_empty() || adapter(&h.addr) {
        return None;
    }
    h.prev_addrs.iter().map(String::as_str).find(|a| adapter(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An address is not an identity: a stranger on a sleeping host's lease completes the
    /// same handshake. Counting that as the host lights the pip and, since wake reads
    /// `!online`, silences Wake-on-LAN for the machine that needs it.
    #[test]
    fn a_probe_answered_by_someone_else_is_not_this_host() {
        let ours = "a".repeat(64);
        let theirs = [0xbbu8; 32];
        assert!(!answered_as_self(&ours, None), "nothing answered");
        assert!(
            !answered_as_self(&ours, Some(theirs)),
            "a stranger on the lease"
        );
        assert!(answered_as_self(&ours, Some([0xaau8; 32])));
        // The store spells a pin lowercase; a hand-typed one need not.
        assert!(answered_as_self(&ours.to_uppercase(), Some([0xaau8; 32])));
        // Saved by address, never paired: there is no pin to compare against.
        assert!(answered_as_self("", Some(theirs)));
        assert!(!answered_as_self("", None));
    }

    /// Only a Frame asks the adapter address, only for a pinned host, and only while the saved
    /// address is elsewhere: a host already on the adapter is asked there alone.
    #[test]
    fn a_frame_asks_its_adapter_address_first() {
        let h = KnownHost {
            addr: "192.168.0.130".into(),
            fp_hex: "a".repeat(64),
            prev_addrs: vec!["100.64.0.7".into(), "10.35.78.24".into()],
            ..Default::default()
        };
        assert_eq!(frame_link_addr(&h, true), Some("10.35.78.24"));
        assert_eq!(frame_link_addr(&h, false), None, "not a Frame");
        let bare = KnownHost {
            fp_hex: String::new(),
            ..h.clone()
        };
        assert_eq!(frame_link_addr(&bare, true), None, "saved by address alone");
        let there = KnownHost {
            addr: "10.35.78.24".into(),
            prev_addrs: vec!["10.35.78.23".into(), "192.168.0.130".into()],
            ..h
        };
        assert_eq!(
            frame_link_addr(&there, true),
            None,
            "already on the adapter"
        );
    }
}
