//! Virtual DualSense Edge on Windows — [`DsWinPad`] under [`WinDsIdentity::dualsense_edge`].
//!
//! Same [`DsState`] codec as [`super::dualsense_windows`]. The host stamps `device_type = 2`
//! before magic so the one UMDF driver serves `VID_054C&PID_0DF2` / hwid `pf_dualsenseedge`.
//! Wire paddles land on native `buttons[2]` ([`edge_paddle_bits`]) instead of fold/drop.
//! Linux analogue: [`crate::dualsense::DualSenseEdgeManager`].

use super::dualsense_proto::{edge_paddle_bits, DsState, DS_TOUCH_H, DS_TOUCH_W};
use super::dualsense_windows::{DsWinPad, WinDsIdentity};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;

/// No `RemapConfig` — paddles have native `buttons[2]` slots (plain DualSense folds or drops them).
#[derive(Default)]
pub struct DsEdgeWinProto;

impl PadProto for DsEdgeWinProto {
    type Pad = DsWinPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense Edge/Windows";
    const DEVICE: &'static str = "DualSense Edge";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DsWinPad> {
        let p = DsWinPad::open(idx, &WinDsIdentity::dualsense_edge())?;
        tracing::info!(
            index = idx,
            "virtual DualSense Edge created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    /// Paddles OR onto `buttons[2]` each frame — they ride the button plane, not `prev`.
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let mut s = DsState::merge_frame(prev, f, f.buttons);
        s.buttons[2] |= edge_paddle_bits(f.buttons);
        s
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
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            // Vibration-field liveness, not any-report — LED/trigger must not arm force-off.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: fb.resync,
        }
    }
}

/// Windows analogue of [`crate::dualsense::DualSenseEdgeManager`].
pub type DualSenseEdgeWindowsManager = UhidManager<DsEdgeWinProto>;
