//! Virtual Steam Controller 2 (Triton, `28DE:1302`) on Windows over the
//! pf_gamepad UMDF shm channel — analogue of Linux UHID/usbip Triton
//! (`super::steam_controller2`), sharing [`crate::triton_proto`].
//!
//! Unlike the Deck ([`super::steam_deck_windows`]), this is not re-synthesized
//! from typed state: the client forwards raw input reports
//! ([`RichInput::HidReport`](punktfunk_core::quic::RichInput)); the host
//! mirrors them into the input slot. The driver trims each to its declared
//! report-id length before hidclass.
//!
//! Steam's SET_REPORT features and `0x80..` haptic OUTPUT reports come back
//! kind-tagged (bit 31 of the slot length,
//! [`pf_driver_proto::triton::OUT_FEATURE_BIT`], [`OutputDrain::drain_tagged`])
//! and go to the client as `HidOutput::HidRaw`. Rumble is also parsed from the
//! untagged OUTPUT plane onto 0xCA so a phone-mirror path works without raw.
//!
//! Same sealed channel + `SwDeviceCreate` as Deck, device-type
//! [`DEVTYPE_TRITON`]. The real wired Triton is single-interface: no `MI_`
//! token; SDL matches `28DE:1302` on VID/PID alone, so `usb_mi` is `None`.

use super::dualsense_windows::{
    create_swdevice, driver_marks, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::{DriverAttach, PadChannel, ProofTransport, SwDevice};
use crate::triton_proto::{
    parse_triton_rumble, serialize_triton_state, triton_serial, TritonState, TRITON_STATE_LEN,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_TRITON;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};
use std::time::Duration;

/// INF hardware id. A package rename must not touch it.
pub(super) const TRITON_HWID: &str = "pf_triton";

/// One virtual SC2: `SwDeviceCreate`'d `pf_triton_<index>` plus the sealed
/// channel. Drop removes the devnode and both sections. `pub` because it is
/// `PadProto::Pad`.
pub struct TritonWinPad {
    /// Devnode RAII (`SwDeviceClose` on drop).
    _sw: Option<SwDevice>,
    channel: PadChannel,
    attach: DriverAttach,
    /// Synth-mode sequence only. The raw path mirrors the physical pad's
    /// bytes, its own sequence byte included.
    seq: u8,
    /// v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Kind-tagged FEATURE/OUTPUT split — see [`OutputDrain::drain_tagged`].
    drain: OutputDrain,
}

impl TritonWinPad {
    /// Stamp `device_type` first and magic last, then spawn `pf_triton_<index>`.
    fn open(index: u8) -> Result<TritonWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_TRITON;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability `2` = "this host drains the v2.2 long ring", stamped before the
            // magic so the driver sees it on attach (see the DualSense open path + PadShm docs).
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; 64],
                neutral_triton_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_triton_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: TRITON_HWID,
            usb_vid_pid: "VID_28DE&PID_1302",
            // Single-interface wired Triton — no MI_ token; SDL claims 0x1302 on
            // VID/PID only. If Steam balks, A/B `Some(0)` (Deck needed `Some(2)`).
            usb_mi: None,
            description: "Punktfunk Virtual Steam Controller",
            enumerator: "punktfunk",
        })?; // Propagate — swallowing latches the slot to a pad with no devnode.
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // Bind the DATA section to THIS devnode, not the pid the LocalService-writable
        // mailbox names.
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(SwDevice::new);
        // The driver must read `device_type = Triton` before hidclass asks for
        // descriptors, or the pad enumerates as DualSense.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(TritonWinPad {
            _sw,
            channel,
            attach: DriverAttach::new(
                TRITON_HWID,
                "pf_gamepad.inf", // one INF serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    fn write_state(&mut self, st: &TritonState) {
        let mut r = [0u8; 64];
        if st.raw_len > 0 {
            let len = (st.raw_len as usize).min(st.raw.len()).min(r.len());
            r[..len].copy_from_slice(&st.raw[..len]);
        } else {
            self.seq = self.seq.wrapping_add(1);
            let mut s = [0u8; TRITON_STATE_LEN];
            serialize_triton_state(&mut s, st, self.seq);
            r[..TRITON_STATE_LEN].copy_from_slice(&s);
        }
        // SAFETY: same contract as DeckWinPad::write_state — the v2.3 input_gen seqlock.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Drain Steam writes: rumble on 0xCA from untagged OUTPUT only (FEATURE
    /// is never rumble); raw kind-tagged for `[0xCD][0x05]`. `resync` is the
    /// ring-overflow flag and must reach `PadFeedback` unchanged.
    fn service(&mut self, idx: u8) -> (Option<(u16, u16)>, Vec<HidOutput>, bool) {
        self.channel.pump();
        // SAFETY: the channel's section is live and SHM_SIZE bytes.
        let (proto, rev) = unsafe { driver_marks(self.channel.data_base()) };
        self.attach.observe_pad(proto, rev);
        let base = self.channel.data_base();
        let mut rumble = None;
        let mut hidout = Vec::new();
        let resync = self.drain.drain_tagged(base, |bytes, feature| {
            // hidclass pads writes to 64; Linux forwards native length (0x80
            // rumble is 10). Trim OUTPUT to `out_report_len` so GATT is not
            // padded. FEATURE stays whole (Steam SETs full reports). Ring
            // slices are non-empty; salvage/legacy is a fixed 64-byte slice.
            let bytes = match (feature, bytes.first()) {
                (false, Some(&id)) => {
                    &bytes[..bytes.len().min(pf_driver_proto::triton::out_report_len(id))]
                }
                _ => bytes,
            };
            if !feature {
                if let Some(r) = parse_triton_rumble(bytes) {
                    rumble = Some(r);
                }
            }
            hidout.push(HidOutput::HidRaw {
                pad: idx,
                kind: if feature {
                    HID_RAW_FEATURE
                } else {
                    HID_RAW_OUTPUT
                },
                data: bytes.to_vec(),
            });
        });
        (rumble, hidout, resync)
    }
}

