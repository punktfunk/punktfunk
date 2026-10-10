//! Virtual Nintendo Switch pads on `/dev/uhid`, bound by `hid-nintendo` (≥ 5.16): the Pro
//! Controller, and a Joy-Con pair as two Bluetooth halves that SDL and Steam combine. State
//! mapping lives in [`super::switch_proto`], the replies in `pf_driver_proto::switch`; this file
//! is the UHID plumbing that answers the driver's probe from [`UhidManager`]'s `service` pass.
//!
//! `hid-nintendo` is not DualSense's three GET_REPORTs: it runs a blocking probe
//! (`0x80` USB commands, then subcommands for device info, SPI calibration, IMU,
//! vibration, input mode, player lights). Each step must see `0x81`/`0x21` within
//! 1–2 s or the probe aborts and no input devices appear.
//!
//! After bind, LED/rumble writes stall up to 250 ms unless `0x30` reports are
//! flowing — the manager's 8 ms silence heartbeat is that stream. Suspend/resume
//! re-runs the whole init; nothing probe-specific is latched here.

use super::switch_proto::{
    parse_output, player_leds_bits, serialize_for, Half, SwitchOutput, SwitchState, SWITCH_PRODUCT,
    SWITCH_VENDOR,
};
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO;
use pf_driver_proto::switch as wire;
use punktfunk_core::quic::{HidOutput, RichInput};

/// One virtual Switch pad on `/dev/uhid`: a Pro Controller or one Joy-Con half. Drop unbinds
/// `hid-nintendo`.
pub struct SwitchPad {
    dev: UhidDevice,
    device_type: u8,
    index: u8,
    /// Rolling report timer (byte 1 of every input report).
    timer: u8,
    /// Last written state. Subcommand replies embed this header so probe reports stay coherent.
    state: SwitchState,
    /// The last output's rumble as `(side 0, side 1)`; a half drives only its own side.
    rumble: (u16, u16),
}

impl SwitchPad {
    /// `index` is name/uniq and the virtual MAC. A Pro is a USB pad, so `hid-nintendo` runs its
    /// USB probe; a Joy-Con half is a Bluetooth one, as the real pads are.
    pub fn open(index: u8, device_type: u8) -> Result<SwitchPad> {
        let (bus, product, name, tag) = match Half::of(device_type) {
            None => (
                crate::uhid_abi::BUS_USB,
                SWITCH_PRODUCT,
                "Switch Pro Controller",
                "switchpro",
            ),
            Some(Half::Left) => (
                crate::uhid_abi::BUS_BLUETOOTH,
                Half::Left.product(),
                "Joy-Con (L)",
                "joycon-l",
            ),
            Some(Half::Right) => (
                crate::uhid_abi::BUS_BLUETOOTH,
                Half::Right.product(),
                "Joy-Con (R)",
                "joycon-r",
            ),
        };
        let dev = UhidDevice::open(&Create2 {
            bus,
            name: &format!("Punktfunk {name} {index}"),
            phys: &format!("punktfunk/{tag}/{index}"),
            uniq: &format!("punktfunk-{tag}-{index}"),
            rdesc: &wire::RDESC,
            vendor: SWITCH_VENDOR,
            product,
            version: 0x0200, // bcdDevice 2.00
        })?;
        Ok(SwitchPad {
            dev,
            device_type,
            index,
            timer: 0,
            state: SwitchState::neutral(),
            rumble: (0, 0),
        })
    }

    /// A Pro Controller at `index`.
    pub fn pro(index: u8) -> Result<SwitchPad> {
        SwitchPad::open(index, DEVTYPE_SWITCH_PRO)
    }

    pub fn write_state(&mut self, st: &SwitchState) -> Result<()> {
        self.state = *st;
        self.timer = self.timer.wrapping_add(1);
        let r = serialize_for(self.device_type, st, self.timer);
        self.dev.write_input(&r)
    }

    /// Drain UHID events. Each probe step blocks `hid-nintendo` until answered; call often.
    /// A handshake command or subcommand is answered as the Windows driver does. Every `0x80`
    /// is acked, including no-timeout (0x04): that skips the driver's 2 × 100 ms wait. Rumble
    /// comes back as `(side 0, side 1)`.
    pub fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        let (timer, state, index, dt) =
            (&mut self.timer, &self.state, self.index, self.device_type);
        let last = &mut self.rumble;
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => {
                match parse_output(data) {
                    Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                        *last = rumble;
                        // No trigger motors on this protocol — see `PadFeedback::rumble`.
                        fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                        // Player lights are the subcommand payload; the reply still acks it.
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
                }
                *timer = timer.wrapping_add(1);
                let report = serialize_for(dt, state, *timer);
                if let Some(reply) = wire::reply(&report, data, dt, index) {
                    let _ = dev.write_input(&reply);
                }
            }
            // hid-nintendo never GET_REPORTs; EIO so a stray request cannot block.
            UhidEvent::GetReport { id, .. } => {
                let _ = dev.reply_get_report(id, None);
            }
            UhidEvent::SetReport(_) => {}
        });
        fb
    }
}

