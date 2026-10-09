//! mDNS `_nvstream._tcp.local.` so Moonlight finds the host. Manual IP add still works.

use crate::host::Host;
use anyhow::{Context, Result};
use mdns_sd::ServiceInfo;
use std::collections::HashMap;

use crate::discovery::Advert;

const SERVICE: &str = "_nvstream._tcp.local.";

pub fn advertise(host: &Host) -> Result<Advert> {
    // Instance name is the display name Moonlight lists; A-record target is a sanitized
    // DNS label so a free-text `PUNKTFUNK_HOST_NAME` cannot emit an illegal record.
    let host_name = format!("{}.local.", crate::discovery::dns_label(&host.hostname));
    let instance = host.hostname.clone();
    let port = host.http_port;
    tracing::info!(
        service = "_nvstream._tcp",
        port,
        host = %host_name,
        "mDNS advertising"
    );
    // Address is per-registration so the record follows the host onto a network that
    // came up after boot — see [`crate::discovery::advertise_live`].
    crate::discovery::advertise_live(SERVICE, move |ip| {
        // Moonlight needs no TXT; it resolves the A record then GETs /serverinfo.
        let props: HashMap<String, String> = HashMap::new();
        ServiceInfo::new(SERVICE, &instance, &host_name, ip, port, props)
            .context("build mDNS ServiceInfo")
    })
}
