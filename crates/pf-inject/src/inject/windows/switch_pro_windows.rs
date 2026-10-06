//! Virtual Switch pads on Windows via the UMDF minidriver: the Pro Controller (device type 8,
//! `VID_057E&PID_2009`, hardware id `pf_switchpro`) and a Joy-Con pair (types 13 and 14,
//! `2006`/`2007`, two Bluetooth devnodes). Same sealed channel as [`super::dualsense_windows`];
//! state mapping is [`super::switch_proto`].
//!
//! The driver answers the `0x80` / `0x01` handshake itself (`pf_driver_proto::switch::reply`)
//! and serves the host's latest `0x30` on the identity's period. Rumble and player lights come
//! back through the output ring. Pin: `dualsense_windows::tests::hwid_matches_inf`.

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use super::switch_proto::{
    parse_output, player_leds_bits, serialize_for, Half, SwitchOutput, SwitchState,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO;
use punktfunk_core::quic::{HidOutput, RichInput};

/// INF hardware ids. A package rename must not change them (`hwid_matches_inf`).
pub(super) const SWITCH_HWID: &str = "pf_switchpro";
pub(super) const JOYCON_LEFT_HWID: &str = "pf_joycon_left";
pub(super) const JOYCON_RIGHT_HWID: &str = "pf_joycon_right";

/// Drop closes the devnode. `pub` because it is `PadProto::Pad`.
pub struct SwitchWinPad {
    shm: ShmPad,
    device_type: u8,
    /// The last output's rumble as `(side 0, side 1)`; a half drives only its own side.
    rumble: (u16, u16),
}

impl SwitchWinPad {
    fn open(index: u8, device_type: u8, profile: &SwDeviceProfile) -> Result<SwitchWinPad> {
        let neutral = serialize_for(device_type, &SwitchState::neutral(), 0);
        let shm = ShmPad::open(index, device_type, &neutral, profile)?;
        Ok(SwitchWinPad {
            shm,
            device_type,
            rumble: (0, 0),
        })
    }

    fn open_pro(index: u8) -> Result<SwitchWinPad> {
        SwitchWinPad::open(
            index,
            DEVTYPE_SWITCH_PRO,
            &SwDeviceProfile {
                instance: &format!("pf_swpro_{index}"),
                container_tag: 0x5046_5357, // "PFSW"
                container_index: index,
                hwid: SWITCH_HWID,
                usb_vid_pid: Some("VID_057E&PID_2009"),
                // A wired Pro Controller is a single-interface HID device.
                usb_mi: None,
                bluetooth: false,
                description: "Punktfunk Virtual Pro Controller",
                enumerator: "VID_057E&PID_2009",
                property: None,
            },
        )
    }

    /// One Joy-Con half of pad `slot`. The driver finds its mailbox by the devnode's index, so
    /// the right half takes one past the host's pad slots.
    fn open_half(slot: u8, half: Half) -> Result<SwitchWinPad> {
        let (index, hwid, side) = match half {
            Half::Left => (slot, JOYCON_LEFT_HWID, 'L'),
            Half::Right => (
                slot + punktfunk_core::input::MAX_PADS as u8,
                JOYCON_RIGHT_HWID,
                'R',
            ),
        };
        let vid_pid = format!("VID_057E&PID_{:04X}", half.product());
        SwitchWinPad::open(
            index,
            half.device_type(),
            &SwDeviceProfile {
                instance: &format!("{hwid}_{slot}"),
                container_tag: 0x5046_4A43, // "PFJC"
                container_index: index,
                hwid,
                usb_vid_pid: Some(&vid_pid),
                usb_mi: None,
                bluetooth: true,
                description: &format!("Punktfunk Virtual Joy-Con ({side})"),
                enumerator: &vid_pid,
                property: None,
            },
        )
    }

    /// The driver stamps the timer byte per served report, so the host's is left at zero.
    fn write_state(&mut self, st: &SwitchState) {
        self.shm.publish(&serialize_for(self.device_type, st, 0));
    }

    /// Rumble from every `0x01` / `0x10` as `(side 0, side 1)`, player lights from subcommand
    /// `0x30`, oldest first.
    fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        let last = &mut self.rumble;
        fb.resync = self.shm.poll(|bytes, _| match parse_output(bytes) {
            Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                *last = rumble;
                fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                if let (0x30, Some(&arg)) = (id, args.first()) {
                    fb.hidout.push(HidOutput::PlayerLeds {
                        pad,
                        bits: player_leds_bits(arg),
                    });
                }
            }
            Some(SwitchOutput::Rumble(r)) => {
                *last = r;
                fb.rumble = Some((r.0, r.1, 0, 0));
            }
            Some(SwitchOutput::UsbCmd(_)) | None => {}
        });
        fb
    }
}

/// Slot table, unplug, heartbeat, and `HidoutDedup` live in [`UhidManager`].
pub struct SwitchWinProto {
    /// Steam back-grip fold. A Pro Controller has no paddle slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchWinProto {
    fn default() -> SwitchWinProto {
        SwitchWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchWinProto {
    type Pad = SwitchWinPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro/Windows";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<SwitchWinPad> {
        let p = SwitchWinPad::open_pro(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        SwitchState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut SwitchWinPad, st: &SwitchState) {
        pad.write_state(st);
    }

    /// HD rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchWinPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // Every subcommand carries rumble, so a poll that saw rumble is the activity signal.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

pub type SwitchProWindowsManager = UhidManager<SwitchWinProto>;

/// Both halves of one pad slot. Drop closes both devnodes.
pub struct JoyConWinPair {
    left: SwitchWinPad,
    right: SwitchWinPad,
}

/// Joy-Con pair [`PadProto`]. SL/SR carry the paddles, so nothing folds.
#[derive(Default)]
pub struct JoyConWinProto;

impl PadProto for JoyConWinProto {
    type Pad = JoyConWinPair;
    type State = SwitchState;
    const LABEL: &'static str = "Joy-Con pair/Windows";
    const DEVICE: &'static str = "Joy-Con pair";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<JoyConWinPair> {
        let pair = JoyConWinPair {
            left: SwitchWinPad::open_half(idx, Half::Left)?,
            right: SwitchWinPad::open_half(idx, Half::Right)?,
        };
        tracing::info!(
            index = idx,
            "virtual Joy-Con pair created (Windows UMDF shm channel)"
        );
        Ok(pair)
    }

    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        SwitchState::merge_joycon_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut JoyConWinPair, st: &SwitchState) {
        pad.left.write_state(st);
        pad.right.write_state(st);
    }

    /// Rumble from each half's own side, player lights from the left half.
    fn service(&self, pad: &mut JoyConWinPair, idx: u8) -> PadFeedback {
        let left = pad.left.service(idx);
        let right = pad.right.service(idx);
        let drove = left.rumble.is_some() || right.rumble.is_some();
        let mut fb = left;
        fb.resync |= right.resync;
        fb.rumble = drove.then(|| {
            let low = Half::Left.rumble(pad.left.rumble);
            let high = Half::Right.rumble(pad.right.rumble);
            (low, high, 0, 0)
        });
        fb.rumble_drove = Some(drove);
        fb
    }
}

pub type JoyConWindowsManager = UhidManager<JoyConWinProto>;
