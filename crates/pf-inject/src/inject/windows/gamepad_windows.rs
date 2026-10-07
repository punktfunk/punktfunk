//! Windows virtual Xbox 360 pad via the XUSB companion UMDF driver
//! (`packaging/windows/drivers/pf-xusb`). One pad per client index, visible to classic
//! `XInputGetState` with no kernel bus: `SwDeviceCreate` a `pf_xusb_<index>` devnode
//! (the driver registers `GUID_DEVINTERFACE_XUSB`) and push XInput state into an unnamed
//! DATA section over the sealed channel ([`PadChannel`] — handle duplicated into WUDFHost,
//! bootstrapped via `Global\pfxusb-boot-<index>`; `design/gamepad-channel-sealing.md`).
//! GameStream/Moonlight already speak XInput (low-16 buttons, sticks −32768..32767 +Y up,
//! triggers 0..255), so the copy is ~1:1.
//!
//! Rumble is the reverse path: `XInputSetState` → driver `SET_STATE` into the section →
//! [`GamepadManager::pump_rumble`] onto the 0xCA plane, matching Linux `EV_FF`. Slots, unplug
//! and the abandoned-rumble force-off are [`UhidManager`]'s.

use super::gamepad_raii::{
    create_swdevice, DriverAttach, PadChannel, ProofTransport, SwDevice, SwDeviceProfile,
};
use super::xbox_proto::XboxState;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;
use std::sync::atomic::{fence, Ordering};
use std::time::Duration;

// Driver maps this same struct; `offset_of!` so a layout change is a compile error.
use pf_driver_proto::gamepad::XusbShm;
const SHM_SIZE: usize = core::mem::size_of::<XusbShm>();
const SHM_MAGIC: u32 = pf_driver_proto::gamepad::XUSB_MAGIC; // "PFXU"
const OFF_PACKET: usize = core::mem::offset_of!(XusbShm, packet);
const OFF_BUTTONS: usize = core::mem::offset_of!(XusbShm, buttons);
const OFF_LT: usize = core::mem::offset_of!(XusbShm, left_trigger);
const OFF_RT: usize = core::mem::offset_of!(XusbShm, right_trigger);
const OFF_LX: usize = core::mem::offset_of!(XusbShm, thumb_lx);
const OFF_LY: usize = core::mem::offset_of!(XusbShm, thumb_ly);
const OFF_RX: usize = core::mem::offset_of!(XusbShm, thumb_rx);
const OFF_RY: usize = core::mem::offset_of!(XusbShm, thumb_ry);
const OFF_RUMBLE_SEQ: usize = core::mem::offset_of!(XusbShm, rumble_seq);
const OFF_RUMBLE: usize = core::mem::offset_of!(XusbShm, rumble_large); // large @28, small @29
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(XusbShm, driver_proto);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(XusbShm, pad_index);
const OFF_MAGIC: usize = core::mem::offset_of!(XusbShm, magic);

/// INF hardware ids. `pf_xusb` installs the `xinputhid` UpperFilters string WGI admits on;
/// PnP fails a devnode whose filter service is missing, so without it the pad takes the
/// filter-free line and XInput alone sees it.
const XUSB_HWID: &str = "pf_xusb";
const XUSB_UNFILTERED_HWID: &str = "pf_xusb_nofilter";

fn xusb_hwid() -> &'static str {
    if super::xbox_windows::xinputhid_registered() {
        XUSB_HWID
    } else {
        XUSB_UNFILTERED_HWID
    }
}

/// One virtual Xbox 360 pad: `pf_xusb_<index>` plus the sealed `XusbShm` channel. `pub`
/// because it is `PadProto::Pad`.
pub struct XusbWinPad {
    _sw: SwDevice,
    channel: PadChannel,
    attach: DriverAttach,
    packet: u32,
    last_rumble_seq: u32,
}

