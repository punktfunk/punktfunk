//! Virtual DualSense and DualSense Edge on macOS, over [`HidUserDevice`]. GameController binds
//! both as `GCDualSenseGamepad`, SDL's raw-HID driver as `ps5`. The transport stays `Virtual`:
//! as Bluetooth the readers would expect the Bluetooth report layout.
//!
//! Codec, descriptors and feature blobs live in [`super::dualsense_proto`]; this file is the
//! identity and its feature replies.

use super::dualsense_proto::{
    ds_pairing_reply, edge_paddle_bits, parse_ds_output, DsEncoder, DsFeedback, DsState,
    DEVTYPE_DUALSENSE, DEVTYPE_DUALSENSE_EDGE, DS_FEATURE_CALIBRATION, DS_FEATURE_FIRMWARE,
    DS_TOUCH_H, DS_TOUCH_W, DUALSENSE_EDGE_RDESC, DUALSENSE_RDESC,
};
use crate::hid_user_device::{HidUserDevice, Identity};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;

/// The identity of pad `index`: feature replies `0x05`, `0x09` and `0x20`. The pairing MAC
/// derives from `index`, so two pads never share one.
pub(crate) fn identity(devtype: u8, index: u8) -> Identity {
    let (rdesc, slug) = if devtype == DEVTYPE_DUALSENSE_EDGE {
        (DUALSENSE_EDGE_RDESC, "dsedge")
    } else {
        (DUALSENSE_RDESC, "ds")
    };
    Identity {
        features: vec![
            DS_FEATURE_CALIBRATION.to_vec(),
            ds_pairing_reply(devtype, index).to_vec(),
            DS_FEATURE_FIRMWARE.to_vec(),
        ],
        ..Identity::of(devtype, format!("punktfunk-{slug}-{index}"), rdesc)
    }
}

/// One virtual DualSense. Drop removes it.
pub struct DualSenseMacPad {
    dev: HidUserDevice,
    enc: DsEncoder,
}

/// DualSense or Edge: one codec; the Edge differs in PID, descriptor and paddles in
/// `buttons[2]`.
pub struct DsMacProto {
    devtype: u8,
    /// Steam back-grip fold for the plain DualSense, which has no paddle slot.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for DsMacProto {
    fn default() -> DsMacProto {
        DsMacProto {
            devtype: DEVTYPE_DUALSENSE,
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl DsMacProto {
    /// `054C:0DF2`: `UhidManager::with_backend(DsMacProto::edge())`.
    pub fn edge() -> DsMacProto {
        DsMacProto {
            devtype: DEVTYPE_DUALSENSE_EDGE,
            ..DsMacProto::default()
        }
    }
}

impl PadProto for DsMacProto {
    type Pad = DualSenseMacPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense/macOS";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str = super::hid_user_device::CREATE_HINT;

    fn open(&mut self, idx: u8) -> Result<DualSenseMacPad> {
        let dev = HidUserDevice::open(&identity(self.devtype, idx))?;
        tracing::info!(
            index = idx,
            edge = self.devtype == DEVTYPE_DUALSENSE_EDGE,
            "virtual DualSense created (IOHIDUserDevice)"
        );
        Ok(DualSenseMacPad {
            dev,
            enc: DsEncoder::default(),
        })
    }

    fn alive(&self, pad: &DualSenseMacPad) -> bool {
        pad.dev.alive()
    }

    /// The Edge puts paddles on `buttons[2]`, rebuilt every frame; the DualSense folds them.
    fn merge_frame(&self, prev: &DsState, f: &GamepadFrame) -> DsState {
        if self.devtype == DEVTYPE_DUALSENSE_EDGE {
            let mut s = DsState::merge_frame(prev, f, f.buttons);
            s.buttons[2] |= edge_paddle_bits(f.buttons);
            return s;
        }
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DualSenseMacPad, st: &DsState) {
        let r = pad.enc.encode(st);
        let _ = pad.dev.write_input(&r);
    }

    /// Output report `0x02`: rumble on 0xCA, lightbar, player LEDs and triggers on 0xCD.
    fn service(&self, pad: &mut DualSenseMacPad, idx: u8) -> PadFeedback {
        let mut fb = DsFeedback::default();
        pad.dev.poll(|_, data| parse_ds_output(idx, data, &mut fb));
        pad.enc.observe(&fb.hidout);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// Session DualSense pads; `with_backend(DsMacProto::edge())` for the Edge.
pub type DualSenseMacManager = UhidManager<DsMacProto>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every feature report macOS asks at bind has a reply led by its id, and two pads differ
    /// in their pairing MAC.
    #[test]
    fn the_bind_features_answer_and_pads_differ_in_mac() {
        for devtype in [DEVTYPE_DUALSENSE, DEVTYPE_DUALSENSE_EDGE] {
            let id = identity(devtype, 0);
            let ids: Vec<u8> = id.features.iter().map(|f| f[0]).collect();
            assert_eq!(ids, [0x05, 0x09, 0x20]);
            assert_ne!(id.features[1], identity(devtype, 1).features[1]);
        }
        assert_eq!(identity(DEVTYPE_DUALSENSE_EDGE, 0).product, 0x0DF2);
    }
}
