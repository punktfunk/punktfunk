//! Virtual-gamepad backend selection for the native host session.
//!
//! [`pick_gamepad`] maps client [`GamepadPref`] plus `PUNKTFUNK_GAMEPAD` onto a backend this
//! host can build (compile-time OS flags). [`resolve_gamepad`] is the session shell: env, logs,
//! and the runtime degrades. [`resolve_pad_kind`] is the same ladder for one mixed-type pad
//! (no Auto/env). [`route_decision`] pins a live pad to its owning manager so a later declared
//! kind cannot duplicate it.
//!
//! `degrade_if_no_uhid`, `physical_steam_product_present`, and `degrade_steam_on_conflict`
//! are cfg-split linux/other. Linux clippy does not see the non-linux copies; re-verify on
//! Windows when those arms change.

use super::*;

/// Pin a pad to one manager for its lifetime.
///
/// A live device stays in `owner` even if `declared` later changes — never a second device
/// in another manager. `declared` is used only on create. Removal routes to `owner` so the
/// right device is torn down, then clears ownership.
pub(super) fn route_decision(
    owner: Option<GamepadPref>,
    declared: GamepadPref,
    present: bool,
) -> (GamepadPref, Option<GamepadPref>) {
    match (owner, present) {
        (Some(k), true) => (k, Some(k)),
        (Some(k), false) => (k, None),
        (None, true) => (declared, Some(declared)),
        (None, false) => (declared, None),
    }
}

/// Same platform + UHID/Steam degrades as [`resolve_gamepad`], without Auto/env.
/// A per-pad declaration is always a concrete kind.
pub(super) fn resolve_pad_kind(kind: GamepadPref) -> GamepadPref {
    let chosen = pick_gamepad(
        kind,
        None,
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    degrade_xbox_identity(degrade_steam_on_conflict(degrade_if_no_uhid(chosen)))
}

/// Session backend from client `pref`, then `PUNKTFUNK_GAMEPAD` under Auto, then Xbox 360.
///
/// `linux`/`windows` are the host OS. DualSense, DualShock 4, DualSense Edge, Xbox One, and
/// Steam Deck have both a Linux and a Windows backend; other wishes fold to Xbox 360 (never
/// an error — a session without rich pads still streams). Xbox Elite has no Linux identity
/// (`PadIdentity` stops at One S). Steam Controller and Switch Pro are Linux-only.
/// Steam Controller 2 is Linux UHID and Windows DEVTYPE_TRITON; the SC2 Puck has a native
/// Linux identity and folds onto the wired one on Windows.
///
/// Compile-time OS flags only. The Windows XUSB backend un-varies Xbox identity at runtime;
/// that fold is [`degrade_xbox_identity`], not this function.
fn pick_gamepad(pref: GamepadPref, env: Option<&str>, linux: bool, windows: bool) -> GamepadPref {
    let want = match pref {
        GamepadPref::Auto => env
            .and_then(GamepadPref::from_name)
            .unwrap_or(GamepadPref::Auto),
        explicit => explicit,
    };
    match want {
        GamepadPref::DualSense if linux || windows => GamepadPref::DualSense,
        GamepadPref::DualShock4 if linux || windows => GamepadPref::DualShock4,
        GamepadPref::XboxOne if linux || windows => GamepadPref::XboxOne,
        // No Linux uinput Elite identity (`PadIdentity` stops at One S); `_` → Xbox360.
        GamepadPref::XboxElite if windows => GamepadPref::XboxElite,
        GamepadPref::SteamDeck if linux => GamepadPref::SteamDeck,
        GamepadPref::SteamController if linux => GamepadPref::SteamController,
        GamepadPref::SteamDeck if windows => GamepadPref::SteamDeck,
        GamepadPref::DualSenseEdge if linux || windows => GamepadPref::DualSenseEdge,
        // Linux UHID hid-nintendo (≥ 5.16). No Windows backend.
        GamepadPref::SwitchPro if linux => GamepadPref::SwitchPro,
        // Linux: UHID passthrough under 28DE:1302; no kernel driver, Steam Input consumes hidraw.
        GamepadPref::SteamController2 if linux => GamepadPref::SteamController2,
        GamepadPref::SteamController2 if windows => GamepadPref::SteamController2,
        // 28DE:1304's seven interfaces have a native Linux synthesis only.
        GamepadPref::SteamController2Puck if linux => GamepadPref::SteamController2Puck,
        // Windows needs none: the dongle PIDs are client-side transports, and Steam treats the
        // wired PID as the canonical controller (triton_proto.rs), so a Puck pad folds onto the
        // same 28DE:1302 pad a cabled one mints. That descriptor declares 0x79, so the client's
        // forwarded connect edge stays legal on it.
        GamepadPref::SteamController2Puck if windows => GamepadPref::SteamController2,
        _ => GamepadPref::Xbox360,
    }
}

/// If `/dev/uhid` is not writable *now*, fold UHID backends to the uinput Xbox 360 pad.
/// Opens and drops the char device — no `UHID_CREATE2`, so nothing is created. No-op off Linux.
#[cfg(target_os = "linux")]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    let needs_uhid = matches!(
        chosen,
        GamepadPref::DualSense
            | GamepadPref::DualSenseEdge
            | GamepadPref::DualShock4
            | GamepadPref::SteamDeck
            | GamepadPref::SteamController
            | GamepadPref::SteamController2
            | GamepadPref::SteamController2Puck
            | GamepadPref::SwitchPro
    );
    if needs_uhid
        && std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uhid")
            .is_err()
    {
        tracing::warn!(
            wanted = chosen.as_str(),
            "/dev/uhid not writable — falling back to the X-Box 360 pad"
        );
        return GamepadPref::Xbox360;
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// `true` when `steamos-manager` is running (`comm` is 15 chars — the name fits exactly) and
/// SELinux is enforcing (`enforce` reads `1`). Both required: permissive logs one denial per
/// walk; without steamos-manager there is no ds_inhibit.
#[cfg(target_os = "linux")]
fn ds_inhibit_storm_risk(proc_root: &std::path::Path, enforce: &std::path::Path) -> bool {
    let enforcing = std::fs::read_to_string(enforce).is_ok_and(|v| v.trim() == "1");
    if !enforcing {
        return false;
    }
    std::fs::read_dir(proc_root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|c| c.trim() == "steamos-manager")
        })
}