impl XusbWinPad {
    /// Unnamed DATA + `Global\pfxusb-boot-<index>` mailbox. Stamp pad index, then magic LAST
    /// (the driver accepts the section only once magic is set).
    fn open(index: u8) -> Result<XusbWinPad> {
        let boot_name = pf_driver_proto::gamepad::xusb_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        // Index first; magic LAST. The driver rejects the section until magic is set.
        let shm = channel.data();
        shm.store_u32(OFF_PAD_INDEX, index.into(), Ordering::Relaxed);
        shm.store_u32(OFF_MAGIC, SHM_MAGIC, Ordering::Relaxed);
        // `?` so PadSlots retries; a swallowed failure latched a phantom pad for the session.
        let hwid = xusb_hwid();
        let (sw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &format!("pf_xusb_{index}"),
            container_tag: 0x5046_5855, // "PFXU"
            container_index: index,
            hwid,
            // XInput finds the device by `GUID_DEVINTERFACE_XUSB`, not VID/PID.
            usb_vid_pid: None,
            usb_mi: None,
            bluetooth: false,
            description: "Punktfunk Virtual Xbox 360 (XUSB)",
            enumerator: "punktfunk",
            property: None,
        })?;
        channel.bind_devnode(index as u32, instance_id.clone(), ProofTransport::XusbIoctl);
        // 1500 ms: EvtDeviceAdd publishes the pid immediately; miss and `service` keeps pumping.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(XusbWinPad {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                hwid,
                "pf_xusb.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pfxusb-driver.log",
                boot_name,
                instance_id,
            ),
            packet: 0,
            last_rumble_seq: 0,
        })
    }

    /// Write XInput state (low-16 buttons); `packet` last so XInput sees a coherent snapshot.
    fn write_state(&mut self, st: &XboxState) {
        self.packet = self.packet.wrapping_add(1);
        let shm = self.channel.data();
        shm.write_bytes(OFF_BUTTONS, &(st.buttons as u16).to_ne_bytes());
        shm.write_bytes(OFF_LT, &[st.left_trigger]);
        shm.write_bytes(OFF_RT, &[st.right_trigger]);
        shm.write_bytes(OFF_LX, &st.ls_x.to_ne_bytes());
        shm.write_bytes(OFF_LY, &st.ls_y.to_ne_bytes());
        shm.write_bytes(OFF_RX, &st.rs_x.to_ne_bytes());
        shm.write_bytes(OFF_RY, &st.rs_y.to_ne_bytes());
        // `packet` LAST: `Release` fence then `Release` store, so an `Acquire` load never sees a
        // torn body on ARM64 (x86-TSO: plain stores).
        fence(Ordering::Release);
        shm.store_u32(OFF_PACKET, self.packet, Ordering::Release);
    }

    /// New rumble `(large, small)` if `rumble_seq` moved. Also pumps handle delivery and attach.
    fn service(&mut self) -> Option<(u8, u8)> {
        self.channel.pump();
        let shm = self.channel.data();
        self.attach
            .observe(shm.load_u32(OFF_DRIVER_PROTO, Ordering::Relaxed));
        // The driver bumps `rumble_seq` AFTER writing the rumble bytes, so this Acquire load
        // orders the byte reads below after it: a fresh seq means a coherent snapshot on ARM64.
        let seq = shm.load_u32(OFF_RUMBLE_SEQ, Ordering::Acquire);
        if seq == self.last_rumble_seq {
            return None;
        }
        self.last_rumble_seq = seq;
        let mut rumble = [0u8; 2];
        shm.read_bytes(OFF_RUMBLE, &mut rumble);
        Some((rumble[0], rumble[1]))
    }
}

/// Windows XUSB [`PadProto`]: frame-only state, no rich plane, no hidout. Rumble arrives as
/// `SET_STATE` (`XINPUT_VIBRATION`: two motors, no impulse triggers).
#[derive(Default)]
pub struct XusbWinProto;

impl PadProto for XusbWinProto {
    type Pad = XusbWinPad;
    type State = XboxState;
    const LABEL: &'static str = "Xbox 360/Windows";
    const DEVICE: &'static str = "Xbox 360";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<XusbWinPad> {
        let p = XusbWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Xbox 360 created (Windows XUSB companion)"
        );
        Ok(p)
    }

    fn merge_frame(&self, _prev: &XboxState, f: &GamepadFrame) -> XboxState {
        XboxState::from_frame(f)
    }

    /// XInput has no rich plane.
    fn apply_rich(&self, _st: &mut XboxState, _rich: RichInput) {}

    fn write_state(&self, pad: &mut XusbWinPad, st: &XboxState) {
        pad.write_state(st);
    }

    /// Motors are 0..255 and the wire 0..65535, so ×257; `large` → `low`, `small` → `high`.
    /// A moved `rumble_seq` is the game driving the plane, even at an unchanged level.
    fn service(&self, pad: &mut XusbWinPad, _idx: u8) -> PadFeedback {
        let r = pad.service();
        PadFeedback {
            rumble: r.map(|(large, small)| (large as u16 * 257, small as u16 * 257, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(r.is_some()),
            resync: false,
        }
    }
}

/// Session Xbox 360 pads — Windows analogue of Linux uinput-xpad.
pub type GamepadManager = UhidManager<XusbWinProto>;

impl UhidManager<XusbWinProto> {
    /// Relay changed rumble. XInput has no rich-feedback plane, so there is no hidout sink.
    pub fn pump_rumble(&mut self, send: impl FnMut(u16, u16, u16, u16, u16)) {
        self.pump(send, |_| {});
    }
}

#[cfg(test)]
mod tests {
    /// Both XUSB ids have a model line, and only `pf_xusb`'s install writes the `xinputhid`
    /// UpperFilters string: on a machine without that service it fails the devnode.
    #[test]
    fn xusb_hwids_match_inf() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/windows/drivers/pf-xusb/pf_xusb.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_xusb.inx");
        let lines: Vec<&str> = inf
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';'))
            .collect();
        let section_for = |hwid: &str| {
            lines.iter().find_map(|l| {
                let (section, ids) = l.split_once('=')?.1.split_once(',')?;
                ids.split(',')
                    .any(|i| i.trim().eq_ignore_ascii_case(hwid))
                    .then(|| section.trim().to_string())
            })
        };
        let hw_block = |section: &str| -> Vec<&str> {
            let head = format!("[{section}.NT.HW]").to_ascii_lowercase();
            lines
                .iter()
                .skip_while(|l| l.to_ascii_lowercase() != head)
                .skip(1)
                .take_while(|l| !l.starts_with('['))
                .copied()
                .collect()
        };
        let filtered = section_for(super::XUSB_HWID).expect("pf_xusb model line");
        let plain = section_for(super::XUSB_UNFILTERED_HWID).expect("pf_xusb_nofilter model line");
        assert!(
            hw_block(&filtered).iter().any(|l| l.starts_with("AddReg=")),
            "{filtered} lost the xinputhid AddReg WGI admits the pad on"
        );
        let plain_hw = hw_block(&plain);
        assert!(
            !plain_hw.is_empty() && !plain_hw.iter().any(|l| l.starts_with("AddReg=")),
            "{plain} must install without the xinputhid UpperFilters string"
        );
    }
}
