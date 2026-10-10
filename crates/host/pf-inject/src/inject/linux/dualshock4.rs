//! Virtual DualShock 4 via `/dev/uhid`.
//!
//! `hid-playstation` binds VID `054C` / PID `09CC` (Linux ≥ 6.2). Input is report
//! `0x01`; OUTPUT `0x05` is rumble (0xCA) and lightbar (`HidOutput::Led`, 0xCD).
//! There are no adaptive triggers, player LEDs, or mute.
//!
//! Codec, report descriptor, feature blobs, and GET_REPORT answers live in
//! [`super::dualshock4_proto`] (shared with the Windows UMDF backend). This file is
//! the UHID transport and the handshake. Pin with the tests here and
//! `crates/host/pf-inject/tests/motion_contract.rs`.

use super::dualsense_proto::DsState;
use super::dualshock4_proto::{
    ds4_pairing_reply, parse_ds4_output, Ds4Encoder, Ds4Feedback, DS4_FEATURE_CALIBRATION,
    DS4_FEATURE_FIRMWARE, DS4_PRODUCT, DS4_RDESC, DS4_TOUCH_H, DS4_TOUCH_W, DS4_VENDOR,
};
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};

/// Drop unbinds `hid-playstation`.
pub struct DualShock4Pad {
    dev: UhidDevice,
    enc: Ds4Encoder,
}

impl DualShock4Pad {
    /// `index` is only the name/uniq suffix, not a HID slot. The uniq is cosmetic;
    /// `hid-playstation` keys uniqueness off the pairing-report MAC.
    pub fn open(index: u8) -> Result<DualShock4Pad> {
        let dev = UhidDevice::open(&Create2 {
            bus: crate::uhid_abi::BUS_USB,
            name: &format!("Punktfunk DualShock 4 {index}"),
            phys: &format!("punktfunk/dualshock4/{index}"),
            uniq: &format!("punktfunk-ds4-{index}"),
            rdesc: DS4_RDESC,
            vendor: DS4_VENDOR as u32,
            product: DS4_PRODUCT as u32,
            version: 0x0100,
        })?;
        Ok(DualShock4Pad {
            dev,
            enc: Ds4Encoder::default(),
        })
    }

    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        let r = self.enc.encode(st);
        self.dev.write_input(&r)
    }

    /// Pairing GET_REPORT (`0x12`) must be answered during `hid-playstation` bind or no
    /// input nodes appear. Call right after [`open`](Self::open). DS4 feedback is OUTPUT,
    /// so a SET_REPORT needs only the ack `poll` sends.
    pub fn service(&mut self, pad: u8) -> Ds4Feedback {
        let mut fb = Ds4Feedback::default();
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => parse_ds4_output(data, &mut fb),
            UhidEvent::GetReport { id, rnum } => {
                let pairing = ds4_pairing_reply(pad);
                let data: Option<&[u8]> = match rnum {
                    0x12 => Some(&pairing),
                    0x02 => Some(DS4_FEATURE_CALIBRATION),
                    0xA3 => Some(DS4_FEATURE_FIRMWARE),
                    _ => None,
                };
                let _ = dev.reply_get_report(id, data);
            }
            UhidEvent::SetReport(_) => {}
        });
        fb
    }
}

/// Slot table, heartbeat, and `HidoutDedup` live in [`UhidManager`]. The kernel restamps
/// the lightbar on every OUTPUT (including rumble-only); `Led` is compared to the last
/// forwarded value and re-armed on create/unplug.
pub struct Ds4LinuxProto {
    /// Steam back-grip fold. DS4 has no paddle HID slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4LinuxProto {
    fn default() -> Ds4LinuxProto {
        Ds4LinuxProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4LinuxProto {
    type Pad = DualShock4Pad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualShock4Pad> {
        let p = DualShock4Pad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (UHID hid-playstation)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    /// Steam dual pads split the one touchpad left/right; clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DualShock4Pad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Rumble on 0xCA, lightbar as 0xCD `Led`. No player LEDs or adaptive triggers.
    fn service(&self, pad: &mut DualShock4Pad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            // Arms abandoned-rumble force-off. `parse_ds4_output` sets rumble only when flag0 bit0 is on.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// `PUNKTFUNK_GAMEPAD=ps4`. [`UhidManager`] heartbeats report `0x01` through input silence;
/// `hid-playstation`/SDL treat a multi-second gap as unplug.
pub type DualShock4Manager = UhidManager<Ds4LinuxProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dualshock4_proto::DS4_FEATURE_PAIRING;

    // Codec tests live in `dualshock4_proto`. This module pins UHID-side feature-report shapes.

    #[test]
    fn feature_report_shapes() {
        assert_eq!(DS4_FEATURE_PAIRING.len(), 16);
        assert_eq!(DS4_FEATURE_PAIRING[0], 0x12);
        assert_eq!(DS4_FEATURE_CALIBRATION.len(), 37);
        assert_eq!(DS4_FEATURE_CALIBRATION[0], 0x02);
        assert_eq!(DS4_FEATURE_FIRMWARE.len(), 49);
        assert_eq!(DS4_FEATURE_FIRMWARE[0], 0xA3);
    }
}
