//! Virtual Steam Controller 2 (Triton) over `/dev/uhid` — as-is passthrough for
//! [`GamepadPref::SteamController2`](punktfunk_core::config::GamepadPref). Descriptor,
//! report ids, typed fallback, rumble parser: [`super::triton_proto`].
//!
//! Mainline `hid-steam` does not bind `28DE:1302`, so the node is `hid-generic` hidraw
//! with no evdev. Steam Input over hidraw is the only consumer; `gamepad_mode` does
//! not apply. [`RichInput::HidReport`](punktfunk_core::quic::RichInput) is written
//! unchanged; Steam SET_REPORT / OUTPUT writes are acked and queued for the physical pad.
//!
//! Steam ignores UHID (`Interface: -1`). Preferred transport is
//! [`super::triton_usbip`] (`vhci_hcd`); this module is the fallback when that is missing.

use super::triton_proto::{
    parse_triton_rumble, strip_report_prefix, triton_feature_reply, triton_serial, triton_unit_id,
    TritonState, TRITON_RDESC, TRITON_VENDOR, TRITON_WIRED_PRODUCT,
};
use crate::uhid_abi::{UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};

/// The `CREATE2` identity of pad `index`: what this host opens, and what the seat broker
/// builds for a seat. Steam matches VID/PID, not the product string.
pub(crate) fn identity(index: u8) -> crate::uhid_abi::Identity {
    crate::uhid_abi::Identity {
        bus: crate::uhid_abi::BUS_USB,
        name: format!("Punktfunk Steam Controller 2 {index}"),
        phys: format!("punktfunk/triton/{index}"),
        uniq: format!("punktfunk-triton-{index}"),
        rdesc: TRITON_RDESC,
        vendor: TRITON_VENDOR,
        product: TRITON_WIRED_PRODUCT,
        version: 0x0100,
    }
}

/// `/dev/uhid` Triton pad. Drop destroys the device.
pub struct TritonPad {
    dev: UhidDevice,
    /// Synth-mode sequence; the raw path carries the physical pad's own seq.
    seq: u8,
    /// Steam writes since the last service pass, kind-tagged for the 0xCD plane.
    pending_raw: Vec<(u8, Vec<u8>)>,
    /// Last feature SET_REPORT (id-first) — the query half of the Valve GET dance.
    last_set: Vec<u8>,
    serial: String,
    unit_id: u32,
    /// Last GET command logged, so the tester line fires once per distinct cmd.
    last_get_logged: u8,
}

impl TritonPad {
    /// Steam matches VID/PID, not the product string; the name keeps the Punktfunk prefix
    /// every virtual pad uses.
    pub fn open(index: u8) -> Result<TritonPad> {
        let dev = UhidDevice::open_kind(
            crate::pad_broker::PadKind::SteamController2,
            index,
            &identity(index),
        )?;
        Ok(TritonPad {
            dev,
            seq: 0,
            pending_raw: Vec::new(),
            last_set: Vec::new(),
            serial: triton_serial(index),
            unit_id: triton_unit_id(index),
            last_get_logged: 0,
        })
    }

    /// Client raw bytes verbatim, else a synthesized `0x42` state report from typed fields.
    pub fn write_state(&mut self, st: &TritonState) -> Result<()> {
        let (r, len) = st.report(&mut self.seq);
        self.dev.write_input(&r[..len])
    }

    /// Non-blocking. Answer GET_REPORT from canned state (the Valve query cannot round-trip to
    /// the physical pad), queue Steam's writes for raw forward; `poll` acks SET_REPORT.
    /// Returns rumble if a `0x80` output was seen.
    pub fn service(&mut self) -> Option<(u16, u16)> {
        let mut rumble = None;
        let (pending_raw, last_set) = (&mut self.pending_raw, &mut self.last_set);
        let (serial, unit_id, last_get_logged) =
            (&self.serial, self.unit_id, &mut self.last_get_logged);
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => {
                let rep = strip_report_prefix(data);
                if let Some(r) = parse_triton_rumble(rep) {
                    rumble = Some(r);
                }
                queue_raw(pending_raw, HID_RAW_OUTPUT, rep);
            }
            UhidEvent::SetReport(data) => {
                let rep = strip_report_prefix(data);
                if let Some(r) = parse_triton_rumble(rep) {
                    rumble = Some(r); // some stacks send haptics on the feature path
                }
                if pf_driver_proto::triton::forwards_to_pad(rep) {
                    queue_raw(pending_raw, HID_RAW_FEATURE, rep);
                }
                // Selects the next GET_REPORT answer (Valve query dance).
                *last_set = rep.to_vec();
            }
            UhidEvent::GetReport { id, .. } => {
                // Echo last SET's command with a canned payload. The wrong command type
                // makes Steam drop the pad; the dance cannot round-trip live.
                let reply = triton_feature_reply(last_set.as_slice(), serial, unit_id);
                if reply[1] != *last_get_logged {
                    *last_get_logged = reply[1];
                    tracing::debug!(
                        cmd = %format_args!("{:#04x}", reply[1]),
                        "virtual SC2: answering feature GET"
                    );
                }
                let _ = dev.reply_get_report(id, Some(&reply));
            }
        });
        rumble
    }
}

/// Cap 32 so a hidraw client gone haywire cannot grow the queue between pumps.
/// Newest wins — these are level-styled commands.
fn queue_raw(queue: &mut Vec<(u8, Vec<u8>)>, kind: u8, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if queue.len() >= 32 {
        queue.remove(0);
    }
    queue.push((kind, data.to_vec()));
}

