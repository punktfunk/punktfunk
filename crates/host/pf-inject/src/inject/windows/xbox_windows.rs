//! Virtual Xbox pads on Windows via the UMDF HID minidriver — Xbox Wireless (device-type 4),
//! Xbox One S (5) and Xbox Elite Series 2 (6). HID-visible alternative to
//! [`super::gamepad_windows`]'s XUSB companion: `pf-xusb` registers only
//! `GUID_DEVINTERFACE_XUSB`, so hidapi / DirectInput / WGI never see it.
//!
//! Transport matches the PS/Deck pads (`SwDeviceCreate` + sealed channel). Stamp
//! `device_type` before the magic so the driver resolves identity before hidclass
//! asks for descriptors. Codec: [`super::xbox_proto`]. Descriptors: `pf_driver_proto::xbox`,
//! the Series one with Share and the One S / Elite one cut from it.
//!
//! Identities are Bluetooth Xbox pads on purpose. Wired ids (`045E:028E`, `045E:02EA`)
//! are vendor-class XUSB/GIP with no HID interface; a HID child claiming one has never
//! existed. Bluetooth ids (`0B13` / `02FD` / `0B22`) are the HID Xbox pads.
//!
//! No rich plane: no touchpad, lightbar, adaptive triggers, or IMU in the HID
//! contract. `apply_rich` / `clear_rich` / `neutralize_gyro` are no-ops; motion is
//! decoded and dropped (`GamepadPref::motion_reaches`).

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use super::xbox_proto::{neutral_xbox_report, parse_xbox_output, serialize_xbox_state, XboxState};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;

/// Xbox identity this backend can present. Same transport as `WinDsIdentity`
/// (`super::dualsense_windows`); only PnP identity and `device_type` differ.
/// Descriptor and report length follow `devtype` (`pf_driver_proto::xbox`).
pub(super) struct WinXboxIdentity {
    /// Stamped into the section; the driver picks VID/PID and product string
    /// from it before hidclass asks.
    pub devtype: u8,
    /// Distinct namespace per identity so two Xbox models never share a
    /// devnode shell.
    pub instance_prefix: &'static str,
    /// INF-matched hardware id. Must be a `pfGamepadXbox` model line in
    /// `pf_gamepad.inx` (`hwid_matches_inf` and
    /// `only_the_xbox_identity_installs_the_xinputhid_section`).
    pub hwid: &'static str,
    /// Synthesized onto the devnode so hidclass derives `HID\VID_045E&PID_xxxx`
    /// — the id SDL/RawInput/WGI and Microsoft's Xbox INFs key off.
    pub usb_vid_pid: &'static str,
    pub description: &'static str,
}

impl WinXboxIdentity {
    /// Xbox Wireless Controller (Series X|S) over Bluetooth, `045E:0B13`.
    /// Default: `0B13` is on Microsoft's `xinputhid.inf` allow-list; a software
    /// devnode still matches none of those entries, so `pfGamepadXbox`'s `AddReg`
    /// writes the two registry values the inbox sections would have written.
    pub(super) const fn wireless() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX,
            instance_prefix: "pf_xbox",
            hwid: "pf_xboxwireless",
            usb_vid_pid: "VID_045E&PID_0B13",
            description: "Punktfunk Virtual Xbox Wireless Controller",
        }
    }

    /// Xbox One S over Bluetooth, `045E:02FD`. `02FD` is only a `BTHENUM` id in
    /// `xinputhid.inf` — no stage-2 `HID\…&IG_00` line, unlike `0B13`. Promotion
    /// rides entirely on our `AddReg`. Unverified on hardware.
    pub(super) const fn one_s() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX_ONE_S,
            instance_prefix: "pf_xbox_ones",
            hwid: "pf_xboxones",
            usb_vid_pid: "VID_045E&PID_02FD",
            description: "Punktfunk Virtual Xbox One S Controller",
        }
    }

    /// Xbox Elite Wireless Controller Series 2, `045E:0B22`.
    /// No paddles: `BTN_PADDLE1..4` still fold for this identity. Once
    /// `xinputhid` promotes the pad it claims the HID collection exclusively,
    /// so extra descriptor buttons may be invisible to every consumer.
    pub(super) const fn elite() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX_ELITE,
            instance_prefix: "pf_xbox_elite",
            hwid: "pf_xboxelite",
            usb_vid_pid: "VID_045E&PID_0B22",
            description: "Punktfunk Virtual Xbox Elite Wireless Controller Series 2",
        }
    }
}