/// Warn once when a hid-playstation pad would trip Valve's ds_inhibit walk under SELinux.
///
/// `steamos-manager` walks `/proc/*/fd/` on every hidraw open/close (no VID/PID filter).
/// Under enforcing that walk is denied; `setroubleshootd` can amplify it into a stall.
/// Warn-only: a per-pad fold has no wire channel (`Welcome::gamepad` is the session default)
/// and would strip DualSense here. The fix is the shipped SELinux drop-in; this cannot see
/// the policy store (root-only), so it still fires after install. Audit
/// `comm="tokio-rt-worker"` is steamos-manager — check `scontext=`.
#[cfg(target_os = "linux")]
fn warn_if_ds_inhibit_storm(chosen: GamepadPref) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    // hid-playstation backends. The kernel's own DS5/DS4 touchpad path selects them; descriptor
    // shaping cannot duck it.
    let playstation = matches!(
        chosen,
        GamepadPref::DualSense | GamepadPref::DualSenseEdge | GamepadPref::DualShock4
    );
    if !playstation
        || !ds_inhibit_storm_risk(
            std::path::Path::new("/proc"),
            std::path::Path::new("/sys/fs/selinux/enforce"),
        )
        || !ONCE.swap(false, Ordering::Relaxed)
    {
        return;
    }
    tracing::warn!(
        gamepad = chosen.as_str(),
        "steamos-manager is running and SELinux is enforcing — its ds_inhibit scans /proc on \
         every open/close of this pad's hidraw, the scan is denied at hundreds of AVCs/sec, and \
         setroubleshootd can amplify that into a box-wide stall that starves the stream. Install \
         the shipped SELinux drop-in (`sudo punktfunk-sysext reapply`, or `sudo semodule -i \
         /usr/share/punktfunk/selinux/punktfunk-ds-inhibit.cil`) — harmless if already installed. \
         Masking setroubleshootd (`sudo systemctl mask --now setroubleshootd`) hardens the box \
         against any audit flood. Details: packaging/bazzite/README.md."
    );
}

#[cfg(not(target_os = "linux"))]
fn warn_if_ds_inhibit_storm(_chosen: GamepadPref) {}

/// Valve product id (`28DE:xxxx`) this virtual Steam backend enumerates as.
/// The conflict gate matches VID **and** PID; vendor `28DE` alone is not a conflict.
#[cfg(target_os = "linux")]
fn steam_backend_product(pref: GamepadPref) -> Option<u16> {
    match pref {
        GamepadPref::SteamDeck => Some(0x1205),
        GamepadPref::SteamController => Some(0x1102),
        GamepadPref::SteamController2 => Some(0x1302),
        GamepadPref::SteamController2Puck => Some(0x1304),
        _ => None,
    }
}

