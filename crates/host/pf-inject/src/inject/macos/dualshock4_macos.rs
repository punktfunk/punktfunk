//! Virtual DualShock 4 (`054C:09CC`) on macOS, over [`HidUserDevice`]. GameController binds it
//! as `GCDualShockGamepad`, SDL's raw-HID driver as `ps4`. The transport stays `Virtual`.
//!
//! Codec, descriptor and feature blobs live in [`super::dualshock4_proto`]; this file is the
//! identity and its feature replies.

use super::dualsense_proto::DsState;
use super::dualshock4_proto::{
    ds4_pairing_reply, parse_ds4_output, Ds4Encoder, Ds4Feedback, DS4_FEATURE_CALIBRATION,
    DS4_FEATURE_FIRMWARE, DS4_RDESC, DS4_TOUCH_H, DS4_TOUCH_W,
};
use crate::hid_user_device::{HidUserDevice, Identity};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::{HidOutput, RichInput};

/// The identity of pad `index`: feature replies `0x12` (pairing MAC from `index`), `0x02` and
/// `0xA3`.
pub(crate) fn identity(index: u8) -> Identity {
    Identity {
        features: vec![
            ds4_pairing_reply(index).to_vec(),
            DS4_FEATURE_CALIBRATION.to_vec(),
            DS4_FEATURE_FIRMWARE.to_vec(),
        ],
        ..Identity::of(
            DEVTYPE_DUALSHOCK4,
            format!("punktfunk-ds4-{index}"),
            DS4_RDESC,
        )
    }
}

/// One virtual DualShock 4. Drop removes it.
pub struct DualShock4MacPad {
    dev: HidUserDevice,
    enc: Ds4Encoder,
}

pub struct Ds4MacProto {
    /// Steam back-grip fold: a DualShock 4 has no paddle slot.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4MacProto {
    fn default() -> Ds4MacProto {
        Ds4MacProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4MacProto {
    type Pad = DualShock4MacPad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4/macOS";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str = super::hid_user_device::CREATE_HINT;

    fn open(&mut self, idx: u8) -> Result<DualShock4MacPad> {
        let dev = HidUserDevice::open(&identity(idx))?;
        tracing::info!(index = idx, "virtual DualShock 4 created (IOHIDUserDevice)");
        Ok(DualShock4MacPad {
            dev,
            enc: Ds4Encoder::default(),
        })
    }

    fn alive(&self, pad: &DualShock4MacPad) -> bool {
        pad.dev.alive()
    }

    fn merge_frame(&self, prev: &DsState, f: &GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DualShock4MacPad, st: &DsState) {
        let r = pad.enc.encode(st);
        let _ = pad.dev.write_input(&r);
    }

    /// Output report `0x05`: rumble on 0xCA, lightbar as 0xCD `Led`.
    fn service(&self, pad: &mut DualShock4MacPad, idx: u8) -> PadFeedback {
        let mut fb = Ds4Feedback::default();
        pad.dev.poll(|_, data| parse_ds4_output(data, &mut fb));
        PadFeedback {
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// Session DualShock 4 pads.
pub type DualShock4MacManager = UhidManager<Ds4MacProto>;

#[cfg(test)]
mod tests {
    /// Every feature report a reader asks at bind has a reply led by its id.
    #[test]
    fn the_bind_features_answer() {
        let ids: Vec<u8> = super::identity(0).features.iter().map(|f| f[0]).collect();
        assert_eq!(ids, [0x12, 0x02, 0xA3]);
        assert_ne!(
            super::identity(0).features[0],
            super::identity(1).features[0]
        );
    }
}
