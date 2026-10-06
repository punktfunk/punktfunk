//! Native punktfunk mDNS advert; [`crate::gamestream::mdns`] is the GameStream analogue.
//!
//! Type **`_punktfunk._udp.local.`** (UDP: the protocol is QUIC). Port is the QUIC
//! control/data port a client `--connect`s. TXT:
//!
//! - `proto` — service id ([`NATIVE_PROTO`]). Clients drop an advert with another value, so it
//!   stays `punktfunk/1` while the protocol itself moves; `wire` names the protocol.
//! - `fp` — host cert SHA-256 (lowercase hex). mDNS is unauthenticated, so this is advisory;
//!   TOFU still verifies on connect.
//! - `pair` — `required` or `optional`.
//! - `id` — stable uniqueid (dedup across IPs / re-advertises).
//! - `mgmt` — management API TCP port when served; omitted otherwise.
//! - `mac` — wake-capable NIC MAC(s), comma-separated, routed NIC first; omitted when none.
//! - `os` — OS identity chain (`linux/fedora/bazzite`; [`crate::osinfo`]).
//! - `wire` — the punktfunk protocols this host answers, comma-separated (`2`).
//! - `addr` — IPv4 this advert was registered for. The resolved A-set is a union polluted by
//!   other per-interface responders; the picker uses this as a tie-break.
//!
//! Every TXT value is advisory. Pinning and pairing still happen on connect.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc;
use std::time::Duration;

pub const NATIVE_SERVICE: &str = "_punktfunk._udp.local.";

/// The `PUNKTFUNK_MDNS` row. Default ON; off disables both native and GameStream adverts.
/// CLI `--no-mdns` is the same knob. Multicast-dead environments (bridged Docker, CI netns)
/// otherwise abort the GameStream plane; clients can still dial a manually-added host.
pub(crate) fn mdns_enabled() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_MDNS")
}

pub const NATIVE_PROTO: &str = "punktfunk/1";

/// A-record target (`<label>.local.`), not the service instance name. Instance may be free
/// text (`PUNKTFUNK_HOST_NAME`); a host name may not. `mdns-sd` rejects the whole `ServiceInfo`
/// if the target is illegal, which takes discovery down rather than looking wrong. Already-legal
/// names pass through; anything outside `[A-Za-z0-9.-]` becomes `-`.
pub(crate) fn dns_label(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if out.len() >= 63 {
            break;
        }
        out.push(if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            c
        } else {
            '-'
        });
    }
    let out = out.trim_matches(['-', '.']).to_string();
    if out.is_empty() {
        "punktfunk-host".to_string()
    } else {
        out
    }
}

/// Holds the mDNS daemon; dropping it unregisters the service and stops the re-announce loop.
pub struct Advert {
    _daemon: ServiceDaemon,
    /// Never sent on. Drop disconnects the re-announce thread's recv and ends it immediately,
    /// instead of leaving a loop polling for a service nobody advertises.
    _stop: mpsc::Sender<()>,
}

const IP_RECHECK: Duration = Duration::from_secs(10);

/// Loopback only while the machine still has none.
fn current_ip() -> IpAddr {
    crate::host::primary_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Register `build(ip)` for the current address and again whenever it changes. Shared by
/// [`advertise_native`] and [`crate::gamestream::mdns`].
///
/// mDNS records are pushed: the address true at `register()` keeps being announced until a
/// newer one is registered. A second `register()` of the same fullname is an update.
///
/// Poll the *routed* address; do not subscribe to the daemon's `IpAdd` events. The NIC often
/// already has an address and only the default route lands late, so no interface event fires.
pub(crate) fn advertise_live(
    service: &'static str,
    build: impl Fn(IpAddr) -> Result<ServiceInfo> + Send + 'static,
) -> Result<Advert> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let registered = current_ip();
    daemon
        .register(build(registered)?)
        .with_context(|| format!("register {service} mDNS service"))?;

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let bg_daemon = daemon.clone();
    std::thread::spawn(move || {
        let mut announced = registered;
        // Sleep and stop signal: timeout → re-check; `Disconnected` when `Advert` drops.
        while matches!(
            stop_rx.recv_timeout(IP_RECHECK),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            let now = current_ip();
            if now == announced {
                continue;
            }
            match build(now)
                .and_then(|info| bg_daemon.register(info).context("re-register mDNS service"))
            {
                Ok(()) => {
                    tracing::info!(service, from = %announced, to = %now, "host address changed — re-announced");
                    announced = now;
                }
                // Keep the previous record and retry next tick rather than going dark.
                Err(e) => {
                    tracing::warn!(service, error = %format!("{e:#}"), "mDNS re-announce failed");
                }
            }
        }
    });
    Ok(Advert {
        _daemon: daemon,
        _stop: stop_tx,
    })
}

