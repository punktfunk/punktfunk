//! Virtual DualSense on Windows via the UMDF minidriver (`packaging/windows/drivers/pf-gamepad`).
//!
//! Same [`DsState`] and report codec as the Linux UHID backend ([`super::dualsense`],
//! [`super::dualsense_proto`]). Transport is [`ShmPad`]: an unnamed `PadShm` DATA section the
//! host duplicates into the driver's WUDFHost through `Global\pfds-boot-<idx>`. hidclass owns the
//! device stack, so a UMDF minidriver has no control device — this IPC is the only channel
//! (`windows-dualsense-scoping.md`).
//!
//! Each pad `SwDeviceCreate`s a `pf_pad_<index>` software devnode (hwid `pf_dualsense`) on open
//! and `SwDeviceClose`s it on drop. The driver package must already be installed.

use super::dualsense_proto::{
    parse_ds_output, DsEncoder, DsFeedback, DsState, DS_TOUCH_H, DS_TOUCH_W,
};
use super::gamepad_raii::{create_swdevice, PadChannel, ProofTransport, SwDeviceProfile};
use super::pad_shm::ShmPad;
use crate::pad_shm_ring::{stamp, OFF_OUTPUT, OFF_OUT_SEQ, SHM_SIZE};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;
use std::sync::atomic::Ordering;

/// One virtual DualSense or Edge: a `pf_pad_<index>` / `pf_edge_<index>` devnode plus the sealed
/// channel. Public because it is `PadProto::Pad`.
pub struct DsWinPad {
    shm: ShmPad,
    enc: DsEncoder,
}

/// Identity a [`DsWinPad`] enumerates with. DualSense and Edge share the transport and report
/// codec; only `device_type` and PnP identity differ. DS4 differs in report codec too, so it
/// keeps its own pad type.
pub(super) struct WinDsIdentity {
    /// Stamped into the section; the driver picks its HID identity off it.
    pub devtype: u8,
    /// Distinct namespaces per type (`pf_pad` / `pf_edge`).
    pub instance_prefix: &'static str,
    pub hwid: &'static str,
    pub usb_vid_pid: &'static str,
    pub description: &'static str,
    /// See [`SwDeviceProfile::enumerator`].
    pub enumerator: &'static str,
}

impl WinDsIdentity {
    pub(super) const fn dualsense() -> WinDsIdentity {
        WinDsIdentity {
            devtype: 0,
            instance_prefix: "pf_pad",
            // Hardware id, not the package name. The INF still matches `pf_dualsense`; renaming
            // this to `pf_gamepad` binds inbox `input.inf`/`HidUsb`, which cannot start on a
            // software-enumerated devnode. `hwid_matches_inf` pins it.
            hwid: "pf_dualsense",
            usb_vid_pid: "VID_054C&PID_0CE6",
            description: "Punktfunk Virtual DualSense",
            enumerator: "VID_054C&PID_0CE6&MI_03",
        }
    }

    pub(super) const fn dualsense_edge() -> WinDsIdentity {
        WinDsIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_DUALSENSE_EDGE,
            instance_prefix: "pf_edge",
            hwid: "pf_dualsenseedge",
            usb_vid_pid: "VID_054C&PID_0DF2",
            description: "Punktfunk Virtual DualSense Edge",
            enumerator: "VID_054C&PID_0DF2&MI_03",
        }
    }
}

impl DsWinPad {
    pub(super) fn open(index: u8, id: &WinDsIdentity) -> Result<DsWinPad> {
        let shm = ShmPad::open(
            index,
            id.devtype,
            &pf_driver_proto::dualsense::NEUTRAL_REPORT,
            &SwDeviceProfile {
                instance: &format!("{}_{index}", id.instance_prefix),
                container_tag: 0x5046_4453, // "PFDS"
                container_index: index,
                hwid: id.hwid,
                usb_vid_pid: Some(id.usb_vid_pid),
                // Composite USB devices: audio on interfaces 0-2, HID on 3. hidapi reads it back.
                usb_mi: Some(3),
                bluetooth: false,
                description: id.description,
                enumerator: id.enumerator,
                property: None,
            },
        )?;
        Ok(DsWinPad {
            shm,
            enc: DsEncoder::default(),
        })
    }

    pub(super) fn write_state(&mut self, st: &DsState) {
        let r = self.enc.encode(st);
        self.shm.publish(&r);
    }