/// Neutral wired-Triton `0x42` state report: report id plus a zero 53-byte
/// payload. Fresh and unplugged pads start here.
fn neutral_triton_report() -> [u8; 64] {
    let mut r = [0u8; 64];
    r[0] = 0x42;
    r
}

/// Windows Triton [`PadProto`]: sealed-channel open, as-is mirroring plus
/// typed fallback, kind-tagged feedback. Lifecycle lives in [`UhidManager`].
///
/// `Default` is required: `UhidManager::new()` bounds `B: PadProto + Default`.
#[derive(Default)]
pub struct TritonWinProto;

impl PadProto for TritonWinProto {
    type Pad = TritonWinPad;
    type State = TritonState;
    const LABEL: &'static str = "Steam Controller 2/Windows";
    const DEVICE: &'static str = "Steam Controller 2";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<TritonWinPad> {
        let p = TritonWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            // Serial the driver answers GET_REPORT with, derived from idx.
            serial = %triton_serial(idx),
            "virtual Steam Controller 2 created (Windows UMDF shm channel, as-is raw passthrough)"
        );
        Ok(p)
    }

    fn neutral(&self) -> TritonState {
        TritonState::neutral()
    }

    fn merge_frame(
        &self,
        prev: &TritonState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> TritonState {
        let mut s = TritonState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        // As-is mode is sticky: a typed frame between two raw reports must not
        // flap the pad back to synth (the client sends both planes).
        s.raw = prev.raw;
        s.raw_len = prev.raw_len;
        s
    }

    fn apply_rich(&self, st: &mut TritonState, rich: RichInput) {
        if let RichInput::HidReport { len, data, .. } = rich {
            let len = (len as usize).min(data.len()).min(st.raw.len());
            if len == 0 {
                return;
            }
            st.raw[..len].copy_from_slice(&data[..len]);
            st.raw_len = len as u8;
        }
        // Touchpad/Motion/TouchpadEx: nothing to fold — the raw feed carries
        // pads + IMU, and the synth fallback has no surface for them.
    }

    // `neutralize_gyro` / `clear_rich` stay the trait no-ops: this device never
    // sees `RichInput::Motion`, and motion lives in an opaque passthrough report.

    fn write_state(&self, pad: &mut TritonWinPad, st: &TritonState) {
        pad.write_state(st);
    }

    /// Rumble on 0xCA, raw kind-tagged on `[0xCD][0x05]`. Forward the drain's
    /// `resync` — unlike Linux (no ring, permanent `false`), this ring can
    /// overflow; hardcoding `false` would drop that signal.
    fn service(&self, pad: &mut TritonWinPad, idx: u8) -> PadFeedback {
        let (rumble, hidout, resync) = pad.service(idx);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout,
            // Steam is a hidraw writer, so abandoned-rumble force-off applies.
            // The raw 0xCD passthrough plane is unaffected.
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

pub type TritonWindowsManager = UhidManager<TritonWinProto>;
