//! Resident virtual HID mouse via the UMDF minidriver (`packaging/windows/drivers/pf-mouse`).
//!
//! With no pointing device, win32k reports `SM_MOUSEPRESENT` = 0 and DWM never composites
//! a cursor into the pf-vdisplay frame — `SendInput` still moves an invisible pointer.
//! One `pf_mouse_<index>` HID devnode for the host process lifetime makes Windows draw it.
//! Sessions still inject via [`super::sendinput`]; `punktfunk-host vmouse-spike` drives
//! the report path here.
//!
//! Transport is the sealed pad channel ([`PadChannel`], `design/gamepad-channel-sealing.md`):
//! unnamed 64-B `MouseShm` duplicated into WUDFHost, bootstrapped via
//! `Global\pfmouse-boot-<index>` ([`mouse_index_for_slot`] — one host per seat, one index each).
//! [`ensure_resident`] never drops the devnode; it dies with the host service.

use super::gamepad_raii::{
    create_swdevice, DriverAttach, PadChannel, ProofTransport, SwDevice, SwDeviceProfile,
    TRUST_MAILBOX_ENV,
};
use anyhow::Result;
use pf_driver_proto::mouse::{input_report, mouse_boot_name, MouseShm, MOUSE_MAGIC};
use std::sync::atomic::Ordering;
use std::time::Duration;

const SHM_SIZE: usize = core::mem::size_of::<MouseShm>();
const OFF_IN_SEQ: usize = core::mem::offset_of!(MouseShm, in_seq);
const OFF_REPORT: usize = core::mem::offset_of!(MouseShm, report);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(MouseShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(MouseShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(MouseShm, pad_index);
const OFF_MAGIC: usize = core::mem::offset_of!(MouseShm, magic);

/// The reserved display connector a seat host owns; unset on the console.
/// `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.
const SEAT_SLOT_ENV: &str = "PUNKTFUNK_SEAT_DISPLAY_SLOT";
const FIRST_SEAT_SLOT: u8 = 12;
const LAST_SEAT_SLOT: u8 = 15;

/// This host's mouse index, from [`SEAT_SLOT_ENV`]. Every OS name the mouse needs — the
/// `Global\pfmouse-boot-<i>` mailbox, the `pf_mouse_<i>` devnode, its container id — is
/// machine-wide, so the several hosts on a seats box must each pick a different `i`. A seat's
/// connector is already unique to it, so it is the index; the console keeps 0. A slot the
/// display plane would refuse reads as the console, which is what an ordinary host has always
/// done.
fn mouse_index_for_slot(raw: Option<&std::ffi::OsStr>) -> u8 {
    raw.and_then(std::ffi::OsStr::to_str)
        .and_then(|slot| slot.parse::<u8>().ok())
        .filter(|slot| (FIRST_SEAT_SLOT..=LAST_SEAT_SLOT).contains(slot))
        .unwrap_or(0)
}

/// Process-lifetime `pf_mouse_<index>` plus sealed `MouseShm`. Dropping it removes the pointer.
pub struct VirtualMouse {
    /// `None` if `SwDeviceCreate` failed; injection then uses an out-of-band devnode.
    _sw: Option<SwDevice>,
    channel: PadChannel,
    attach: DriverAttach,
    seq: u32,
}

impl VirtualMouse {
    /// Unnamed DATA + `Global\pfmouse-boot-<index>` for this host's [`mouse_index_for_slot`].
    /// Stamp index, then magic LAST.
    pub fn open() -> Result<VirtualMouse> {
        let index = mouse_index_for_slot(std::env::var_os(SEAT_SLOT_ENV).as_deref());
        let boot_name = mouse_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        // Index first, magic LAST — the same publish order the pads use.
        let shm = channel.data();
        shm.store_u32(OFF_PAD_INDEX, u32::from(index), Ordering::Relaxed);
        shm.store_u32(OFF_MAGIC, MOUSE_MAGIC, Ordering::Relaxed);
        let instance = format!("pf_mouse_{index}");
        let (sw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &instance,
            container_tag: 0x5046_4D4F, // "PFMO" — never grouped with a pad's container
            container_index: index,
            hwid: "pf_mouse",
            // Virtual identity (PF:MO). USB tokens are inert for a mouse; shared profile = one path.
            usb_vid_pid: Some("VID_5046&PID_4D4F"),
            usb_mi: None,
            bluetooth: false,
            description: "Punktfunk Virtual Mouse",
            enumerator: "punktfunk",
        }) {
            Ok((sw, id)) => (Some(sw), id),
            // Without a devnode the sealed channel refuses the mailbox pid, so the mouse would
            // never come up: fail, and the keeper retries. The trusted mailbox can still serve.
            Err(e) if std::env::var_os(TRUST_MAILBOX_ENV).is_none() => {
                return Err(e.context("create the pf_mouse devnode"));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_mouse devnode");
                (None, None)
            }
        };
        // Bind to this devnode's serving pid, not the LocalService-writable mailbox.
        channel.bind_devnode(
            u32::from(index),
            instance_id.clone(),
            ProofTransport::HidSerialString,
        );
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(VirtualMouse {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                "pf_mouse",
                "pf_mouse.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pfmouse-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
        })
    }

    /// Publish a 5-bit-button / 15-bit-abs / wheel report; bump `in_seq` (never 0).
    pub fn send_report(&mut self, buttons: u8, x: u16, y: u16, wheel: i8, pan: i8) {
        let r = input_report(buttons, x, y, wheel, pan);
        self.seq = self.seq.wrapping_add(1).max(1); // never publish seq 0 (= "nothing yet")
                                                    // The report before the seq (Release): the driver's Acquire load of `in_seq` observes
                                                    // the matching report.
        let shm = self.channel.data();
        shm.write_bytes(OFF_REPORT, &r);
        shm.store_u32(OFF_IN_SEQ, self.seq, Ordering::Release);
    }

    /// Pump sealed-channel delivery and feed the attach watcher (8 ms timer stamps `driver_proto`).
    pub fn service(&mut self) {
        self.channel.pump();
        self.attach.observe(self.driver_proto());
    }

    fn driver_proto(&self) -> u32 {
        self.channel
            .data()
            .load_u32(OFF_DRIVER_PROTO, Ordering::Relaxed)
    }

    fn driver_heartbeat(&self) -> u32 {
        self.channel
            .data()
            .load_u32(OFF_DRIVER_HEARTBEAT, Ordering::Relaxed)
    }
}

/// Ensure the one process-wide virtual mouse exists. Called from
/// [`InjectorService`](crate::InjectorService) start; native + GameStream share it.
/// The keeper thread owns the devnode for the process lifetime.
///
/// `PUNKTFUNK_NO_VIRTUAL_MOUSE=1` opts out.
pub(crate) fn ensure_resident() {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        if std::env::var_os("PUNKTFUNK_NO_VIRTUAL_MOUSE").is_some_and(|v| v != "0") {
            tracing::info!(
                "virtual HID mouse disabled (PUNKTFUNK_NO_VIRTUAL_MOUSE) — with no physical \
                 pointer attached, Windows will not draw a cursor into the stream"
            );
            return;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-vmouse".into())
            .spawn(keeper_thread)
        {
            tracing::warn!(error = %e, "virtual-mouse keeper thread spawn failed");
        }
    });
}

