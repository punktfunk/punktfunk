//! Virtual Steam Deck on Windows via the UMDF minidriver — analogue of the Linux
//! UHID Deck ([`super::steam_controller`]'s `SteamProto`). Shares
//! [`super::steam_proto`]: the `ID_CONTROLLER_DECK_STATE` serializer, the
//! XInput/rich mappers, the `0xEB` rumble parser.
//!
//! Transport is the sealed shared-memory channel plus a `SwDeviceCreate` devnode
//! (device-type 3). USB hardware ids must carry `&MI_02` (wired Deck controller
//! interface); hidclass mirrors that into the HID child as `bInterfaceNumber`,
//! and Steam Input claims on it. Missing `MI_` → hidapi reports interface 0.
//!
//! Steam writes rumble (`0xEB`) and trackpad haptics (`0x8F`) via SET_FEATURE;
//! the driver republishes them into the output slot (report-id-0 prefixed).
//! [`parse_steam_output`] reads the same wire shape as Linux. No gamepad-mode
//! entry pulse — that gate is Linux-evdev only.

use super::dualsense_windows::{
    create_swdevice, driver_marks, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::PadChannel;
use super::steam_proto::{
    neutral_deck_report, parse_steam_output, serialize_deck_state, SteamState, STEAM_REPORT_LEN,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;
use std::time::Duration;

/// INF hardware id. A package rename must not touch it
/// (`dualsense_windows::tests::hwid_matches_inf`).
pub(super) const DECK_HWID: &str = "pf_steamdeck";

/// One virtual Deck: `SwDeviceCreate`'d `pf_deck_<index>` plus the sealed
/// channel. Drop removes the devnode and both sections. `pub` because it is
/// `PadProto::Pad`.
pub struct DeckWinPad {
    /// Devnode RAII (`SwDeviceClose` on drop).
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: super::gamepad_raii::DriverAttach,
    seq: u32,
    /// v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Ring drain (v2.1+) or legacy latest-slot seq (old driver).
    drain: OutputDrain,
}

impl DeckWinPad {
    /// Stamp `device_type` first and magic last, then spawn `pf_deck_<index>`
    /// with the `MI_02` USB identity Steam's promotion gate requires.
    fn open(index: u8) -> Result<DeckWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = pf_driver_proto::gamepad::DEVTYPE_STEAMDECK;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability `2` = "this host drains the v2.2 long ring", stamped before the
            // magic so the driver sees it on attach (see the DualSense open path + PadShm docs).
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; STEAM_REPORT_LEN],
                neutral_deck_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_deck_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: DECK_HWID,
            usb_vid_pid: "VID_28DE&PID_1205",
            // Wired Deck controller interface. Without this the HID child has no MI_
            // token, hidapi reports interface 0, and Steam never claims the pad.
            usb_mi: Some(2),
            description: "Punktfunk Virtual Steam Deck",
            enumerator: "punktfunk",
        })?; // Propagate — swallowing latches the slot to a pad with no devnode.
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // Bind the DATA section to THIS devnode, not the pid the LocalService-writable
        // mailbox names.
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // The driver must read `device_type = 3` before hidclass asks for
        // descriptors, or the pad enumerates as DualSense.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(DeckWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_steamdeck",
                "pf_gamepad.inf", // one INF serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    fn write_state(&mut self, st: &SteamState) {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, st, self.seq);
        // SAFETY: `data_base()` points at a live PAD_SHM_SIZE-byte section and `r` is the 64-byte
        // Deck state frame.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    fn service(&mut self) -> (Option<(u16, u16)>, bool) {
        self.channel.pump();
        // SAFETY: the channel's section is live and SHM_SIZE bytes.
        let (proto, rev) = unsafe { driver_marks(self.channel.data_base()) };
        self.attach.observe_pad(proto, rev);
        let mut rumble = None;
        let base = self.channel.data_base();
        let resync = self.drain.drain(base, |bytes| {
            // Last rumble-carrying report wins. `0x8F` trackpad-haptic reports
            // carry none and must not clear it.
            if let Some(r) = parse_steam_output(bytes).rumble {
                rumble = Some(r);
            }
        });
        (rumble, resync)
    }
}

/// Windows Deck [`PadProto`]: sealed-channel open under the promoted identity,
/// same [`SteamState`] mappers as Linux. Slot table, unplug, heartbeat, and
/// rumble dedup live in [`UhidManager`].
#[derive(Default)]
pub struct DeckWinProto;

impl PadProto for DeckWinProto {
    type Pad = DeckWinPad;
    type State = SteamState;
    const LABEL: &'static str = "Steam Deck/Windows";
    const DEVICE: &'static str = "Steam Deck";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DeckWinPad> {
        let p = DeckWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Steam Deck created (Windows UMDF shm channel, MI_02 promoted identity)"
        );
        Ok(p)
    }

    fn neutral(&self) -> SteamState {
        SteamState::neutral()
    }

    /// Keep trackpads, motion, and pad clicks from `prev` — they arrive on a
    /// different plane and must survive a button-only frame.
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
        use super::steam_proto::btn;
        let mut s = SteamState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.rpad_x = prev.rpad_x;
        s.rpad_y = prev.rpad_y;
        s.lpad_x = prev.lpad_x;
        s.lpad_y = prev.lpad_y;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.buttons |= prev.buttons & (btn::RPAD_TOUCH | btn::LPAD_TOUCH);
        s.lpad_click = prev.lpad_click;
        s.rpad_click = prev.rpad_click;
        s
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn neutralize_gyro(&self, st: &mut SteamState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SteamState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DeckWinPad, st: &SteamState) {
        pad.write_state(st);
    }

    /// Rumble on the 0xCA plane. No lightbar / adaptive triggers, so `hidout`
    /// stays empty — same as Linux.
    fn service(&self, pad: &mut DeckWinPad, _idx: u8) -> PadFeedback {
        // `Some` means a rumble-carrying report landed, even at an unchanged
        // level — that is the rumble-plane activity signal.
        let (rumble, resync) = pad.service();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

pub type SteamDeckWindowsManager = UhidManager<DeckWinProto>;