/// Xbox identities in wire order (`device_type` 4, 5, 6). INF tests sweep this
/// table: each `hwid` must sit on a `pfGamepadXbox` model line, and no
/// non-Xbox model line may. `static` not `const`: [`XboxWinProto`] holds
/// `&'static WinXboxIdentity`, and `&CONST[i]` is not `'static` without
/// rvalue static promotion.
pub(super) static XBOX_IDENTITIES: [WinXboxIdentity; 3] = [
    WinXboxIdentity::wireless(),
    WinXboxIdentity::one_s(),
    WinXboxIdentity::elite(),
];

/// The `pfGamepad` (unfiltered) Xbox model line in `pf_gamepad.inx`. Not in
/// [`XBOX_IDENTITIES`]: the INF tests require every id in that table to sit on
/// a `pfGamepadXbox` line, and this one deliberately does not.
pub(super) const XBOX_UNFILTERED_HWID: &str = "pf_xbox_nofilter";

/// Whether `xinputhid` is a registered service on this machine.
///
/// `pfGamepadXbox` appends it to the devnode's `UpperFilters`, and PnP treats a
/// filter service it cannot resolve as fatal. Windows Server — the SKU every
/// seat runs on — ships no `xinputhid` at all, so the host picks XUSB there.
pub fn xinputhid_registered() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };
    let mut key = HKEY::default();
    // SAFETY: a static wide literal, a live out-param, and the handle is closed
    // on the success path only — `RegOpenKeyExW` writes `key` only when it wins.
    let ok = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Services\xinputhid"),
            None,
            KEY_READ,
            &mut key,
        )
        .is_ok()
    };
    if ok {
        // SAFETY: `key` is the handle the call above just opened.
        unsafe {
            let _ = RegCloseKey(key);
        }
    }
    ok
}

/// The hardware id to enumerate `id` under: its own when `xinputhid` can promote
/// the pad, else the unfiltered line. Without the service the promotion the
/// filter exists for is impossible anyway, and asking for it costs the whole
/// device — `CM_PROB_REGISTRY`, no driver, no pad.
fn inf_hwid(id: &WinXboxIdentity) -> &'static str {
    static PROMOTABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *PROMOTABLE.get_or_init(|| {
        let present = xinputhid_registered();
        if !present {
            tracing::info!(
                "no xinputhid service on this Windows SKU — virtual Xbox pads enumerate unfiltered \
                 (HID and WGI see them; classic XInput cannot, which needs that service anyway)"
            );
        }
        present
    }) {
        id.hwid
    } else {
        XBOX_UNFILTERED_HWID
    }
}

/// Virtual Xbox pad: `SwDeviceCreate`'d `pf_xbox_<index>` plus the sealed
/// channel. Drop removes the devnode and closes both sections.
pub struct XboxWinPad {
    shm: ShmPad,
    /// Rumble `enable` bytes already logged for this pad — see [`XboxWinPad::service`].
    seen_enable: Vec<u8>,
    /// Input bytes the identity's descriptor declares; Share rides past One S / Elite's 16.
    report_len: usize,
}

