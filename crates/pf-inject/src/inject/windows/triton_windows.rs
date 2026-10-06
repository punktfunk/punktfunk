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
//! [`pf_driver_proto::triton::OUT_FEATURE_BIT`], [`crate::pad_shm_ring::OutputDrain`])
//! and go to the client as `HidOutput::HidRaw`. Rumble is also parsed from the
//! untagged OUTPUT plane onto 0xCA so a phone-mirror path works without raw.
//!
//! Same sealed channel + `SwDeviceCreate` as Deck, device-type
//! [`DEVTYPE_TRITON`]. The real wired Triton is single-interface: no `MI_`
//! token; SDL matches `28DE:1302` on VID/PID alone, so `usb_mi` is `None`.

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use crate::triton_proto::{parse_triton_rumble, triton_serial, Sc2Identity, TritonState};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_TRITON;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};

/// INF hardware id. A package rename must not touch it.
pub(super) const TRITON_HWID: &str = "pf_triton";

/// One virtual SC2: `SwDeviceCreate`'d `pf_triton_<index>` plus the sealed
/// channel. `pub` because it is `PadProto::Pad`.
pub struct TritonWinPad {
    shm: ShmPad,
    /// Synth-mode sequence only. The raw path mirrors the physical pad's
    /// bytes, its own sequence byte included.
    seq: u8,
}

impl TritonWinPad {
    /// The devnode always carries an identity property: it outlives the pad, and a pad without
    /// an identity must not answer with the last one's.
    fn open(index: u8, identity: Option<&Sc2Identity>) -> Result<TritonWinPad> {
        let blob = identity.map_or_else(|| vec![0], Sc2Identity::blob);
        let shm = ShmPad::open(
            index,
            DEVTYPE_TRITON,
            &neutral_triton_report(),
            &SwDeviceProfile {
                instance: &format!("pf_triton_{index}"),
                container_tag: 0x5046_4453, // "PFDS"
                container_index: index,
                hwid: TRITON_HWID,
                usb_vid_pid: Some("VID_28DE&PID_1302"),
                // Single-interface wired Triton — no MI_ token; SDL claims 0x1302 on
                // VID/PID only. If Steam balks, A/B `Some(0)` (Deck needed `Some(2)`).
                usb_mi: None,
                bluetooth: false,
                description: "Punktfunk Virtual Steam Controller",
                // Steam merges a pad's HID views by the VID/PID token in the instance path.
                enumerator: "VID_28DE&PID_1302",
                property: Some((
                    pf_driver_proto::triton::IDENTITY_PROPKEY_FMTID,
                    pf_driver_proto::triton::IDENTITY_PROPKEY_PID,
                    &blob,
                )),
            },
        )?;
        Ok(TritonWinPad { shm, seq: 0 })
    }

    fn write_state(&mut self, st: &TritonState) {
        // The whole 64-byte slot: the driver trims to the report id's declared length.
        let (r, _) = st.report(&mut self.seq);
        self.shm.publish(&r);
    }

    /// Drain Steam writes: rumble on 0xCA from untagged OUTPUT only (FEATURE
    /// is never rumble); raw kind-tagged for `[0xCD][0x05]`. `resync` is the
    /// ring-overflow flag and must reach `PadFeedback` unchanged.
    fn service(&mut self, idx: u8) -> (Option<(u16, u16)>, Vec<HidOutput>, bool) {
        let mut rumble = None;
        let mut hidout = Vec::new();
        let resync = self.shm.poll(|bytes, feature| {
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
            } else if !pf_driver_proto::triton::forwards_to_pad(bytes) {
                return;
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
        let identity = crate::triton_proto::identity_for(idx);
        let p = TritonWinPad::open(idx, identity.as_deref())?;
        let serial = identity.as_ref().and_then(|i| i.serial.clone());
        tracing::info!(
            index = idx,
            serial = serial.unwrap_or_else(|| triton_serial(idx)),
            replies = identity.as_ref().map_or(0, |i| i.replies.len()),
            "virtual Steam Controller 2 created (Windows UMDF shm channel, as-is raw passthrough)"
        );
        Ok(p)
    }

    fn merge_frame(
        &self,
        prev: &TritonState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> TritonState {
        TritonState::merge_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut TritonState, rich: RichInput) {
        st.apply_rich(rich);
    }

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
