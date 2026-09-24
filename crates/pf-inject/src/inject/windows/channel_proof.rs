//! Ask a devnode which process is serving it — the host half of
//! [`pf_driver_proto::gamepad::ChannelProof`].
//!
//! The pad channel duplicates the unnamed DATA section into this pid, not the
//! bootstrap mailbox's `driver_pid`. The mailbox is LocalService-writable (WUDFHost
//! must open it) and `verify_is_wudfhost` only checks the image path of a
//! world-executable `WUDFHost.exe`. I/O to the instance id `SwDeviceCreate` returned
//! reaches only the driver PnP bound to that device.
//!
//! Three transports, one per driver shape — see [`ProofTransport`]. Evidence:
//! `design/gamepad-channel-sealing.md`, `pf_umdf_util::hid`.

use anyhow::{anyhow, bail, Context, Result};
use pf_driver_proto::gamepad::{
    ChannelProof, DECK_PROOF_CMD, HID_FEATURE_REPORT_CHANNEL_PROOF, HID_STRING_INDEX_CHANNEL_PROOF,
    IOCTL_PF_GET_CHANNEL_PROOF,
};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Child, CM_Get_Device_IDW, CM_Get_Device_Interface_ListW,
    CM_Get_Device_Interface_List_SizeW, CM_Get_Sibling, CM_Locate_DevNodeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetIndexedString,
    HidD_GetPreparsedData, HidD_GetProductString, HidD_GetSerialNumberString, HidD_SetFeature,
    HidP_GetCaps, GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// Interface `pf_xusb` registers on its own devnode — also what `xinput1_4` enumerates.
const GUID_DEVINTERFACE_XUSB: GUID = GUID::from_u128(0xEC87F1E3_C13B_4100_B5F7_8B84D54260CB);

/// How a driver answers the proof. One variant per driver shape: hidclass sits on the HID
/// minidrivers and swallows a private IOCTL, so they cannot share `pf_xusb`'s path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofTransport {
    /// `pf_xusb`: private IOCTL on [`GUID_DEVINTERFACE_XUSB`]. Not a HID minidriver, so it owns
    /// `IRP_MJ_CREATE` and its own IOCTL dispatch — hidclass is not sitting above it.
    XusbIoctl,
    /// `pf_mouse`: the proof is the HID serial-number string. `HidD_GetSerialNumberString` on a
    /// zero-access handle is the named-string IOCTL hidclass forwards to a UMDF HID minidriver.
    /// Safe here because nothing reads the virtual mouse's serial.
    HidSerialString,
    /// `pf_gamepad`: a HID feature report. Pad serials are what SDL and Steam dedup on, so they
    /// cannot share the mouse's serial transport. Costs no descriptor change: DualSense / DualShock 4
    /// / Edge already declare unused feature `0x85`; the Deck's unnumbered feature is command→response
    /// and the proof rides a private two-byte command.
    HidFeatureReport,
}