impl XboxWinPad {
    /// Spawn the Bluetooth Xbox identity's devnode.
    fn open(index: u8, id: &WinXboxIdentity) -> Result<XboxWinPad> {
        let shm = ShmPad::open(
            index,
            id.devtype,
            &neutral_xbox_report(),
            &SwDeviceProfile {
                instance: &format!("{}_{index}", id.instance_prefix),
                // Per-family tag. The three identities share it: only one can hold
                // a given pad index, so their containers never collide.
                container_tag: 0x5046_5842, // "PFXB"
                container_index: index,
                hwid: inf_hwid(id),
                usb_vid_pid: Some(id.usb_vid_pid),
                // Bluetooth pad, not USB composite — no interface number. Deck
                // Steam promotion needs `&MI_02`; Xbox does not.
                usb_mi: None,
                bluetooth: false,
                description: id.description,
                // The HID child becomes `HID\VID_045E&PID_…&IG_00`: Steam merges a pad's views by
                // the VID/PID in its path, and under `punktfunk` it listed one Xbox pad twice.
                enumerator: id.usb_vid_pid,
                property: None,
            },
        )?;
        Ok(XboxWinPad {
            shm,
            seen_enable: Vec::new(),
            report_len: pf_driver_proto::xbox::input_len(id.devtype),
        })
    }

    /// Publish the identity's slice of `st`.
    fn write_state(&mut self, st: &XboxState) {
        let r = serialize_xbox_state(st);
        self.shm.publish(&r[..self.report_len]);
    }

    fn service(&mut self) -> (Option<(u16, u16, u16, u16)>, bool) {
        let mut rumble = None;
        let mut first_seen = Vec::new();
        let seen = &mut self.seen_enable;
        let resync = self.shm.poll(|bytes, _| {
            if let Some(r) = parse_xbox_output(bytes) {
                if !seen.contains(&bytes[1]) {
                    seen.push(bytes[1]);
                    first_seen.push(bytes.to_vec());
                }
                rumble = Some(r); // last rumble-carrying report wins
            }
        });
        // Which motor bits xinputhid sets is unmeasured; log each new mask once.
        for raw in first_seen {
            tracing::debug!(
                mailbox = self.shm.mailbox(),
                enable = %format!("{:#04x}", raw[1]),
                raw = ?raw,
                "xbox rumble enable byte"
            );
        }
        (rumble, resync)
    }
}

/// Windows-Xbox `PadProto`. Lifecycle lives in [`UhidManager`]. Identity is a
/// field, not three types: same codec, same output parse, same rumble plane.
/// `Default` is Xbox Wireless, so `XboxWindowsManager::new()` is unchanged.
pub struct XboxWinProto {
    identity: &'static WinXboxIdentity,
}

impl Default for XboxWinProto {
    fn default() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[0],
        }
    }
}

impl XboxWinProto {
    /// `045E:02FD` — `UhidManager::with_backend(XboxWinProto::one_s())`.
    pub fn one_s() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[1],
        }
    }

    /// `045E:0B22`.
    pub fn elite() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[2],
        }
    }
}

impl PadProto for XboxWinProto {
    type Pad = XboxWinPad;
    type State = XboxState;
    const LABEL: &'static str = "Xbox Wireless/Windows";
    const DEVICE: &'static str = "Xbox Wireless Controller";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<XboxWinPad> {
        let p = XboxWinPad::open(idx, self.identity)?;
        tracing::info!(
            index = idx,
            identity = self.identity.usb_vid_pid,
            description = self.identity.description,
            "virtual Xbox pad created (Windows UMDF HID)"
        );
        Ok(p)
    }

    fn merge_frame(&self, _prev: &XboxState, f: &punktfunk_core::input::GamepadFrame) -> XboxState {
        XboxState::from_frame(f)
    }

    /// No rich plane on an Xbox pad.
    fn apply_rich(&self, _st: &mut XboxState, _rich: RichInput) {}

    fn write_state(&self, pad: &mut XboxWinPad, st: &XboxState) {
        pad.write_state(st);
    }

    /// Motor rumble on 0xCA. `hidout` stays empty — no lightbar or adaptive
    /// triggers. Parity with Linux xpad.
    fn service(&self, pad: &mut XboxWinPad, _idx: u8) -> PadFeedback {
        let (rumble, resync) = pad.service();
        PadFeedback {
            rumble,
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

/// Session table of virtual Xbox pads, same surface as the other Windows pad
/// managers via [`UhidManager`].
pub type XboxWindowsManager = UhidManager<XboxWinProto>;
