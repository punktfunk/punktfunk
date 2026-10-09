//! Virtual 8BitDo pads on Windows via the UMDF minidriver: device types 9–11, hardware ids
//! `pf_8bitdo_*`. Same sealed channel as [`super::dualsense_windows`]; the state codec is
//! [`super::eightbitdo_proto`], the descriptors and the capability reply
//! [`pf_driver_proto::eightbitdo`].
//!
//! The driver serves the host's latest report on the identity's period, stamps the Pro models'
//! IMU clock, and answers feature `0x06` itself. The Ultimate 2 presents as Bluetooth, the
//! transport whose 120 Hz step SDL assumes for it. Rumble comes back through the output ring.

use super::eightbitdo_proto::{parse_rumble, EightBitDoState, Model};
use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::eightbitdo::NEUTRAL_REPORT;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;

/// INF hardware ids. A package rename must not change them (`hwid_matches_inf`).
pub(super) const ULTIMATE2_HWID: &str = "pf_8bitdo_ultimate2";
pub(super) const PRO2_HWID: &str = "pf_8bitdo_pro2";
pub(super) const PRO3_HWID: &str = "pf_8bitdo_pro3";

fn hwid(model: Model) -> &'static str {
    match model {
        Model::Ultimate2 => ULTIMATE2_HWID,
        Model::Pro2 => PRO2_HWID,
        Model::Pro3 => PRO3_HWID,
    }
}

/// Drop closes the devnode. `pub` because it is `PadProto::Pad`.
pub struct EightBitDoWinPad {
    shm: ShmPad,
}

impl EightBitDoWinPad {
    fn open(model: Model, index: u8) -> Result<EightBitDoWinPad> {
        let vid_pid = format!("VID_2DC8&PID_{:04X}", model.product());
        let shm = ShmPad::open(
            index,
            model.devtype(),
            &NEUTRAL_REPORT,
            &SwDeviceProfile {
                instance: &format!("{}_{index}", hwid(model)),
                container_tag: 0x5046_3842, // "PF8B"
                container_index: index,
                hwid: hwid(model),
                usb_vid_pid: Some(&vid_pid),
                usb_mi: None,
                bluetooth: model.bluetooth(),
                description: &format!("Punktfunk Virtual {}", model.name()),
                enumerator: &vid_pid,
                property: None,
            },
        )?;
        Ok(EightBitDoWinPad { shm })
    }
}

pub struct EightBitDoWinProto {
    model: Model,
}

impl EightBitDoWinProto {
    pub fn new(model: Model) -> EightBitDoWinProto {
        EightBitDoWinProto { model }
    }
}

impl PadProto for EightBitDoWinProto {
    type Pad = EightBitDoWinPad;
    type State = EightBitDoState;
    const LABEL: &'static str = "8BitDo/Windows";
    const DEVICE: &'static str = "8BitDo";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<EightBitDoWinPad> {
        let p = EightBitDoWinPad::open(self.model, idx)?;
        tracing::info!(
            index = idx,
            model = self.model.name(),
            "virtual 8BitDo created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &EightBitDoState, f: &GamepadFrame) -> EightBitDoState {
        EightBitDoState::merge_frame(self.model, prev, f)
    }

    fn apply_rich(&self, st: &mut EightBitDoState, rich: RichInput) {
        st.apply_rich(rich);
    }

    /// The driver stamps the clock per served report, so the host's is left at zero.
    fn write_state(&self, pad: &mut EightBitDoWinPad, st: &EightBitDoState) {
        pad.shm.publish(&st.serialize(None));
    }

    fn service(&self, pad: &mut EightBitDoWinPad, _idx: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        fb.resync = pad.shm.poll(|bytes, _| {
            if let Some((low, high)) = parse_rumble(bytes) {
                fb.rumble = Some((low, high, 0, 0));
            }
        });
        // Every `0x05` names both motors, so a poll that saw one drove the rumble plane.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

pub type EightBitDoWindowsManager = UhidManager<EightBitDoWinProto>;

/// A manager for one model's pads.
pub fn manager(model: Model) -> EightBitDoWindowsManager {
    UhidManager::with_backend(EightBitDoWinProto::new(model))
}
