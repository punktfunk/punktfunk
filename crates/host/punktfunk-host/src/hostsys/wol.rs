//! Host-side Wake-on-LAN / Wake-on-Wireless-LAN.
//!
//! Two jobs, both best-effort (failure never affects streaming):
//!  1. [`wake_macs`] — advertise wake-capable NIC MACs so a client can persist
//!     them (mDNS `mac` TXT, [`crate::discovery`]) and wake this host once it
//!     is asleep. Wired and Wi-Fi: a magic packet is the same packet; an
//!     associated station in WoWLAN sleep receives the broadcast the AP buffers.
//!  2. [`warn_if_not_armed`] — detect and warn only. Never change NIC settings.
//!
//! Wired and wireless arm through different interfaces: `ethtool <iface>`
//! reports the wired `Wake-on: g` bit; a Wi-Fi NIC's trigger lives in nl80211
//! (`iw phy <phy> wowlan show`). Most wireless drivers print `Wake-on: d`
//! whether or not WoWLAN is armed, so ethtool on Wi-Fi is actively misleading.

use std::net::IpAddr;

/// Cap advertised MACs so the mDNS TXT record stays small. The routed NIC is always first.
const MAX_MACS: usize = 4;

/// Wake-capable NIC MACs, lowercase `aa:bb:cc:dd:ee:ff`. The NIC that bears `primary_ip` is first.
/// Empty means clients cannot auto-wake. All-zero MACs skipped; capped at [`MAX_MACS`].
pub fn wake_macs(primary_ip: IpAddr) -> Vec<String> {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();

    // One MAC per iface, but `get_if_addrs` yields one row per address — de-dupe by name.
    let mut names: Vec<String> = Vec::new();
    if let Some(primary) = ifaces.iter().find(|i| i.ip() == primary_ip) {
        names.push(primary.name.clone());
    }
    for i in &ifaces {
        if i.is_loopback() {
            continue;
        }
        if !names.contains(&i.name) {
            names.push(i.name.clone());
        }
    }

    let mut out: Vec<String> = Vec::new();
    for name in names {
        let Ok(Some(mac)) = mac_address::mac_address_by_name(&name) else {
            continue;
        };
        let b = mac.bytes();
        if b == [0u8; 6] {
            continue; // unset / virtual
        }
        let s = format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        );
        if !out.contains(&s) {
            out.push(s);
        }
        if out.len() >= MAX_MACS {
            break;
        }
    }
    out
}