/// Ask the `SwDeviceCreate` instance `instance_id` which process serves it. The pid is already
/// checked against `expect_pad_index` and this build's protocol version; every error is a
/// refusal to deliver (missing devnode vs. unbound vs. a proof we did not mint).
pub(super) fn query(
    instance_id: &str,
    transport: ProofTransport,
    expect_pad_index: u32,
) -> Result<u32> {
    let paths = match transport {
        ProofTransport::XusbIoctl => interface_paths(&GUID_DEVINTERFACE_XUSB, instance_id)
            .with_context(|| format!("enumerate the XUSB interface on {instance_id}"))?,
        ProofTransport::HidSerialString | ProofTransport::HidFeatureReport => {
            // hidclass publishes the collection interface on a CHILD PDO, not on our devnode.
            let children = child_device_ids(instance_id)
                .with_context(|| format!("enumerate the HID children of {instance_id}"))?;
            let mut all = Vec::new();
            for child in &children {
                all.extend(interface_paths(&GUID_DEVINTERFACE_HID, child).unwrap_or_default());
            }
            all
        }
    };
    if paths.is_empty() {
        bail!(
            "no device interface for {instance_id} yet — the driver has not finished starting (or \
             is not bound to this devnode at all)"
        );
    }
    // One devnode can publish several collections; take the first well-formed proof, not the first answer.
    let mut last_err = None;
    for path in &paths {
        let r = match transport {
            ProofTransport::XusbIoctl => ask_ioctl(path, expect_pad_index),
            ProofTransport::HidSerialString => ask_hid_path(path, expect_pad_index),
            ProofTransport::HidFeatureReport => ask_feature_path(path, expect_pad_index),
        };
        match r {
            Ok(pid) => return Ok(pid),
            Err(e) => last_err = Some(e.context(format!("ask {path}"))),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no interface answered a channel proof")))
}

/// Private IOCTL on `pf_xusb`'s own interface (not a HID collection).
fn ask_ioctl(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    let proof = ask_xusb(HANDLE(handle.as_raw_handle()))?;
    proof
        .check(expect_pad_index)
        .map_err(|why| anyhow!("{why}"))
}

fn ask_feature_path(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    ask_feature(HANDLE(handle.as_raw_handle()), expect_pad_index)
}

/// Return a pid only if [`ChannelProof::check`] accepts it. [`ChannelProof::from_feature_report`]
/// reinterprets any 17 bytes, so a foreign report becomes a proof-shaped struct of junk.
fn accept(
    p: Option<ChannelProof>,
    expect: u32,
    rejected: &mut Option<&'static str>,
) -> Option<u32> {
    match p?.check(expect) {
        Ok(pid) => Some(pid),
        Err(why) => {
            *rejected = Some(why);
            None
        }
    }
}

/// Parse a SET→GET reply. The driver writes `[DECK_PROOF_CMD, ChannelProof, zeros…]`; hidclass
/// returns it either as served (offset 0) or one byte in, behind the report-id slot. Accept both
/// rather than pinning a marshalling that differs across the descriptors this one driver serves.
fn proof_from_reply(reply: &[u8], expect: u32, rejected: &mut Option<&'static str>) -> Option<u32> {
    for off in [0usize, 1] {
        let Some(body) = reply.get(off..) else {
            continue;
        };
        if let Some(tail) = body.strip_prefix(&DECK_PROOF_CMD[..])
            && let Some(pid) = accept(ChannelProof::from_bytes(tail), expect, rejected)
        {
            return Some(pid);
        }
    }
    None
}

/// Probe every identity this one driver binary can become. DualSense / DualShock 4 / Edge answer
/// on declared-but-unserved `0x85`; the Deck answers its unnumbered report after a private
/// SET_FEATURE command; Triton answers that same command on declared feature id `0x01`. The host
/// does not know which identity this devnode became until the DATA section is attached — the
/// thing this proof is trying to earn.
///
/// Shape is not enough: a Deck serves its one unnumbered feature for *any* requested id, so the
/// `0x85` probe returns Steam attribute bytes that parse as a proof and fail on magic. Returning
/// that first answer stopped the search while the real proof sat one transport away.
fn ask_feature(h: HANDLE, expect_pad_index: u32) -> Result<u32> {
    // HidD_{Get,Set}Feature reject a buffer shorter than `FeatureReportByteLength`. PS identities
    // land on 64 (63 + report id); the Deck's unnumbered 64-byte feature is reported as 65
    // (payload + the reserved report-id slot). Hardcoding 64 failed every correctly-enumerated Deck.
    let buf_len = feature_report_len(h).unwrap_or(64).max(64);
    let mut rejected = None;

    let mut buf = vec![0u8; buf_len];
    buf[0] = HID_FEATURE_REPORT_CHANNEL_PROOF;
    // SAFETY: `h` is the live HID interface handle; `buf` is a valid `buf_len`-sized in/out buffer.
    if unsafe { HidD_GetFeature(h, buf.as_mut_ptr().cast(), buf_len as u32) } {
        let p = ChannelProof::from_feature_report(&buf);
        if let Some(pid) = accept(p, expect_pad_index, &mut rejected) {
            return Ok(pid);
        }
    }

    // Deck: SET the private command, then GET. Byte 0 stays 0 — unnumbered report id.
    let mut cmd = vec![0u8; buf_len];
    cmd[1..1 + DECK_PROOF_CMD.len()].copy_from_slice(&DECK_PROOF_CMD);
    // SAFETY: `h` is live; `cmd` is a valid `buf_len`-sized buffer.
    let set_ok = unsafe { HidD_SetFeature(h, cmd.as_mut_ptr().cast(), buf_len as u32) };
    if set_ok {
        let mut reply = vec![0u8; buf_len];
        // SAFETY: as above.
        if unsafe { HidD_GetFeature(h, reply.as_mut_ptr().cast(), buf_len as u32) }
            && let Some(pid) = proof_from_reply(&reply, expect_pad_index, &mut rejected)
        {
            return Ok(pid);
        }
    }

    // Triton (numbered): same SET→GET command, framed id-first. hidclass rejects a feature
    // buffer whose byte 0 is not a declared nonzero feature id — Triton's 0x85 is OUTPUT, so
    // both legs above die at hidclass. Ride declared id 0x01; the driver strips a leading
    // 0x00 or 0x01 before matching (`triton_proof_requested`).
    let mut cmd = vec![0u8; buf_len];
    cmd[0] = 0x01;
    cmd[1..1 + DECK_PROOF_CMD.len()].copy_from_slice(&DECK_PROOF_CMD);
    // SAFETY: `h` is live; `cmd` is a valid `buf_len`-sized buffer.
    let set_ok_numbered = unsafe { HidD_SetFeature(h, cmd.as_mut_ptr().cast(), buf_len as u32) };
    if set_ok_numbered {
        let mut reply = vec![0u8; buf_len];
        // The GET buffer must name a declared feature id too — the same hidclass gate.
        reply[0] = 0x01;
        // SAFETY: as above.
        if unsafe { HidD_GetFeature(h, reply.as_mut_ptr().cast(), buf_len as u32) }
            && let Some(pid) = proof_from_reply(&reply, expect_pad_index, &mut rejected)
        {
            return Ok(pid);
        }
    }
    // A well-formed answer that failed validation is not "no proof" — surface the check that refused.
    if let Some(why) = rejected {
        bail!("{why}");
    }
    bail!(
        "this HID collection carries no channel proof (feature 0x{:02x}: no; Deck command: {}; \
         numbered command: {}) — the driver predates the proof (reinstall: punktfunk-host.exe \
         driver install --gamepad)",
        HID_FEATURE_REPORT_CHANNEL_PROOF,
        if set_ok {
            "no matching reply"
        } else {
            "SET_FEATURE failed"
        },
        if set_ok_numbered {
            "no matching reply"
        } else {
            "SET_FEATURE failed"
        },
    )
}

/// This collection's `FeatureReportByteLength` — the buffer size `HidD_GetFeature` /
/// `HidD_SetFeature` demand. `None` when the preparsed data can't be read (caller falls back).
fn feature_report_len(h: HANDLE) -> Option<usize> {
    let mut pp = PHIDP_PREPARSED_DATA::default();
    // SAFETY: `h` is the live HID interface handle; `pp` receives an owned preparsed-data handle.
    if !unsafe { HidD_GetPreparsedData(h, &mut pp) } {
        return None;
    }
    let mut caps = HIDP_CAPS::default();
    // SAFETY: `pp` is the handle just obtained (freed below); `caps` is a valid out-param.
    let st = unsafe { HidP_GetCaps(pp, &mut caps) };
    // SAFETY: `pp` came from `HidD_GetPreparsedData` and is not used after this.
    let _ = unsafe { HidD_FreePreparsedData(pp) };
    (st.0 >= 0 && caps.FeatureReportByteLength > 0).then_some(caps.FeatureReportByteLength as usize)
}

fn ask_hid_path(path: &str, expect_pad_index: u32) -> Result<u32> {
    let handle = open_device(path)?;
    let proof = ask_hid(HANDLE(handle.as_raw_handle()))?;
    proof
        .check(expect_pad_index)
        .map_err(|why| anyhow!("{why}"))
}

/// VID/PID the driver reports on `instance_id`'s HID collection (`HidD_GetAttributes`): what SDL,
/// Steam and Windows read. `None` until hidclass has published the collection.
pub(super) fn hid_vid_pid(instance_id: &str) -> Option<(u16, u16)> {
    for child in child_device_ids(instance_id).ok()? {
        for path in interface_paths(&GUID_DEVINTERFACE_HID, &child).unwrap_or_default() {
            let Ok(handle) = open_device(&path) else {
                continue;
            };
            let mut attrs = HIDD_ATTRIBUTES {
                Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
                ..Default::default()
            };
            // SAFETY: `handle` is the live collection handle just opened; `attrs` is a valid,
            // size-stamped out-param.
            if unsafe { HidD_GetAttributes(HANDLE(handle.as_raw_handle()), &mut attrs) } {
                return Some((attrs.VendorID, attrs.ProductID));
            }
        }
    }
    None
}

/// Same call the delivery path makes, for the `channel-proof-probe` subcommand — reports the
/// production answer rather than a re-implementation of it.
pub fn probe_pid(
    instance_id: &str,
    transport: ProofTransport,
    expect_pad_index: u32,
) -> Result<u32> {
    query(instance_id, transport, expect_pad_index)
}

/// Walk the same lookup [`query`] does, reporting each step. For the HID serial transport,
/// print which of `HidD_GetIndexedString` / `HidD_GetSerialNumberString` hidclass actually
/// forwarded — hidclass currently swallows an arbitrary indexed-string request to a UMDF HID
/// minidriver (`punktfunk-host channel-proof-probe`).
pub fn diagnose(instance_id: &str, transport: ProofTransport, expect_pad_index: u32) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "devnode : {instance_id}");
    let _ = writeln!(
        out,
        "transport: {transport:?}, expected pad index {expect_pad_index}"
    );

    let paths = match transport {
        ProofTransport::XusbIoctl => match interface_paths(&GUID_DEVINTERFACE_XUSB, instance_id) {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(out, "  XUSB interface lookup FAILED: {e:#}");
                return out;
            }
        },
        ProofTransport::HidSerialString | ProofTransport::HidFeatureReport => {
            let children = child_device_ids(instance_id).unwrap_or_default();
            let _ = writeln!(out, "  hidclass children: {}", children.len());
            let mut all = Vec::new();
            for c in &children {
                let p = interface_paths(&GUID_DEVINTERFACE_HID, c).unwrap_or_default();
                let _ = writeln!(out, "    {c} -> {} HID interface(s)", p.len());
                all.extend(p);
            }
            all
        }
    };
    if paths.is_empty() {
        let _ = writeln!(
            out,
            "  NO device interface — the driver has not started, or is not bound to this devnode"
        );
        return out;
    }
    for path in &paths {
        let _ = writeln!(out, "  interface {path}");
        let handle = match open_device(path) {
            Ok(h) => h,
            Err(e) => {
                let _ = writeln!(out, "    open FAILED: {e:#}");
                continue;
            }
        };
        let h = HANDLE(handle.as_raw_handle());
        match transport {
            ProofTransport::XusbIoctl => match ask_xusb(h) {
                Ok(p) => {
                    let _ = writeln!(out, "    IOCTL_PF_GET_CHANNEL_PROOF -> {p:?}");
                    let _ = writeln!(out, "    check: {:?}", p.check(expect_pad_index));
                }
                Err(e) => {
                    let _ = writeln!(out, "    IOCTL_PF_GET_CHANNEL_PROOF FAILED: {e:#}");
                }
            },
            ProofTransport::HidFeatureReport => {
                let _ = writeln!(
                    out,
                    "    feature report length: {:?}",
                    feature_report_len(h)
                );
                match ask_feature(h, expect_pad_index) {
                    Ok(pid) => {
                        let _ = writeln!(out, "    feature proof -> wudf pid {pid}");
                    }
                    Err(e) => {
                        let _ = writeln!(out, "    feature proof FAILED: {e:#}");
                    }
                }
            }
            ProofTransport::HidSerialString => {
                let (indexed, serial, control) = ask_hid_both(h);
                let _ = writeln!(out, "    HidD_GetSerialNumberString   -> {serial}");
                let _ = writeln!(out, "    HidD_GetIndexedString(proof) -> {indexed}");
                let _ = writeln!(out, "    HidD_GetProductString        -> {control}");
            }
        }
    }
    out
}

