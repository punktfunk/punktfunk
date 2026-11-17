//! JNI seam for Wake-on-LAN: parse the stored MAC strings and hand them to the shared core sender
//! (`punktfunk_core::wol`). Like [`crate::discovery`], this takes no session handle — a sleeping
//! host has no ARP entry, so the broadcast the core sends is what wakes it, and Kotlin calls this
//! to wake an offline saved host.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::EnvUnowned;

/// `NativeBridge.nativeWakeOnLan(macsCsv: String, lastIp: String): Boolean` — send a Wake-on-LAN
/// magic packet. `macsCsv` is comma-separated MACs (`aa:bb:..,cc:dd:..`, learned from the host's
/// mDNS `mac` TXT while it was online); `lastIp` is the host's last-known IPv4 (or empty).
/// Returns true if at least one datagram went out.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeWakeOnLan<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    macs_csv: JString<'local>,
    last_ip: JString<'local>,
) -> jni::sys::jboolean {
    env.with_env(|env| -> jni::errors::Result<bool> {
        let macs_csv: String = macs_csv.try_to_string(env)?;
        // Unlike `macs_csv`, an unreadable `lastIp` is not fatal: the core falls back to the
        // subnet broadcast when it has no address, so it reads as empty.
        let last_ip: String = last_ip.try_to_string(env).unwrap_or_default();
        let macs: Vec<[u8; 6]> = macs_csv
            .split(',')
            .filter_map(|s| punktfunk_core::wol::parse_mac(s.trim()))
            .collect();
        if macs.is_empty() {
            return Ok(false);
        }
        let ip = last_ip.trim().parse::<std::net::Ipv4Addr>().ok();
        Ok(punktfunk_core::wol::send_magic_packet(&macs, ip).is_ok())
    })
    .resolve::<LogErrorAndDefault>()
}
