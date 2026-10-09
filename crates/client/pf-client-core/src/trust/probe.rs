//! Reachability probes: whether a saved host answers as itself, and where.

use super::{hex, rekey_addr, KnownHost};
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
/// that replied, or `None` when nothing did; `want` is the record's pin, empty for one saved
/// by address alone — that has nothing to compare, so any answer is the host it names.
fn answered_as_self(want: &str, answered: Option<[u8; 32]>) -> bool {
    match answered {
        None => false,
        Some(fp) => want.is_empty() || hex(&fp).eq_ignore_ascii_case(want),
    }
}

/// Probe every saved host where it could be, in parallel: its address, then — only when
/// that is silent — the addresses it left (`prev_addrs`), all at once. A pinned host that
/// answers at one of those moves back there, so Tailscale takes over again once the LAN is
/// gone. A host saved by address alone is asked there only. Result index matches `hosts`:
/// `true` = online.
pub fn probe_known(hosts: &[KnownHost], timeout: std::time::Duration) -> Vec<bool> {
    let found: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|h| s.spawn(move || where_answers(h, timeout)))
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

/// Where `h` answers as itself: its address, else the first of the addresses it left.
fn where_answers(h: &KnownHost, timeout: std::time::Duration) -> Option<String> {
    let asks = |addr: &str| {
        answered_as_self(
            &h.fp_hex,
            NativeClient::probe_identity(addr, h.port, timeout),
        )
    };
    if asks(&h.addr) {
        return Some(h.addr.clone());
    }
    if h.fp_hex.is_empty() {
        return None;
    }
    let asks = &asks;
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
}