/// Indexed-string, serial, and `HidD_GetProductString` on the same zero-access handle. The
/// product string is a known-good control: it succeeds against a UMDF HID minidriver on the
/// handle where `HidD_GetIndexedString` fails for every index (see [`ask_hid`]).
fn ask_hid_both(h: HANDLE) -> (String, String, String) {
    let text = |ok: bool, buf: &[u16]| -> String {
        if !ok {
            let e = std::io::Error::last_os_error();
            return format!("call failed ({e})");
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    };
    let mut a = [0u16; 128];
    let bytes = (a.len() * 2) as u32;
    // SAFETY: `h` is the live HID interface handle; `a` is a valid `bytes`-sized out-buffer.
    let ok_a = unsafe {
        HidD_GetIndexedString(
            h,
            HID_STRING_INDEX_CHANNEL_PROOF,
            a.as_mut_ptr().cast(),
            bytes,
        )
    };
    let indexed = match (ok_a, decode_proof(&a)) {
        (true, Some(p)) => format!("{p:?}"),
        (true, None) => format!("answered, but not a proof: {:?}", text(true, &a)),
        (false, _) => text(false, &a),
    };
    let mut c = [0u16; 128];
    // SAFETY: as above — live handle, valid out-buffer.
    let ok_c = unsafe { HidD_GetSerialNumberString(h, c.as_mut_ptr().cast(), bytes) };
    let serial = match (ok_c, decode_proof(&c)) {
        (true, Some(p)) => format!("{p:?}"),
        (true, None) => format!("plain serial, no proof: {:?}", text(true, &c)),
        (false, _) => text(false, &c),
    };
    let mut b = [0u16; 128];
    // SAFETY: as above — live handle, valid out-buffer.
    let ok_b = unsafe { HidD_GetProductString(h, b.as_mut_ptr().cast(), bytes) };
    let control = format!(
        "{} (control: proves user mode reaches this driver)",
        text(ok_b, &b)
    );
    (indexed, serial, control)
}

/// `CreateFileW` with no access rights. Enough for `FILE_ANY_ACCESS` IOCTLs and `HidD_*`,
/// and the only open that works on a HID mouse/keyboard collection — Windows refuses a
/// user-mode read handle on those.
fn open_device(path: &str) -> Result<OwnedHandle> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path for the duration of the call; the returned
    // handle is owned solely by the `OwnedHandle` built from it.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .with_context(|| format!("CreateFileW({path})"))?
    };
    // SAFETY: `h` is the fresh handle just opened, moved into a single owner that closes it on drop.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0 as _) })
}

