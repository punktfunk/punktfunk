//! Virtual Switch Pro Controller (`057E:2009`) on macOS, over [`HidUserDevice`]. The transport
//! stays `Virtual`, which readers take for USB, so they open with the `0x80` USB commands.
//!
//! A reader drives the pad through output reports: `0x80` commands and `0x01` subcommands, each
//! answered with an input report (`0x81`, `0x21`) as the Windows driver answers them. Mapping
//! lives in [`super::switch_proto`], the replies in `pf_driver_proto::switch`.

use super::switch_proto::{
    parse_output, player_leds_bits, serialize_for, SwitchOutput, SwitchState,
};
use crate::hid_user_device::{HidUserDevice, Identity};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO;
use pf_driver_proto::switch as wire;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::{HidOutput, RichInput};

/// One virtual Switch Pro Controller. Drop removes it.
pub struct SwitchProMacPad {
    dev: HidUserDevice,
    index: u8,
    /// Rolling report timer, byte 1 of every input report.
    timer: u8,
    /// Last written state: a subcommand reply carries its header.
    state: SwitchState,
}

impl SwitchProMacPad {
    fn write(&mut self, st: &SwitchState) {
        self.state = *st;
        self.timer = self.timer.wrapping_add(1);
        let _ = self
            .dev
            .write_input(&serialize_for(DEVTYPE_SWITCH_PRO, st, self.timer));
    }

    /// Answer every queued command and subcommand. Rumble rides each `0x01` and `0x10`;
    /// subcommand `0x30` sets the player lights.
    fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        let (timer, state, index) = (&mut self.timer, &self.state, self.index);
        self.dev.poll(|dev, data| {
            match parse_output(data) {
                Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                    fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                    if let (0x30, Some(&arg)) = (id, args.first()) {
                        fb.hidout.push(HidOutput::PlayerLeds {
                            pad,
                            bits: player_leds_bits(arg),
                        });
                    }
                }
                Some(SwitchOutput::Rumble(r)) => fb.rumble = Some((r.0, r.1, 0, 0)),
                Some(SwitchOutput::UsbCmd(_)) | None => {}
            }
            *timer = timer.wrapping_add(1);
            let report = serialize_for(DEVTYPE_SWITCH_PRO, state, *timer);
            if let Some(reply) = wire::reply(&report, data, DEVTYPE_SWITCH_PRO, index) {
                let _ = dev.write_input(&reply);
            }
        });
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

pub struct SwitchProMacProto {
    /// Steam back-grip fold: a Pro Controller has no paddle slot.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchProMacProto {
    fn default() -> SwitchProMacProto {
        SwitchProMacProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchProMacProto {
    type Pad = SwitchProMacPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro/macOS";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str = super::hid_user_device::CREATE_HINT;

    fn open(&mut self, idx: u8) -> Result<SwitchProMacPad> {
        let id = Identity::of(
            DEVTYPE_SWITCH_PRO,
            format!("punktfunk-switchpro-{idx}"),
            &wire::RDESC,
        );
        let dev = HidUserDevice::open(&id)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (IOHIDUserDevice)"
        );
        Ok(SwitchProMacPad {
            dev,
            index: idx,
            timer: 0,
            state: SwitchState::neutral(),
        })
    }

    fn alive(&self, pad: &SwitchProMacPad) -> bool {
        pad.dev.alive()
    }

    fn merge_frame(&self, prev: &SwitchState, f: &GamepadFrame) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        SwitchState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut SwitchProMacPad, st: &SwitchState) {
        pad.write(st);
    }

    fn service(&self, pad: &mut SwitchProMacPad, idx: u8) -> PadFeedback {
        pad.service(idx)
    }
}

/// Session Switch Pro pads; a Joy-Con pair presents as one on macOS.
pub type SwitchProMacManager = UhidManager<SwitchProMacProto>;