/// Open-with-retry, then hold + pump every 250 ms. Open fails on a mailbox squat (another
/// host); a missing driver is not an open failure (`DriverAttach` diagnoses via the pump).
/// The device only exists: it never reports motion of its own.
fn keeper_thread() {
    loop {
        match VirtualMouse::open() {
            Ok(mut m) => {
                tracing::info!(
                    "resident virtual HID mouse created (pf_mouse — keeps SM_MOUSEPRESENT true \
                     so DWM composites the cursor on headless hosts)"
                );
                loop {
                    m.service();
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "virtual HID mouse open failed — retrying in 60s (headless hosts stream an \
                     invisible cursor until it exists)"
                );
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// `vmouse-spike`: drive the real cursor through HID reports. Stop the host
/// service first (it owns the mailbox). Expect `pf_mouse` + HID child,
/// `SM_MOUSEPRESENT` = 1 with no physical mouse, and a mid-screen sweep.
pub fn spike_hold(secs: u64) -> Result<()> {
    let mut m = VirtualMouse::open()?;
    println!("virtual HID mouse devnode up (5046:4D4F) — waiting for the driver to attach…");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while m.driver_proto() == 0 && std::time::Instant::now() < deadline {
        m.service();
        std::thread::sleep(Duration::from_millis(50));
    }
    if m.driver_proto() == 0 {
        println!(
            "driver never attached (10s). Install it: punktfunk-host.exe driver install --gamepad \
             --dir <stage>  (pf_mouse.inf ships with the gamepad drivers); see the WARN above."
        );
    } else {
        println!(
            "driver attached (proto {}). Sweeping the cursor for {secs}s — watch the glass: the \
             pointer should glide left↔right across mid-screen; wheel ticks every second.",
            m.driver_proto()
        );
    }
    let t0 = std::time::Instant::now();
    let mut i: u64 = 0;
    let beat_before = m.driver_heartbeat();
    while t0.elapsed() < Duration::from_secs(secs) {
        // Triangle-wave X over the middle 3/4, fixed mid Y; one wheel tick per second.
        let phase = (i % 240) as i32; // 240 steps × 16 ms ≈ 4 s per round trip
        let tri = if phase < 120 { phase } else { 240 - phase };
        let x = 4096 + (tri as u32 * (24576 / 120)) as u16;
        let wheel: i8 = if i % 60 == 0 { 1 } else { 0 };
        m.send_report(0, x, 0x4000, wheel, 0);
        m.service();
        i += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    let beat = m.driver_heartbeat();
    println!(
        "vmouse-spike: done (driver heartbeat advanced {} ticks — {}). Devnode removed on exit.",
        beat.wrapping_sub(beat_before),
        if beat != beat_before {
            "driver alive"
        } else {
            "driver NOT ticking"
        }
    );
    Ok(())
}

/// Throwaway `pf_mouse_probe` at pad index 9: print which HID IOCTL hidclass
/// forwards to a UMDF minidriver (`HidD_GetIndexedString` vs `IOCTL_HID_GET_STRING`).
/// The mailbox is LocalService-writable so the pad channel trusts the devnode's
/// [`pf_driver_proto::gamepad::ChannelProof`] instead. Needs `pf_mouse` installed.
pub fn channel_proof_probe() -> Result<()> {
    use crate::channel_proof::{self, ProofTransport};

    /// A pad index no real pad uses, so the proof's index check is actually exercised and the
    /// probe can never be confused with the resident mouse at 0.
    const PROBE_INDEX: u8 = 9;

    println!("creating a throwaway pf_mouse devnode (pad index {PROBE_INDEX})…");
    let (_sw, instance_id) = create_swdevice(&SwDeviceProfile {
        instance: "pf_mouse_probe",
        container_tag: 0x5046_4D4F, // "PFMO"
        container_index: PROBE_INDEX,
        hwid: "pf_mouse",
        usb_vid_pid: Some("VID_5046&PID_4D4F"),
        usb_mi: None,
        bluetooth: false,
        description: "Punktfunk Virtual Mouse (channel-proof probe)",
        enumerator: "punktfunk",
    })?;
    let Some(instance_id) = instance_id else {
        anyhow::bail!("SwDeviceCreate reported no instance id to look the devnode up by");
    };

    // Poll: PnP + hidclass publish in tens of ms; a fixed sleep would miss a slow box.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let report = loop {
        let r = channel_proof::diagnose(
            &instance_id,
            ProofTransport::HidSerialString,
            PROBE_INDEX as u32,
        );
        if r.contains("ChannelProof") || std::time::Instant::now() >= deadline {
            break r;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    println!("\n{report}");

    match channel_proof::probe_pid(
        &instance_id,
        ProofTransport::HidSerialString,
        PROBE_INDEX as u32,
    ) {
        Ok(pid) => println!(
            "RESULT: the devnode proved its driver is pid {pid} — the HID channel proof WORKS on \
             this build of Windows, so the pad channel never has to trust the mailbox."
        ),
        Err(e) => println!(
            "RESULT: no usable channel proof ({e:#}).\n\
             If BOTH HID lines above say \"call failed\", hidclass on this build forwards neither \
             IOCTL to a UMDF minidriver and the HID pads/mouse need a different transport (the \
             xusb leg is unaffected — it owns its own device interface). If one says \"answered, \
             but not a proof\", an OLD pf_mouse driver is installed: reinstall with\n\
             \x20  punktfunk-host.exe driver install --gamepad"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Two seat hosts must never derive the same mouse index, and the console keeps 0.
    #[test]
    fn only_a_reserved_seat_connector_moves_the_mouse_off_index_zero() {
        assert_eq!(mouse_index_for_slot(None), 0);
        for refused in [
            "", "0", "11", "16", "255", "999", "12.0", "-1", " 12", "12 ",
        ] {
            assert_eq!(
                mouse_index_for_slot(Some(OsStr::new(refused))),
                0,
                "{refused}"
            );
        }
        let mut seen = Vec::new();
        for slot in FIRST_SEAT_SLOT..=LAST_SEAT_SLOT {
            let index = mouse_index_for_slot(Some(OsStr::new(&slot.to_string())));
            assert_eq!(index, slot);
            assert!(
                !seen.contains(&index),
                "seat slot {slot} reused index {index}"
            );
            seen.push(index);
        }
        assert!(
            !seen.contains(&0),
            "a seat must not land on the console index"
        );
    }
}