/// True if a physical Valve device with this exact product id is already attached.
///
/// Match VID **and** PID: a physical SC2 (`28DE:1302`) must not block a virtual Deck
/// (`28DE:1205`) — Steam Input drives distinct controllers side by side.
///
/// HID dirs are `BUS:VID:PID.INST`. Skip our own pads (`HID_UNIQ=FVPF…`,
/// [`steam_proto::deck_serial`]). Skip `/devices/virtual/` and `vhci_hcd`: usbip/gadget
/// presents a real USB device, so a detaching or concurrent session pad would look
/// physical and fold the next Deck session.
#[cfg(target_os = "linux")]
fn physical_steam_product_present(product: u16) -> bool {
    let needle = format!(":28DE:{product:04X}");
    let Ok(entries) = std::fs::read_dir("/sys/bus/hid/devices") else {
        return false;
    };
    entries.flatten().any(|e| {
        if !e.file_name().to_string_lossy().contains(&needle) {
            return false;
        }
        if std::fs::read_to_string(e.path().join("uevent"))
            .is_ok_and(|u| u.lines().any(|l| l.starts_with("HID_UNIQ=FVPF")))
        {
            return false;
        }
        match std::fs::read_link(e.path()) {
            Ok(target) => {
                let t = target.to_string_lossy();
                !t.contains("/virtual/") && !t.contains("vhci_hcd")
            }
            Err(_) => true,
        }
    })
}

/// Fold a virtual Steam pad to DualSense when a physical same-PID Valve device is attached.
/// Override with `PUNKTFUNK_STEAM_FORCE=1` on a host with no competing Steam Input.
#[cfg(target_os = "linux")]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    let Some(product) = steam_backend_product(chosen) else {
        return chosen;
    };
    let forced = std::env::var("PUNKTFUNK_STEAM_FORCE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !forced && physical_steam_product_present(product) {
        let conflict = format!("28DE:{product:04X}");
        tracing::warn!(
            wanted = chosen.as_str(),
            conflict = conflict.as_str(),
            "a physical Steam controller of the same identity is attached — the host's Steam Input \
             would manage two identical 28DE devices; falling back to DualSense (set \
             PUNKTFUNK_STEAM_FORCE=1 to override)"
        );
        return degrade_if_no_uhid(GamepadPref::DualSense);
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Fold Xbox One / Elite to 360 when [`windows_xbox_hid`] picks XUSB.
/// The XUSB companion has one fixed 360 identity; folding here keeps the `Welcome` echo honest.
/// No-op off Windows (`XboxElite` never survives [`pick_gamepad`] there; `XboxOne` is uinput).
#[cfg(target_os = "windows")]
fn degrade_xbox_identity(chosen: GamepadPref) -> GamepadPref {
    if matches!(chosen, GamepadPref::XboxOne | GamepadPref::XboxElite) && !windows_xbox_hid() {
        tracing::warn!(
            wanted = chosen.as_str(),
            "the XUSB backend has one fixed X-Box 360 identity — falling back to the 360 pad"
        );
        return GamepadPref::Xbox360;
    }
    chosen
}

#[cfg(not(target_os = "windows"))]
fn degrade_xbox_identity(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Build Xbox-family pads as HID ([`crate::inject::xbox_windows`]) instead of XUSB
/// ([`crate::inject::gamepad`]). Windows only; decided once per process by [`xbox_backend_hid`]
/// and logged.
///
/// XUSB registers only `GUID_DEVINTERFACE_XUSB` — no HID collection — so Steam, DirectInput,
/// `joy.cpl`, and WGI/GameInput never see it. HID plus inbox `xinputhid` is a superset, but
/// without that filter (Windows Server) XInput cannot see the HID pad at all.
///
/// The two backends are mutually exclusive per pad: both would be two controllers for one pair
/// of hands. Read by both input planes (`Pads::handle` and `gamestream::control::SessionPads`).
#[cfg(target_os = "windows")]
pub(crate) fn windows_xbox_hid() -> bool {
    static HID: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HID.get_or_init(|| {
        let env = std::env::var("PUNKTFUNK_XBOX_BACKEND").ok();
        let xinputhid = crate::inject::xbox_windows::xinputhid_registered();
        let hid = xbox_backend_hid(env.as_deref(), xinputhid);
        tracing::info!(
            backend = if hid { "hid" } else { "xusb" },
            env = env.as_deref().unwrap_or(""),
            xinputhid,
            "virtual Xbox pad backend"
        );
        hid
    })
}

/// `PUNKTFUNK_XBOX_BACKEND` (`hid` / `xusb`) wins; unset or unrecognised follows `xinputhid`.
/// Without that filter the HID pad sits on the unfiltered line, where XInput cannot see it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn xbox_backend_hid(env: Option<&str>, xinputhid: bool) -> bool {
    match env.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("xusb") => false,
        Some(v) if v.eq_ignore_ascii_case("hid") => true,
        _ => xinputhid,
    }
}

