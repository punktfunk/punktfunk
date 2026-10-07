//! Virtual Wireless HORIPAD for Steam on Windows via the UMDF minidriver: device type 12,
//! `VID_0F0D&PID_01AB`, hardware id `pf_horipad_steam`. Same sealed channel as
//! [`super::dualsense_windows`]; the state codec is [`super::hori_proto`]. The driver serves on
//! its 4 ms period and stamps the clock. No output reports: the pad has no rumble.

use super::gamepad_raii::SwDeviceProfile;
use super::hori_proto::{serial, HoriState};
use super::pad_shm::ShmPad;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_HORIPAD_STEAM;
use pf_driver_proto::hori::NEUTRAL_REPORT;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;

/// INF hardware id. A package rename must not change it (`hwid_matches_inf`).
pub(super) const HORI_HWID: &str = "pf_horipad_steam";

/// Drop closes the devnode. `pub` because it is `PadProto::Pad`.
pub struct HoriWinPad {
    shm: ShmPad,
    serial: [u8; 6],
}

impl HoriWinPad {
    fn open(index: u8) -> Result<HoriWinPad> {
        let shm = ShmPad::open(
            index,
            DEVTYPE_HORIPAD_STEAM,
            &NEUTRAL_REPORT,
            &SwDeviceProfile {
                instance: &format!("pf_horipad_{index}"),
                container_tag: 0x5046_4852, // "PFHR"
                container_index: index,
                hwid: HORI_HWID,
                usb_vid_pid: Some("VID_0F0D&PID_01AB"),
                usb_mi: None,
                bluetooth: false,
                description: "Punktfunk Virtual Wireless HORIPAD For Steam",
                enumerator: "VID_0F0D&PID_01AB",
                property: None,
            },
        )?;
        Ok(HoriWinPad {
            shm,
            serial: serial(index),
        })
    }
}

#[derive(Default)]
pub struct HoriWinProto;

impl PadProto for HoriWinProto {
    type Pad = HoriWinPad;
    type State = HoriState;
    const LABEL: &'static str = "HORIPAD/Windows";
    const DEVICE: &'static str = "HORIPAD for Steam";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<HoriWinPad> {
        let p = HoriWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual HORIPAD for Steam created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &HoriState, f: &GamepadFrame) -> HoriState {
        HoriState::merge_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut HoriState, rich: RichInput) {
        st.apply_rich(rich);
    }

    /// The driver stamps the clock per served report.
    fn write_state(&self, pad: &mut HoriWinPad, st: &HoriState) {
        pad.shm.publish(&st.serialize(0, pad.serial));
    }

    fn service(&self, pad: &mut HoriWinPad, _idx: u8) -> PadFeedback {
        PadFeedback {
            resync: pad.shm.poll(|_, _| {}),
            ..PadFeedback::default()
        }
    }
}

pub type HoriWindowsManager = UhidManager<HoriWinProto>;