/// Warn if the NIC bearing `primary_ip` is not armed for a magic packet. Never changes settings.
/// Linux-only (`iw`/`ethtool`); silent when it cannot tell.
#[cfg(target_os = "linux")]
pub fn warn_if_not_armed(primary_ip: IpAddr) {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let Some(iface) = ifaces
        .iter()
        .find(|i| i.ip() == primary_ip)
        .map(|i| i.name.clone())
    else {
        return;
    };

    // nl80211 phy ⇒ wireless: ask WoWLAN, not ethtool.
    if let Some(phy) = wireless_phy(&iface) {
        match wowlan_has_magic(phy.as_deref(), &iface) {
            Some(true) => tracing::info!(
                iface = %iface,
                phy = phy.as_deref().unwrap_or("?"),
                "Wake-on-WLAN armed (magic packet) on host Wi-Fi NIC"
            ),
            Some(false) => {
                let phy = phy.as_deref().unwrap_or("phy0");
                // Kernel wakeup off blocks any network wake; enabling WoWLAN alone would not fix it.
                let extra = if device_wakeup_enabled(&iface) == Some(false) {
                    " The kernel also has wake-up switched off for this device \
                     (/sys/class/net/<iface>/device/power/wakeup reads `disabled`), which blocks \
                     a network wake by itself."
                } else {
                    ""
                };
                tracing::warn!(
                    iface = %iface,
                    "Wake-on-WLAN is NOT armed on this host's Wi-Fi NIC — clients cannot wake it \
                     from sleep. Enable it with: sudo iw phy {phy} wowlan enable magic-packet \
                     (NetworkManager resets that on every re-connect; make it stick with: sudo \
                     nmcli connection modify <connection> 802-11-wireless.wake-on-wlan magic). \
                     The adapter must also stay powered and associated while the host sleeps, and \
                     be allowed to wake the machine in BIOS/UEFI.{extra}",
                )
            }
            None => {} // unknown: stay quiet
        }
        return;
    }

    match ethtool_wol_has_magic(&iface) {
        Some(true) => {
            tracing::info!(iface = %iface, "Wake-on-LAN armed (magic packet) on host NIC")
        }
        Some(false) => tracing::warn!(
            iface = %iface,
            "Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep. \
             Enable it with: sudo ethtool -s {iface} wol g  (and turn on 'Wake on LAN'/'Wake on \
             PCIe' in BIOS).",
        ),
        None => {} // unknown: stay quiet
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warn_if_not_armed(_primary_ip: IpAddr) {}

/// Nested option: `Some(Some("phy0"))` = wireless with a phy name; `Some(None)` = wireless,
/// phy unread; `None` = wired (or sysfs missing — the ethtool path then applies).
#[cfg(target_os = "linux")]
fn wireless_phy(iface: &str) -> Option<Option<String>> {
    let dir = format!("/sys/class/net/{iface}/phy80211");
    if !std::path::Path::new(&dir).exists() {
        return None;
    }
    let name = std::fs::read_to_string(format!("{dir}/name"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(name)
}

/// Whether a Wi-Fi NIC is armed for a magic-packet wake. `iw` reads the live nl80211 state.
///
/// Fallbacks when `iw` cannot answer (missing binary, no WoWLAN command, no phy, no privilege —
/// the host runs as a user service):
///  * a *positive* ethtool reading counts; a negative one never does — some drivers expose the
///    bit, but `Wake-on: d` from a wireless driver means nothing;
///  * then sysfs `device/power/wakeup`: `disabled` is conclusive in the negative — the kernel
///    will not arm this device, so firmware WoWLAN triggers can never fire.
#[cfg(target_os = "linux")]
fn wowlan_has_magic(phy: Option<&str>, iface: &str) -> Option<bool> {
    if let Some(v) = phy.and_then(iw_wowlan_has_magic) {
        return Some(v);
    }
    if let Some(true) = ethtool_wol_has_magic(iface) {
        return Some(true);
    }
    // Only the negative is meaningful: `enabled` does not mean a magic packet is a wake source.
    match device_wakeup_enabled(iface) {
        Some(false) => Some(false),
        _ => None,
    }
}

/// `/sys/class/net/<iface>/device/power/wakeup`: whether the kernel will arm this device at all.
/// `None` if the attribute is missing (platform/SDIO often have none) or unreadable.
#[cfg(target_os = "linux")]
fn device_wakeup_enabled(iface: &str) -> Option<bool> {
    let text =
        std::fs::read_to_string(format!("/sys/class/net/{iface}/device/power/wakeup")).ok()?;
    match text.trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

/// `iw phy <phy> wowlan show`: is the magic-packet trigger on? `None` if `iw` is missing or the driver has no WoWLAN.
#[cfg(target_os = "linux")]
fn iw_wowlan_has_magic(phy: &str) -> Option<bool> {
    let out = std::process::Command::new("iw")
        .args(["phy", phy, "wowlan", "show"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_iw_wowlan(&String::from_utf8_lossy(&out.stdout))
}

/// `ethtool <iface>`: does the current Wake-on setting include `g`? `None` if the tool or field is missing.
#[cfg(target_os = "linux")]
fn ethtool_wol_has_magic(iface: &str) -> Option<bool> {
    let out = std::process::Command::new("ethtool")
        .arg(iface)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ethtool_wol(&String::from_utf8_lossy(&out.stdout))
}

/// Does the current Wake-on setting include `g` (MagicPacket)? `None` if the field is absent.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn parse_ethtool_wol(text: &str) -> Option<bool> {
    for line in text.lines() {
        let t = line.trim();
        // Current setting is "Wake-on: <flags>"; skip "Supports Wake-on: ...". `g` = MagicPacket.
        if let Some(flags) = t.strip_prefix("Wake-on:") {
            return Some(flags.trim().contains('g'));
        }
    }
    None
}

/// `iw phy <phy> wowlan show` → is the magic-packet trigger on?
///
/// `WoWLAN is disabled` / `WoWLAN is enabled:` plus `* wake up on magic packet`.
/// `* wake up on anything` counts too (every frame, magic included). Enabled with
/// only other triggers is not armed. `None` when the output says nothing about WoWLAN.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn parse_iw_wowlan(text: &str) -> Option<bool> {
    let mut seen = false;
    let mut magic = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(state) = t.strip_prefix("WoWLAN is ") {
            seen = true;
            if state
                .trim()
                .trim_end_matches(':')
                .eq_ignore_ascii_case("disabled")
            {
                return Some(false);
            }
        } else if seen && t.starts_with('*') {
            let l = t.to_ascii_lowercase();
            if l.contains("magic packet") || l.contains("anything") {
                magic = true;
            }
        }
    }
    seen.then_some(magic)
}

#[cfg(test)]
mod tests {
    use super::{parse_ethtool_wol, parse_iw_wowlan};

    #[test]
    fn ethtool_current_setting_not_capability_line() {
        let armed =
            "Settings for enp5s0:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n\tLink detected: yes\n";
        assert_eq!(parse_ethtool_wol(armed), Some(true));
        // "Supports Wake-on: ...g..." must NOT be read as the current setting.
        let off = "Settings for enp5s0:\n\tSupports Wake-on: pumbg\n\tWake-on: d\n";
        assert_eq!(parse_ethtool_wol(off), Some(false));
        assert_eq!(
            parse_ethtool_wol("Settings for lo:\n\tLink detected: yes\n"),
            None
        );
    }

    #[test]
    fn iw_wowlan_states() {
        assert_eq!(parse_iw_wowlan("WoWLAN is disabled\n"), Some(false));
        assert_eq!(
            parse_iw_wowlan("WoWLAN is enabled:\n * wake up on magic packet\n"),
            Some(true)
        );
        // Enabled, but not for magic packets — a magic packet will not wake this NIC.
        assert_eq!(
            parse_iw_wowlan(
                "WoWLAN is enabled:\n * wake up on pattern match, up to 20 patterns of 16 - 128 bytes\n"
            ),
            Some(false)
        );
        // The `any` trigger wakes on every received frame, magic packets included.
        assert_eq!(
            parse_iw_wowlan("WoWLAN is enabled:\n * wake up on anything (device continues operating normally)\n"),
            Some(true)
        );
        // Nothing to go on — the driver has no WoWLAN command.
        assert_eq!(parse_iw_wowlan(""), None);
        assert_eq!(
            parse_iw_wowlan("Wiphy phy0\n\tmax # scan SSIDs: 20\n"),
            None
        );
    }
}