/// Private METHOD_BUFFERED IOCTL; 16 is [`ChannelProof`]'s wire size.
fn ask_xusb(h: HANDLE) -> Result<ChannelProof> {
    let mut buf = [0u8; 16];
    let mut returned = 0u32;
    // SAFETY: `h` is the live interface handle; `buf` is a valid 16-byte out-buffer and `returned`
    // a valid out-param. METHOD_BUFFERED, so the I/O manager copies — no pointer escapes.
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_PF_GET_CHANNEL_PROOF,
            None,
            0,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
        .context("IOCTL_PF_GET_CHANNEL_PROOF")?;
    }
    ChannelProof::from_bytes(&buf[..returned as usize]).ok_or_else(|| {
        anyhow!("the XUSB interface returned {returned} bytes, too short for a channel proof")
    })
}

/// `pf_mouse` serial-number string. Named string IOCTLs (`HidD_GetSerialNumberString` and
/// friends) reach a UMDF HID minidriver; an arbitrary indexed-string request does not.
///
/// Do not re-add a user-mode `IOCTL_HID_GET_STRING`: METHOD_NEITHER and kernel-facing —
/// hidclass sends it down to the minidriver. User mode never sends it up.
fn ask_hid(h: HANDLE) -> Result<ChannelProof> {
    let mut buf = [0u16; 128];
    let bytes = (buf.len() * 2) as u32;
    // SAFETY: `h` is the live HID interface handle; `buf` is a valid `bytes`-sized out-buffer.
    if unsafe { HidD_GetSerialNumberString(h, buf.as_mut_ptr().cast(), bytes) } {
        if let Some(p) = decode_proof(&buf) {
            return Ok(p);
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        bail!(
            "this HID collection's serial is {:?}, not a channel proof — an old driver is installed \
             (reinstall: punktfunk-host.exe driver install --gamepad)",
            String::from_utf16_lossy(&buf[..end])
        );
    }
    bail!("HidD_GetSerialNumberString failed on this HID collection")
}

fn decode_proof(buf: &[u16]) -> Option<ChannelProof> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16(&buf[..end]).ok()?;
    ChannelProof::from_hid_string(&s)
}