/// usbip (`vhci_hcd`) first — a real USB device Steam lists — with UHID as fallback.
/// No gadget rung: no captured gadget layout for Triton, and usbip is universal.
pub enum TritonTransport {
    Usbip(crate::triton_usbip::TritonUsbip),
    Uhid(TritonPad),
}

/// One `service()` pass: rumble `(left, right)` plus raw `(kind, payload)` writes.
type TritonServiced = (Option<(u16, u16)>, Vec<(u8, Vec<u8>)>);

impl TritonTransport {
    fn write_state(&mut self, st: &TritonState) {
        match self {
            TritonTransport::Usbip(u) => u.write_state(st),
            TritonTransport::Uhid(p) => {
                let _ = p.write_state(st);
            }
        }
    }

    fn service(&mut self) -> TritonServiced {
        match self {
            TritonTransport::Usbip(u) => {
                let fb = u.service();
                (fb.rumble, fb.raw)
            }
            TritonTransport::Uhid(p) => {
                let rumble = p.service();
                (rumble, std::mem::take(&mut p.pending_raw))
            }
        }
    }
}

/// Best Steam-visible SC2 transport: usbip (`vhci_hcd`) then UHID. Steam ignores the
/// UHID leg (`Interface: -1`), so fallback is hidraw-only — log the `vhci_hcd` remedy.
fn open_transport(
    idx: u8,
    puck: Option<&mut crate::triton_usbip::PuckHubs>,
) -> Result<TritonTransport> {
    if crate::triton_usbip::usbip_preferred() {
        let opened = if let Some(hubs) = puck {
            crate::triton_usbip::TritonUsbip::open_puck(idx, hubs)
        } else {
            crate::triton_usbip::TritonUsbip::open(idx)
        };
        match opened {
            Ok(u) => return Ok(TritonTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip SC2 unavailable — falling back to UHID")
            }
        }
    }
    let p = TritonPad::open(idx)?;
    tracing::warn!(
        index = idx,
        "virtual Steam Controller 2 created as UHID — Steam WON'T list it (no USB interface; \
         confirmed on-glass). Load vhci_hcd (usbip) so the pad arrives as a real USB device: \
         `sudo modprobe vhci_hcd`, and ensure it loads at boot."
    );
    Ok(TritonTransport::Uhid(p))
}

/// Triton [`PadProto`]: raw mirroring with typed fallback, and raw-forwarding `service`.
/// A Puck backend keeps this session's virtual Pucks, so pads of one Puck share it.
#[derive(Default)]
pub struct TritonProto {
    puck: Option<crate::triton_usbip::PuckHubs>,
}

impl TritonProto {
    pub fn puck() -> Self {
        Self {
            puck: Some(crate::triton_usbip::PuckHubs::default()),
        }
    }
}

impl TritonTransport {
    /// `false` once a uhid pad's relay to the seat supervisor ended.
    fn alive(&self) -> bool {
        match self {
            TritonTransport::Uhid(pad) => pad.dev.alive(),
            _ => true,
        }
    }
}

impl PadProto for TritonProto {
    type Pad = TritonTransport;

    fn alive(&self, pad: &TritonTransport) -> bool {
        pad.alive()
    }
    type State = TritonState;
    const LABEL: &'static str = "Steam Controller 2";
    const DEVICE: &'static str = "Steam Controller 2";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<TritonTransport> {
        open_transport(idx, self.puck.as_mut())
    }

    /// Typed fallback. Once `raw_len > 0`, only refresh typed fields for diagnostics;
    /// `write_state` keeps mirroring the raw report.
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

    fn write_state(&self, pad: &mut TritonTransport, st: &TritonState) {
        pad.write_state(st);
    }

    /// Ack + queue Steam's writes onto 0xCD; rumble also rides 0xCA (deduped) so the
    /// client's phone-mirror path keeps working.
    fn service(&self, pad: &mut TritonTransport, idx: u8) -> PadFeedback {
        let (rumble, raw) = pad.service();
        let hidout = raw
            .into_iter()
            .map(|(kind, data)| HidOutput::HidRaw {
                pad: idx,
                kind,
                data,
            })
            .collect();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout,
            // Steam is a hidraw writer here too, so abandoned-rumble force-off applies
            // (the 0xCD passthrough plane is unaffected).
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }
}

/// Session's virtual SC2 pads — `PUNKTFUNK_GAMEPAD=steamcontroller2` (aliases `sc2`/`ibex`),
/// or the per-pad kind an Android client declares for a captured physical pad.
pub type Triton2Manager = UhidManager<TritonProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sysfs::hid_entry;

    /// `hid-generic` hidraw for `28DE:1302` (no evdev binds this PID). Needs `/dev/uhid`
    /// and the input group.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs the input group"]
    fn triton_backend_creates_hidraw_and_mirrors_raw() {
        let mut pad = TritonPad::open(0).expect("open TritonPad (/dev/uhid + input group?)");
        let mut st = TritonState::neutral();
        let raw: &[u8] = &[0x42, 1, 0x01, 0, 0, 0, 0xFF, 0x7F]; // truncated fixture is enough
        st.raw[..raw.len()].copy_from_slice(raw);
        st.raw_len = raw.len() as u8;
        for _ in 0..50 {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            if hid_entry(":28DE:1302").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        assert!(
            hid_entry(":28DE:1302").is_some(),
            "virtual 28DE:1302 HID device not created"
        );
        drop(pad);
    }
}
