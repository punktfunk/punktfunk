//! Virtual DualShock 4 on Windows via the UMDF minidriver. Same sealed channel
//! as [`super::dualsense_windows`], same [`DsState`]. Differs in PnP identity
//! (`VID_054C&PID_09CC`, `pf_dualshock4`) and codec ([`super::dualshock4_proto`]).
//!
//! Stamp `device_type = 1` before the section magic so hidclass binds DS4, not
//! DualSense. Feedback is rumble (0xCA) and lightbar (`Led`, 0xCD). Pin:
//! `dualsense_windows::tests::hwid_matches_inf`. Channel:
//! `design/gamepad-channel-sealing.md`.

use super::dualsense_proto::DsState;
use super::dualshock4_proto::{
    parse_ds4_output, Ds4Encoder, Ds4Feedback, DS4_TOUCH_H, DS4_TOUCH_W,
};
use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};

/// INF hardware id. A package rename must not change this (`hwid_matches_inf`).
pub(super) const DS4_HWID: &str = "pf_dualshock4";

/// Drop closes the `pf_ds4_<index>` devnode. `pub` because it is `PadProto::Pad`.
pub struct Ds4WinPad {
    shm: ShmPad,
    enc: Ds4Encoder,
}

impl Ds4WinPad {
    fn open(index: u8) -> Result<Ds4WinPad> {
        let shm = ShmPad::open(
            index,
            pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4,
            &pf_driver_proto::dualshock4::NEUTRAL_REPORT,
            &SwDeviceProfile {
                instance: &format!("pf_ds4_{index}"),
                container_tag: 0x5046_4453, // "PFDS"
                container_index: index,
                hwid: DS4_HWID,
                usb_vid_pid: Some("VID_054C&PID_09CC"),
                // Composite USB device (headset audio on 0-2); the HID interface is 3.
                usb_mi: Some(3),
                bluetooth: false,
                description: "Punktfunk Virtual DualShock 4",
                enumerator: "VID_054C&PID_09CC&MI_03",
                property: None,
            },
        )?;
        Ok(Ds4WinPad {
            shm,
            enc: Ds4Encoder::default(),
        })
    }

    fn write_state(&mut self, st: &DsState) {
        let r = self.enc.encode(st);
        self.shm.publish(&r);
    }

    /// Drain every new `0x05` oldest-first so a stop-then-LED burst keeps both.
    fn service(&mut self) -> Ds4Feedback {
        let mut fb = Ds4Feedback::default();
        fb.resync = self.shm.poll(|bytes, _| parse_ds4_output(bytes, &mut fb));
        fb
    }
}

/// Slot table, unplug, heartbeat, and `HidoutDedup` live in [`UhidManager`].
pub struct Ds4WinProto {
    /// Steam back-grip policy. DS4 has no paddle HID slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4WinProto {
    fn default() -> Ds4WinProto {
        Ds4WinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4WinProto {
    type Pad = Ds4WinPad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4/Windows";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<Ds4WinPad> {
        let p = Ds4WinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    /// Steam dual pads split one DS4 touchpad left/right; pad clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn write_state(&self, pad: &mut Ds4WinPad, st: &DsState) {
        pad.write_state(st);
    }

    /// Rumble on 0xCA, lightbar as 0xCD `Led`. No player LEDs or adaptive triggers.
    fn service(&self, pad: &mut Ds4WinPad, idx: u8) -> PadFeedback {
        let fb = pad.service();
        PadFeedback {
            // Trigger-motor slots are unused; see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            // Rumble-plane liveness; `parse_ds4_output` gates on flag0 bit0.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: fb.resync,
        }
    }
}

pub type DualShock4WindowsManager = UhidManager<Ds4WinProto>;