/// Switch Pro [`PadProto`]: UHID open, [`SwitchState`] mappers, probe `service`.
/// Slot table / unplug / heartbeat / dedup live in [`UhidManager`].
pub struct SwitchProProto {
    /// Steam back-grip fold. A Pro Controller has no paddle slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchProProto {
    fn default() -> SwitchProProto {
        SwitchProProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchProProto {
    type Pad = SwitchPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<SwitchPad> {
        let p = SwitchPad::pro(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (UHID hid-nintendo)"
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

    fn write_state(&self, pad: &mut SwitchPad, st: &SwitchState) {
        let _ = pad.write_state(st);
    }

    /// Probe conversation + feedback: HD-rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // hid-nintendo embeds rumble in every command, so a poll that saw rumble is
        // the activity signal. Physical HD-rumble decays faster than the idle window;
        // abandoned-rumble force-off covers a writer that latches a level.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

/// Session Switch Pro pads (`PUNKTFUNK_GAMEPAD=switchpro`, or a Nintendo-family per-pad kind).
pub type SwitchProManager = UhidManager<SwitchProProto>;

/// A Joy-Con pair: both halves of one pad slot. The left half drives the low motor, the right
/// half the high one.
pub struct JoyConPair {
    left: SwitchPad,
    right: SwitchPad,
}

/// Joy-Con pair [`PadProto`]. SL/SR carry the paddles, so nothing folds.
#[derive(Default)]
pub struct JoyConPairProto;

impl PadProto for JoyConPairProto {
    type Pad = JoyConPair;
    type State = SwitchState;
    const LABEL: &'static str = "Joy-Con pair";
    const DEVICE: &'static str = "Joy-Con pair";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<JoyConPair> {
        let pair = JoyConPair {
            left: SwitchPad::open(idx, Half::Left.device_type())?,
            right: SwitchPad::open(idx, Half::Right.device_type())?,
        };
        tracing::info!(
            index = idx,
            "virtual Joy-Con pair created (UHID hid-nintendo)"
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

    fn write_state(&self, pad: &mut JoyConPair, st: &SwitchState) {
        let _ = pad.left.write_state(st);
        let _ = pad.right.write_state(st);
    }

    /// Both probes, rumble from each half's own side, player lights from the left half (a host
    /// lights both halves alike).
    fn service(&self, pad: &mut JoyConPair, idx: u8) -> PadFeedback {
        let left = pad.left.service(idx);
        let right = pad.right.service(idx);
        let drove = left.rumble.is_some() || right.rumble.is_some();
        let mut fb = left;
        fb.rumble = drove.then(|| {
            let low = Half::Left.rumble(pad.left.rumble);
            let high = Half::Right.rumble(pad.right.rumble);
            (low, high, 0, 0)
        });
        fb.rumble_drove = Some(drove);
        fb
    }
}

/// Session Joy-Con pairs (a Joy-Con pair client, or `PUNKTFUNK_GAMEPAD=joyconpair`).
pub type JoyConPairManager = UhidManager<JoyConPairProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::gamepad as gs;
    use punktfunk_core::input::{GamepadEvent, GamepadFrame};
    use std::time::{Duration, Instant};

    /// Holds a Joy-Con pair live for `PF_PAD_HOLD_SECS` (default 3) so an SDL probe can read it:
    /// all four paddles beat every 400 ms, a steady 100 °/s pitch, rumble echoed to stdout.
    #[test]
    #[ignore = "creates real /dev/uhid devices; needs the input group"]
    fn joycon_pair_holds_for_a_probe() {
        let secs = std::env::var("PF_PAD_HOLD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let (idx, mut pair) = (4u8, JoyConPairManager::new());
        pair.handle(&GamepadEvent::Arrival {
            index: idx,
            kind: 0,
            capabilities: 0,
            audio_caps: 0,
        });
        assert_eq!(pair.live_pads(), 1, "the pair must be created");
        println!("Joy-Con pair up for {secs}s");
        let paddles = gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4;
        let (start, mut last, mut beat) = (Instant::now(), Instant::now(), 0u32);
        let mut last_motion = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            // 50 Hz: under the manager's 100 ms idle watchdog, which zeroes a stalled gyro.
            if last_motion.elapsed() >= Duration::from_millis(20) {
                last_motion = Instant::now();
                pair.apply_rich(RichInput::Motion {
                    pad: idx,
                    gyro: [(100 * gs::MOTION_GYRO_LSB_PER_DEG_S) as i16, 0, 0],
                    accel: [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
                });
            }
            if last.elapsed() >= Duration::from_millis(400) {
                last = Instant::now();
                beat += 1;
                let buttons = if beat % 2 == 0 {
                    gs::BTN_A | gs::BTN_DPAD_UP | paddles
                } else {
                    0
                };
                pair.handle(&GamepadEvent::State(GamepadFrame {
                    index: idx as i16,
                    active_mask: 1 << idx,
                    buttons,
                    ..Default::default()
                }));
            }
            let echo = |pad, low, high, _, _| println!("rumble pad={pad} low={low} high={high}");
            pair.pump(echo, |_| {});
            pair.heartbeat(Duration::from_millis(8));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