/// Native LAN advert. One argument per TXT key; a params struct would restate the module doc.
#[allow(clippy::too_many_arguments)]
pub fn advertise_native(
    hostname: &str,
    port: u16,
    fingerprint: &str,
    require_pairing: bool,
    uniqueid: &str,
    mgmt_port: Option<u16>,
    os_chain: &str,
) -> Result<Advert> {
    // `hostname` is the instance label clients read back; the A-record target must be a legal
    // DNS name, hence [`dns_label`].
    let host_name = format!("{}.local.", dns_label(hostname));
    // Owned: [`advertise_live`] rebuilds the record on address change. Everything except the
    // address (and MACs derived from it) is fixed, so it is computed once and moved in.
    let instance = hostname.to_string();
    let mut fixed: HashMap<String, String> = HashMap::new();
    fixed.insert("proto".into(), NATIVE_PROTO.into());
    fixed.insert("fp".into(), fingerprint.to_string());
    fixed.insert(
        "pair".into(),
        if require_pairing {
            "required"
        } else {
            "optional"
        }
        .into(),
    );
    fixed.insert("id".into(), uniqueid.to_string());
    fixed.insert("wire".into(), "2".into());
    if let Some(mgmt) = mgmt_port {
        fixed.insert("mgmt".into(), mgmt.to_string());
    }
    if !os_chain.is_empty() {
        fixed.insert("os".into(), os_chain.to_string());
    }
    tracing::info!(
        service = "_punktfunk._udp",
        port,
        host = %host_name,
        pair = if require_pairing { "required" } else { "optional" },
        "native punktfunk mDNS advertising"
    );
    advertise_live(NATIVE_SERVICE, move |ip| {
        let mut props = fixed.clone();
        // Re-read on each rebuild: the routed NIC (and its MAC) may have changed.
        let macs = crate::wol::wake_macs(ip);
        if !macs.is_empty() {
            props.insert("mac".into(), macs.join(","));
        }
        props.insert("addr".into(), ip.to_string());
        // Detect and warn only. Re-check on address change: the routed NIC may be a different one.
        crate::wol::warn_if_not_armed(ip);
        ServiceInfo::new(NATIVE_SERVICE, &instance, &host_name, ip, port, props)
            .context("build native mDNS ServiceInfo")
    })
}

#[cfg(test)]
mod tests {
    use super::dns_label;

    #[test]
    fn dns_label_passes_machine_names_through_and_tames_display_names() {
        // Machine hostnames (no `PUNKTFUNK_HOST_NAME`) pass through unchanged.
        for plain in ["bazzite-htpc", "DESKTOP-1A2B3C", "box.lan", "steamdeck"] {
            assert_eq!(dns_label(plain), plain);
        }
        // Free-text display names become a legal label instead of poisoning the record.
        assert_eq!(dns_label("Living Room PC"), "Living-Room-PC");
        assert_eq!(dns_label("  Ben's Rig!  "), "Ben-s-Rig");
        assert_eq!(dns_label("Wohnzimmer-PC ☕"), "Wohnzimmer-PC");
        assert_eq!(dns_label("***"), "punktfunk-host");
        assert_eq!(dns_label(""), "punktfunk-host");
        // DNS caps a label at 63 bytes.
        assert!(dns_label(&"a".repeat(200)).len() <= 63);
    }
}
