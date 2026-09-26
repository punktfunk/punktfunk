//! Virtual Nintendo Switch Pro Controller on `/dev/uhid`, bound by `hid-nintendo`
//! (≥ 5.16). State mapping lives in [`super::switch_proto`], the replies in
//! `pf_driver_proto::switch`; this file is the UHID plumbing that answers the driver's
//! probe from [`UhidManager`]'s `service` pass.
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
    parse_output, player_leds_bits, serialize_report_0x30, SwitchOutput, SwitchState,
    SWITCH_PRODUCT, SWITCH_VENDOR,
};
use crate::uhid_abi::{
    put_cstr, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2, UHID_DESTROY, UHID_EVENT_SIZE,
    UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2, UHID_OUTPUT, UHID_PATH,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use pf_driver_proto::switch as wire;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

/// Virtual Pro Controller on `/dev/uhid`. Drop sends `UHID_DESTROY` and unbinds `hid-nintendo`.
pub struct SwitchProPad {
    fd: File,
    index: u8,
    /// Rolling report timer (byte 1 of every input report).
    timer: u8,
    /// Last written state. Subcommand replies embed this header so probe reports stay coherent.
    state: SwitchState,
}

impl SwitchProPad {
    /// `index` is name/uniq and the virtual MAC.
    pub fn open(index: u8) -> Result<SwitchProPad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut pad = SwitchProPad {
            fd,
            index,
            timer: 0,
            state: SwitchState::neutral(),
        };
        pad.send_create2(index).context("UHID_CREATE2 Switch Pro")?;
        Ok(pad)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // uhid_create2_req at 4: name[128] phys[64] uniq[64] rd_size bus vid pid version country rd_data.
        // BUS_USB selects hid-nintendo's USB probe, not Bluetooth.
        put_cstr(
            &mut ev,
            4,
            128,
            &format!("Punktfunk Switch Pro Controller {index}"),
        );
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/switchpro/{index}"));
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-swpro-{index}"));
        ev[260..262].copy_from_slice(&(wire::RDESC.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
        ev[264..268].copy_from_slice(&SWITCH_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&SWITCH_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0200u32.to_ne_bytes()); // bcdDevice 2.00
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes());
        ev[280..280 + wire::RDESC.len()].copy_from_slice(&wire::RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    fn write_report(&mut self, r: &[u8; wire::REPORT_LEN]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        // uhid_input2_req: size u16 at 4, data at 6.
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes());
        ev[6..6 + r.len()].copy_from_slice(r);
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    pub fn write_state(&mut self, st: &SwitchState) -> Result<()> {
        self.state = *st;
        self.timer = self.timer.wrapping_add(1);
        let r = serialize_report_0x30(st, self.timer);
        self.write_report(&r)
    }

    /// Answer a handshake command or subcommand, as the Windows driver does. Every `0x80` is
    /// acked, including no-timeout (0x04): that skips the driver's 2 × 100 ms wait.
    fn answer(&mut self, output: &[u8]) {
        self.timer = self.timer.wrapping_add(1);
        let state = serialize_report_0x30(&self.state, self.timer);
        if let Some(reply) = wire::reply(&state, output, self.index) {
            let _ = self.write_report(&reply);
        }
    }

    /// Drain UHID events. Each probe step blocks `hid-nintendo` until answered; call often.
    pub fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => {
                    // uhid_output_req: data[4096] at [4..4100], size u16 at [4100..4102].
                    let size = u16::from_ne_bytes([ev[4100], ev[4101]]) as usize;
                    let end = 4 + size.min(HID_MAX_DESCRIPTOR_SIZE);
                    let data = ev[4..end].to_vec();
                    match parse_output(&data) {
                        Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                            // No trigger motors on this protocol — see `PadFeedback::rumble`.
                            fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                            // Player lights are the subcommand payload; `answer` still acks it.
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
                    self.answer(&data);
                }
                UHID_GET_REPORT => {
                    // hid-nintendo never GET_REPORTs; EIO so a stray request cannot block.
                    let req_id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_get_report_err(req_id);
                }
                _ => {}
            }
        }
        fb
    }

    fn reply_get_report_err(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&5u16.to_ne_bytes()); // EIO
        self.fd
            .write_all(&ev)
            .context("write UHID_GET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for SwitchProPad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
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
    type Pad = SwitchProPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<SwitchProPad> {
        let p = SwitchProPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (UHID hid-nintendo)"
        );
        Ok(p)
    }

    fn neutral(&self) -> SwitchState {
        SwitchState::neutral()
    }

    /// Button/stick/trigger frame. Keep prev motion — it arrives on the rich plane.
    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = SwitchState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s
    }

    /// IMU samples only; a Pro Controller has no touchpad.
    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            st.apply_motion(gyro, accel);
        }
    }

    fn neutralize_gyro(&self, st: &mut SwitchState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SwitchState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut SwitchProPad, st: &SwitchState) {
        let _ = pad.write_state(st);
    }

    /// Probe conversation + feedback: HD-rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchProPad, idx: u8) -> PadFeedback {
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