/// Env/logging shell around [`pick_gamepad`]. Always concrete: `Welcome` reports what we drive.
pub(super) fn resolve_gamepad(pref: GamepadPref) -> GamepadPref {
    let env = pf_host_config::config().gamepad.clone();
    let chosen = pick_gamepad(
        pref,
        env.as_deref(),
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    let chosen = degrade_if_no_uhid(chosen);
    let chosen = degrade_steam_on_conflict(chosen);
    let chosen = degrade_xbox_identity(chosen);
    warn_if_ds_inhibit_storm(chosen);
    match pref {
        GamepadPref::Auto => {
            // Env did not produce `chosen` (typo, or a DualSense wish with no UHID): log it.
            if let Some(env) = env.as_deref() {
                if GamepadPref::from_name(env) != Some(chosen) {
                    tracing::warn!(
                        env,
                        chosen = chosen.as_str(),
                        "PUNKTFUNK_GAMEPAD unrecognized or unavailable — falling back"
                    );
                }
            }
            tracing::info!(gamepad = chosen.as_str(), "gamepad backend (client: auto)")
        }
        want if want == chosen => {
            tracing::info!(gamepad = chosen.as_str(), "honoring client gamepad request")
        }
        want => tracing::warn!(
            requested = want.as_str(),
            chosen = chosen.as_str(),
            "client-requested gamepad backend unavailable — falling back"
        ),
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::{pick_gamepad, route_decision, xbox_backend_hid};
    use punktfunk_core::config::GamepadPref;

    /// The env override wins either way; otherwise HID only where `xinputhid` can promote it.
    #[test]
    fn xbox_backend_follows_xinputhid_unless_overridden() {
        assert!(xbox_backend_hid(None, true));
        assert!(!xbox_backend_hid(None, false), "no xinputhid → XUSB");
        assert!(!xbox_backend_hid(Some(" XUSB "), true));
        assert!(xbox_backend_hid(Some("hid"), false), "operator forces HID");
        assert!(
            !xbox_backend_hid(Some("hdi"), false),
            "a typo follows the machine"
        );
        assert!(xbox_backend_hid(Some(""), true));
    }

    #[test]
    fn per_pad_route_decision() {
        use GamepadPref::{DualSense, Xbox360};
        assert_eq!(
            route_decision(None, DualSense, true),
            (DualSense, Some(DualSense))
        );
        // Arrival-after-first-frame reorder: stay on `owner`, never a second device.
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, true),
            (DualSense, Some(DualSense))
        );
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, false),
            (DualSense, None)
        );
        assert_eq!(route_decision(None, Xbox360, false), (Xbox360, None));
        assert_eq!(
            route_decision(None, Xbox360, true),
            (Xbox360, Some(Xbox360))
        );
    }

    #[test]
    fn gamepad_resolution_precedence() {
        use GamepadPref::*;
        assert_eq!(
            pick_gamepad(DualSense, Some("xbox360"), true, false),
            DualSense
        );
        assert_eq!(
            pick_gamepad(Xbox360, Some("dualsense"), true, false),
            Xbox360
        );
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), true, false),
            DualSense
        );
        assert_eq!(pick_gamepad(Auto, Some("xbox360"), true, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, None, true, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("bogus"), true, false), Xbox360);
        assert_eq!(pick_gamepad(DualSense, None, false, true), DualSense);
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), false, true),
            DualSense
        );
        assert_eq!(pick_gamepad(DualSense, None, false, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("dualsense"), false, false), Xbox360);
        assert_eq!(pick_gamepad(DualShock4, None, true, false), DualShock4);
        assert_eq!(pick_gamepad(Auto, Some("ps4"), true, false), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, true), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, false), Xbox360);
        // Distinct on Linux (uinput) and Windows (UMDF). The xusb fold is [`degrade_xbox_identity`].
        assert_eq!(pick_gamepad(XboxOne, None, true, false), XboxOne);
        assert_eq!(pick_gamepad(Auto, Some("series"), true, false), XboxOne);
        assert_eq!(pick_gamepad(XboxOne, None, false, true), XboxOne);
        assert_eq!(pick_gamepad(XboxOne, None, false, false), Xbox360);
        // Windows-only; no Linux uinput Elite identity.
        assert_eq!(pick_gamepad(XboxElite, None, false, true), XboxElite);
        assert_eq!(pick_gamepad(Auto, Some("elite"), false, true), XboxElite);
        assert_eq!(pick_gamepad(XboxElite, None, true, false), Xbox360);
        assert_eq!(pick_gamepad(XboxElite, None, false, false), Xbox360);

        assert_eq!(pick_gamepad(SteamDeck, None, true, false), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, true), SteamDeck);
        assert_eq!(pick_gamepad(Auto, Some("deck"), false, true), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, false), Xbox360);
        assert_eq!(
            pick_gamepad(SteamController, None, true, false),
            SteamController
        );
        assert_eq!(
            pick_gamepad(Auto, Some("steamcontroller"), true, false),
            SteamController
        );
        assert_eq!(pick_gamepad(SteamController, None, false, true), Xbox360);

        assert_eq!(
            pick_gamepad(DualSenseEdge, None, true, false),
            DualSenseEdge
        );
        assert_eq!(
            pick_gamepad(DualSenseEdge, None, false, true),
            DualSenseEdge
        );
        assert_eq!(pick_gamepad(Auto, Some("edge"), true, false), DualSenseEdge);
        assert_eq!(pick_gamepad(DualSenseEdge, None, false, false), Xbox360);
        assert_eq!(pick_gamepad(SwitchPro, None, true, false), SwitchPro);
        assert_eq!(
            pick_gamepad(Auto, Some("switchpro"), true, false),
            SwitchPro
        );
        assert_eq!(pick_gamepad(Auto, Some("switch"), true, false), SwitchPro);
        assert_eq!(pick_gamepad(SwitchPro, None, false, true), Xbox360);
        assert_eq!(pick_gamepad(SwitchPro, None, false, false), Xbox360);
        assert_eq!(
            pick_gamepad(SteamController2, None, true, false),
            SteamController2
        );
        assert_eq!(
            pick_gamepad(Auto, Some("sc2"), true, false),
            SteamController2
        );
        assert_eq!(
            pick_gamepad(Auto, Some("ibex"), true, false),
            SteamController2
        );
        assert_eq!(
            pick_gamepad(SteamController2, None, false, true),
            SteamController2
        );
        assert_eq!(pick_gamepad(SteamController2, None, false, false), Xbox360);
        assert_eq!(
            pick_gamepad(SteamController2Puck, None, true, false),
            SteamController2Puck
        );
        assert_eq!(
            pick_gamepad(Auto, Some("sc2puck"), true, false),
            SteamController2Puck
        );
        // Windows has no virtual Puck; the pref folds onto the wired Triton identity, which
        // Steam treats as the canonical controller — NOT to the Xbox 360 degrade.
        assert_eq!(
            pick_gamepad(SteamController2Puck, None, false, true),
            SteamController2
        );
    }

    // Gate keys on PID: physical SC2 (1302) must not block a virtual Deck (1205).
    #[cfg(target_os = "linux")]
    #[test]
    fn steam_backend_product_ids() {
        use super::steam_backend_product;
        use GamepadPref::*;
        assert_eq!(steam_backend_product(SteamDeck), Some(0x1205));
        assert_eq!(steam_backend_product(SteamController), Some(0x1102));
        assert_eq!(steam_backend_product(SteamController2), Some(0x1302));
        assert_eq!(steam_backend_product(SteamController2Puck), Some(0x1304));
        assert_eq!(steam_backend_product(DualSense), None);
        assert_eq!(steam_backend_product(Xbox360), None);
        assert_eq!(steam_backend_product(SwitchPro), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ds_inhibit_storm_risk_needs_both_halves() {
        use super::ds_inhibit_storm_risk;
        let dir = tempfile::tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let enforce = dir.path().join("enforce");
        std::fs::create_dir_all(proc_root.join("123")).unwrap();

        std::fs::write(proc_root.join("123/comm"), "steamos-manager\n").unwrap();
        std::fs::write(&enforce, "1\n").unwrap();
        assert!(ds_inhibit_storm_risk(&proc_root, &enforce));

        std::fs::write(&enforce, "0\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
        assert!(!ds_inhibit_storm_risk(
            &proc_root,
            &dir.path().join("missing")
        ));

        std::fs::write(&enforce, "1\n").unwrap();
        std::fs::write(proc_root.join("123/comm"), "not-steamos\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
        // Whole trimmed `comm` only — a prefix/contains match would false-positive.
        std::fs::write(proc_root.join("123/comm"), "steamos-managerX\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
    }
}