fn interface_paths(class: &GUID, device_id: &str) -> Result<Vec<String>> {
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let mut len = 0u32;
    // SAFETY: `len` is a valid out-param; `wide` is a NUL-terminated id valid for the call.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            class,
            PCWSTR(wide.as_ptr()),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is `len` UTF-16 units as the size call just reported; same id/class as above.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            PCWSTR(wide.as_ptr()),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        bail!("CM_Get_Device_Interface_ListW failed (CONFIGRET {})", cr.0);
    }
    // A REG_MULTI_SZ-shaped list: NUL-separated, double-NUL terminated.
    Ok(buf
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// Immediate children of `instance_id`. For a HID minidriver those are the collection PDOs
/// hidclass created under our devnode — the HID interface lives there, not on the parent.
fn child_device_ids(instance_id: &str) -> Result<Vec<String>> {
    let wide: Vec<u16> = instance_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut devinst = 0u32;
    // SAFETY: `devinst` is a valid out-param; `wide` is a NUL-terminated id valid for the call.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        bail!(
            "CM_Locate_DevNodeW({instance_id}) failed (CONFIGRET {})",
            cr.0
        );
    }
    let mut out = Vec::new();
    let mut child = 0u32;
    // SAFETY: `devinst` is the devnode just located; `child` is a valid out-param.
    if unsafe { CM_Get_Child(&mut child, devinst, 0) } != CR_SUCCESS {
        return Ok(out); // no children yet — hidclass has not enumerated the collections
    }
    loop {
        let mut buf = [0u16; 512];
        // SAFETY: `child` is a live devnode handle from CM_Get_Child/CM_Get_Sibling; `buf` is a
        // valid out-buffer whose length the binding passes along.
        if unsafe { CM_Get_Device_IDW(child, &mut buf, 0) } == CR_SUCCESS {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            out.push(String::from_utf16_lossy(&buf[..end]));
        }
        let mut next = 0u32;
        // SAFETY: as above — `child` is live, `next` is a valid out-param.
        if unsafe { CM_Get_Sibling(&mut next, child, 0) } != CR_SUCCESS {
            break;
        }
        child = next;
    }
    Ok(out)
}
