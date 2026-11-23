//! Virtual Xbox pads on macOS, over [`HidUserDevice`]: Xbox Wireless `0B13` (the macOS default
//! pad), One S `02FD` and Elite Series 2 `0B22`, the Bluetooth identities GameController allows
//! virtual. Each sets transport `Bluetooth`, or SDL reads none of them.
//!
//! Codec: [`super::xbox_proto`]. Descriptors: `pf_driver_proto::xbox`. No rich plane.

use super::xbox_proto::{parse_xbox_output, serialize_xbox_state, XboxState};
use crate::hid_user_device::{HidUserDevice, Identity};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::{DEVTYPE_XBOX, DEVTYPE_XBOX_ELITE, DEVTYPE_XBOX_ONE_S};
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;

/// The identity of pad `index` as `devtype`.
pub(crate) fn identity(devtype: u8, index: u8) -> Identity {
    Identity {
        bluetooth: true,
        ..Identity::of(
            devtype,
            format!("punktfunk-xbox{devtype}-{index}"),
            pf_driver_proto::xbox::rdesc(devtype),
        )
    }
}

/// One virtual Xbox pad. Drop removes it.
pub struct XboxMacPad {
    dev: HidUserDevice,
    /// Input bytes the identity's descriptor declares; Share rides past One S / Elite's 16.
    report_len: usize,
}

/// One identity per manager. `Default` is the Xbox Wireless, which `XboxMacManager::new()`
/// builds.
pub struct XboxMacProto {
    devtype: u8,
}

impl Default for XboxMacProto {
    fn default() -> XboxMacProto {
        XboxMacProto {
            devtype: DEVTYPE_XBOX,
        }
    }
}

impl XboxMacProto {
    /// `045E:02FD`.
    pub fn one_s() -> XboxMacProto {
        XboxMacProto {
            devtype: DEVTYPE_XBOX_ONE_S,
        }
    }

    /// `045E:0B22`.
    pub fn elite() -> XboxMacProto {
        XboxMacProto {
            devtype: DEVTYPE_XBOX_ELITE,
        }
    }
}

impl PadProto for XboxMacProto {
    type Pad = XboxMacPad;
    type State = XboxState;
    const LABEL: &'static str = "Xbox Wireless/macOS";
    const DEVICE: &'static str = "Xbox Wireless Controller";
    const CREATE_HINT: &'static str = super::hid_user_device::CREATE_HINT;

    fn open(&mut self, idx: u8) -> Result<XboxMacPad> {
        let id = identity(self.devtype, idx);
        let dev = HidUserDevice::open(&id)?;
        tracing::info!(
            index = idx,
            identity = %format!("{:04X}:{:04X}", id.vendor, id.product),
            "virtual Xbox pad created (IOHIDUserDevice)"
        );
        Ok(XboxMacPad {
            dev,
            report_len: pf_driver_proto::xbox::input_len(self.devtype),
        })
    }

    fn alive(&self, pad: &XboxMacPad) -> bool {
        pad.dev.alive()
    }

    fn merge_frame(&self, _prev: &XboxState, f: &GamepadFrame) -> XboxState {
        XboxState::from_frame(f)
    }

    /// No rich plane on an Xbox pad.
    fn apply_rich(&self, _st: &mut XboxState, _rich: RichInput) {}

    fn write_state(&self, pad: &mut XboxMacPad, st: &XboxState) {
        let r = serialize_xbox_state(st);
        let _ = pad.dev.write_input(&r[..pad.report_len]);
    }

    /// Output report `0x03`: both handle motors and both trigger motors on 0xCA.
    fn service(&self, pad: &mut XboxMacPad, _idx: u8) -> PadFeedback {
        let mut rumble = None;
        pad.dev.poll(|_, data| {
            if let Some(r) = parse_xbox_output(data) {
                rumble = Some(r);
            }
        });
        PadFeedback {
            rumble,
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }
}

/// Session Xbox pads; `new()` is the Xbox Wireless, the macOS default pad.
pub type XboxMacManager = UhidManager<XboxMacProto>;

impl UhidManager<XboxMacProto> {
    /// Relay changed rumble. An Xbox pad has no rich-feedback plane.
    pub fn pump_rumble(&mut self, send: impl FnMut(u16, u16, u16, u16, u16)) {
        self.pump(send, |_| {});
    }
}