    /// Drain the output plane oldest → newest so a stop-then-LED burst yields both, never just
    /// the latest report.
    pub(super) fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
        fb.resync = self
            .shm
            .poll(|bytes, _| parse_ds_output(pad, bytes, &mut fb));
        self.enc.observe(&fb.hidout);
        fb
    }
}

/// Windows DualSense [`PadProto`]: sealed-channel open, the same [`DsState`] mappers as
/// `linux/dualsense.rs`, section feedback poll. Lifecycle lives in [`UhidManager`].
pub struct DsWinProto {
    /// Steam back grips have no DualSense HID slot. `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for DsWinProto {
    fn default() -> DsWinProto {
        DsWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for DsWinProto {
    type Pad = DsWinPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense/Windows";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DsWinPad> {
        let p = DsWinPad::open(idx, &WinDsIdentity::dualsense())?;
        tracing::info!(
            index = idx,
            "virtual DualSense created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DsWinPad, st: &DsState) {
        pad.write_state(st);
    }

    fn service(&self, pad: &mut DsWinPad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            // Only a report that asserted vibration counts — an LED/trigger stream must not feed
            // the abandoned-rumble force-off clock.
            rumble_drove: Some(fb.rumble.is_some()),
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            resync: fb.resync,
        }
    }
}

/// Hold a software-devnode HID Steam Deck (`device_type = 3`, `VID_28DE&PID_1205`) for `secs`,
/// streaming the neutral Deck frame. Wired to `deck-windows-spike`; never used by a session.
/// Watch Steam's `logs/controller.txt` / controller settings: does Steam Input promote a
/// software-devnode HID Deck, or does it require a real USB bus identity?
pub fn deck_spike_hold(index: u8, secs: u64) -> Result<()> {
    let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
    let mut channel = PadChannel::create(boot_name, SHM_SIZE)?;
    // Ring version 0: the spike reads only the legacy output slot.
    stamp(
        channel.data(),
        pf_driver_proto::gamepad::DEVTYPE_STEAMDECK,
        index,
        0,
        &super::steam_proto::neutral_deck_report(),
    );
    let (_sw, spike_instance_id) = create_swdevice(&SwDeviceProfile {
        instance: &format!("pf_deckspike_{index}"),
        container_tag: 0x5046_4453, // "PFDS"
        container_index: index,
        hwid: super::steam_deck_windows::DECK_HWID,
        usb_vid_pid: Some("VID_28DE&PID_1205"),
        // hidapi parses MI_ from the child hwids; absent = interface 0, Steam wants 2.
        usb_mi: Some(2),
        bluetooth: false,
        description: "Punktfunk Virtual Steam Deck (spike)",
        enumerator: "punktfunk",
        property: None,
    })?;
    // Same devnode-proved delivery as a session pad — a bring-up tool must not fall back
    // to the mailbox.
    channel.bind_devnode(
        index as u32,
        spike_instance_id,
        ProofTransport::HidFeatureReport,
    );
    channel.deliver_eager(std::time::Duration::from_millis(1500));
    println!(
        "virtual Steam Deck devnode up (28DE:1205, device_type 3) — holding {secs}s.\n\
         Observe: Get-PnpDevice -PresentOnly | findstr 1205; Steam logs\\controller.txt for a\n\
         detect/promote line; Steam Settings > Controller for a 'Steam Deck' entry.\n\
         GO = Steam lists/promotes it; NO-GO = it never appears (the Linux `Interface: -1` gap\n\
         applies verbatim — document and keep the SteamDeck->DualSense Windows fold)."
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last_out_seq = 0u32;
    while std::time::Instant::now() < deadline {
        channel.pump();
        let seq = channel.data().load_u32(OFF_OUT_SEQ, Ordering::Relaxed);
        if seq != last_out_seq {
            last_out_seq = seq;
            let mut out = [0u8; 16];
            channel.data().read_bytes(OFF_OUTPUT, &mut out);
            println!("  output report from a client (Steam?): {out:02x?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("deck-windows-spike: done (devnode removed on exit)");
    Ok(())
}

/// Session DualSense pads — Windows analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager). Heartbeat keeps the section fresh
/// (the driver's timer streams whatever is in it).
pub type DualSenseWindowsManager = UhidManager<DsWinProto>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every hwid the host puts on a pad devnode must be one the shipped INF declares. Otherwise
    /// PnP falls through to the synthesized USB ids and binds inbox `input.inf`/`HidUsb`, which
    /// cannot start on a software-enumerated devnode. Hardware ids outlive package renames.
    #[test]
    fn hwid_matches_inf() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packaging/windows/drivers/pf-gamepad/pf_gamepad.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_gamepad.inx");
        // Match the install section by prefix, not `pfGamepad,` — Xbox installs `pfGamepadXbox`
        // (xinputhid filter) and PlayStation/Deck must not. An exact match went vacuous at that
        // split.
        let declared: Vec<String> = inf
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';'))
            .filter_map(|l| l.split_once('='))
            .filter(|(_, rhs)| rhs.trim_start().starts_with("pfGamepad"))
            .flat_map(|(_, rhs)| {
                // `pfGamepad[Suffix], <hwid>[, <hwid>…]` — drop the section name. `AddReg=` lines
                // have no comma and contribute nothing.
                rhs.split(',')
                    .skip(1)
                    .map(|id| id.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            declared.len() >= 4,
            "parsed {} hardware ids out of {inx} — the [Models] shape changed and this test went \
             vacuous; fix the parse rather than deleting the assert",
            declared.len()
        );
        for hwid in [
            WinDsIdentity::dualsense().hwid,
            WinDsIdentity::dualsense_edge().hwid,
            super::super::dualshock4_windows::DS4_HWID,
            super::super::steam_deck_windows::DECK_HWID,
            super::super::triton_windows::TRITON_HWID,
            super::super::switch_pro_windows::SWITCH_HWID,
            super::super::switch_pro_windows::JOYCON_LEFT_HWID,
            super::super::switch_pro_windows::JOYCON_RIGHT_HWID,
            super::super::eightbitdo_windows::ULTIMATE2_HWID,
            super::super::eightbitdo_windows::PRO2_HWID,
            super::super::eightbitdo_windows::PRO3_HWID,
            super::super::hori_windows::HORI_HWID,
        ]
        .into_iter()
        // Every Xbox identity, not just the first — a new one without its INF model line never starts.
        .chain(
            super::super::xbox_windows::XBOX_IDENTITIES
                .iter()
                .map(|i| i.hwid),
        )
        // The unfiltered Xbox line the host falls back to where `xinputhid` is not a service.
        .chain(std::iter::once(
            super::super::xbox_windows::XBOX_UNFILTERED_HWID,
        )) {
            let want = hwid.to_ascii_lowercase();
            let rooted = format!("root\\{want}");
            assert!(
                declared
                    .iter()
                    .any(|d| d.as_str() == want || d.as_str() == rooted),
                "the host creates pad devnodes with hardware id {hwid:?}, which pf_gamepad.inx \
                 does not declare (it has {declared:?}) — PnP would bind inbox input.inf/HidUsb \
                 instead and the pad would never start"
            );
        }
    }

    /// Xbox identities install `pfGamepadXbox` (xinputhid upper filter + `BusDevice`); PlayStation
    /// and Deck must not. That filter claims the HID collection exclusively — a DualSense handed
    /// to Microsoft's Xbox translator disappears from Steam and SDL. A new Xbox identity pasted
    /// onto `pfGamepad` enumerates and is never promoted.
    #[test]
    fn only_the_xbox_identity_installs_the_xinputhid_section() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packaging/windows/drivers/pf-gamepad/pf_gamepad.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_gamepad.inx");
        let xbox: Vec<String> = super::super::xbox_windows::XBOX_IDENTITIES
            .iter()
            .map(|i| i.hwid.to_ascii_lowercase())
            .collect();

        let mut seen: Vec<&str> = Vec::new();
        for line in inf.lines().map(str::trim).filter(|l| !l.starts_with(';')) {
            let Some((_, rhs)) = line.split_once('=') else {
                continue;
            };
            let rhs = rhs.trim_start();
            let Some((section, ids)) = rhs.split_once(',') else {
                continue;
            };
            if !section.starts_with("pfGamepad") {
                continue;
            }
            let ids: Vec<String> = ids
                .split(',')
                .map(|i| i.trim().to_ascii_lowercase())
                .collect();
            // `contains`, not `==`: model lines carry the bare id and its `root\` twin.
            let matched: Vec<&str> = xbox
                .iter()
                .filter(|x| ids.iter().any(|i| i.contains(x.as_str())))
                .map(String::as_str)
                .collect();
            if matched.is_empty() {
                assert_eq!(
                    section, "pfGamepad",
                    "a non-Xbox model line ({ids:?}) installs {section:?}; if that section carries \
                     the xinputhid filter, this pad is about to be handed to Microsoft's Xbox \
                     translator"
                );
            } else {
                seen.extend(matched);
                assert_ne!(
                    section, "pfGamepad",
                    "an Xbox model line ({ids:?}) installs the SHARED section, so either the \
                     xinputhid filter would be attached to every PlayStation and Deck pad too, or \
                     this Xbox pad silently never gets promoted"
                );
            }
        }
        for want in &xbox {
            assert!(
                seen.contains(&want.as_str()),
                "no [Models] line mentions {want:?} — either the identity has no INF line at all, \
                 or the parse went vacuous; fix that rather than deleting the assert"
            );
        }
    }

    /// The driver picks HID identity from the hardware id at `EvtDeviceAdd`, before the sealed
    /// channel exists, through `pf_driver_proto::gamepad::devtype_from_hwids`. Every host hwid
    /// must name the device_type the host stamps; a Deck frame parsed as DualSense `0x01` pins
    /// the left stick and holds d-pad UP.
    #[test]
    fn hwid_devtype_table_matches_the_driver() {
        for (hwid, devtype) in [
            (WinDsIdentity::dualsense().hwid, 0),
            (
                WinDsIdentity::dualsense_edge().hwid,
                pf_driver_proto::gamepad::DEVTYPE_DUALSENSE_EDGE,
            ),
            (
                super::super::dualshock4_windows::DS4_HWID,
                pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4,
            ),
            (
                super::super::steam_deck_windows::DECK_HWID,
                pf_driver_proto::gamepad::DEVTYPE_STEAMDECK,
            ),
            (
                super::super::triton_windows::TRITON_HWID,
                pf_driver_proto::gamepad::DEVTYPE_TRITON,
            ),
            (
                super::super::switch_pro_windows::SWITCH_HWID,
                pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO,
            ),
            (
                super::super::switch_pro_windows::JOYCON_LEFT_HWID,
                pf_driver_proto::gamepad::DEVTYPE_JOYCON_LEFT,
            ),
            (
                super::super::switch_pro_windows::JOYCON_RIGHT_HWID,
                pf_driver_proto::gamepad::DEVTYPE_JOYCON_RIGHT,
            ),
            (
                super::super::eightbitdo_windows::ULTIMATE2_HWID,
                pf_driver_proto::gamepad::DEVTYPE_8BITDO_ULTIMATE2,
            ),
            (
                super::super::eightbitdo_windows::PRO2_HWID,
                pf_driver_proto::gamepad::DEVTYPE_8BITDO_PRO2,
            ),
            (
                super::super::eightbitdo_windows::PRO3_HWID,
                pf_driver_proto::gamepad::DEVTYPE_8BITDO_PRO3,
            ),
            (
                super::super::hori_windows::HORI_HWID,
                pf_driver_proto::gamepad::DEVTYPE_HORIPAD_STEAM,
            ),
            // Server's unfiltered Xbox line: any Xbox type, so the pad never enumerates as a
            // DualSense while hidclass asks; the section sets the real one on attach.
            (
                super::super::xbox_windows::XBOX_UNFILTERED_HWID,
                pf_driver_proto::gamepad::DEVTYPE_XBOX,
            ),
        ]
        .into_iter()
        // Xbox identities share a report descriptor, so a hwid→devtype slip is the wrong PID, not
        // a mangled report (an Elite that Steam maps as a Series X|S pad).
        .chain(
            super::super::xbox_windows::XBOX_IDENTITIES
                .iter()
                .map(|i| (i.hwid, i.devtype)),
        ) {
            let got = pf_driver_proto::gamepad::devtype_from_hwids(&hwid.to_ascii_lowercase());
            assert_eq!(
                got,
                Some(devtype),
                "the host stamps device_type={devtype} for hardware id {hwid:?}, but the driver's \
                 table says {got:?} — the pad would enumerate with another controller's report \
                 descriptor"
            );
        }
    }
}
