//! A pad's software devnode: `SwDeviceCreate` with the hardware, compatible and container
//! ids games look for, fenced to this host's session, and removed on drop.

use super::attach::devnode_status_line;
use crate::channel_proof;
use crate::pad_slots::PadCreateFault;
use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{w, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_SessionId, DEVPROPCOMPKEY, DEVPROPERTY, DEVPROPTYPE, DEVPROP_STORE_SYSTEM,
    DEVPROP_TYPE_BINARY, DEVPROP_TYPE_UINT32,
};
use windows::Win32::Foundation::{CloseHandle, DEVPROPKEY, E_FAIL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// `SwDeviceCreate` completion context: event, HRESULT, and PnP instance id.
/// [`create_swdevice`] blocks on the event.
#[repr(C)]
struct SwCreateCtx {
    event: HANDLE,
    result: HRESULT,
    instance_id: [u16; 128],
}

/// `SwDeviceCreate` callback: stash result + instance id and wake the creator.
/// The creator blocks on the event, so there is no concurrent access to `*ctx`.
unsafe extern "system" fn sw_create_cb(
    _dev: HSWDEVICE,
    result: HRESULT,
    ctx: *const c_void,
    id: PCWSTR,
) {
    if !ctx.is_null() {
        // SAFETY: ctx is the &mut SwCreateCtx the creator passed; it outlives this callback (the
        // creator blocks on the event). `id` is a NUL-terminated string for the callback's duration.
        unsafe {
            let c = ctx as *mut SwCreateCtx;
            (*c).result = result;
            if !id.is_null() {
                for i in 0..(*c).instance_id.len() - 1 {
                    let ch = *id.0.add(i);
                    (*c).instance_id[i] = ch;
                    if ch == 0 {
                        break;
                    }
                }
            }
            let _ = SetEvent((*c).event);
        }
    }
}

impl SwCreateCtx {
    fn instance_id(&self) -> Option<String> {
        let len = self.instance_id.iter().position(|&c| c == 0)?;
        (len > 0).then(|| String::from_utf16_lossy(&self.instance_id[..len]))
    }
}

/// A `SwDeviceCreate`'d devnode; drop removes it (`SwDeviceClose`).
pub(crate) struct SwDevice(HSWDEVICE);

/// PnP identity for a virtual devnode, so one [`create_swdevice`] builds every pad, the XUSB
/// pad and the mouse.
pub(crate) struct SwDeviceProfile<'a> {
    /// Distinct namespaces per type (`pf_pad_<idx>` vs `pf_ds4_<idx>`) so two types never reuse
    /// a devnode shell.
    pub instance: &'a str,
    /// `Data1` of the ContainerId — a per-family tag (`"PFDS"`, `"PFXB"`, `"PFMO"` mouse, …) so
    /// two families at the same index never share a container (Windows would group them as one
    /// device).
    pub container_tag: u32,
    /// Also stamped into the devnode Location, which the driver reads as its bootstrap-mailbox
    /// index.
    pub container_index: u8,
    /// INF-matched hardware id, listed first so the INF binds.
    pub hwid: &'static str,
    /// `VID_…&PID_…` behind the synthesized `USB\` hardware and compatible ids. `None` lists the
    /// INF id alone: XInput finds the XUSB pad by interface GUID, not VID/PID.
    pub usb_vid_pid: Option<&'a str>,
    /// Appended as `&MI_xx` on the USB hardware ids. hidclass mirrors the parent's `USB\VID…`
    /// tokens into the HID child; hidapi/SDL/Steam parse `MI_` as `bInterfaceNumber` (0 if
    /// absent). The Steam Deck controller lives on interface 2.
    pub usb_mi: Option<u8>,
    /// Present as a classic-Bluetooth HID pad: `BTHENUM\` hardware and compatible ids built from
    /// `usb_vid_pid` instead of `USB\` ones. SDL and Steam read the bus from the compatible ids.
    pub bluetooth: bool,
    pub description: &'a str,
    /// The `SWD\<enumerator>\<instance>` namespace. hidclass names the HID child after it, so a
    /// pad Steam must recognise carries its VID/PID here (`VID_054C&PID_0CE6&MI_03`,
    /// `VID_045E&PID_0B13`): Steam merges a pad's views by that token in the path, and under
    /// `punktfunk` it would list the same pad twice.
    pub enumerator: &'a str,
    /// A binary device property the driver reads at `EvtDeviceAdd`: `(fmtid, pid, bytes)`.
    pub property: Option<(u128, u32, &'a [u8])>,
}

/// The Bluetooth HID service every classic-Bluetooth pad enumerates under.
const BTH_HID_SERVICE: &str = "BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}";

/// `VID_2DC8&PID_6012` → the hardware id Windows gives that pad over Bluetooth,
/// `BTHENUM\{00001124-…}_VID&00022DC8_PID&6012` (`0002`: a USB-IF vendor id).
fn bthenum_id(vid_pid: &str) -> String {
    let vid = vid_pid.get(4..8).unwrap_or("0000");
    let pid = vid_pid.get(13..17).unwrap_or("0000");
    format!("{BTH_HID_SERVICE}_VID&0002{vid}_PID&{pid}")
}

static PAD_SESSION: OnceLock<u32> = OnceLock::new();

/// The session every [`create_swdevice`] devnode is fenced to: this process's own, unless
/// [`set_pad_session`] ran first. `None` only while Windows can't say.
pub fn pad_session() -> Option<u32> {
    if let Some(&session) = PAD_SESSION.get() {
        return Some(session);
    }
    pf_win_display::own_session_id().map(|own| *PAD_SESSION.get_or_init(|| own))
}

/// Fence this process's devnodes to `session` instead of its own. First call wins, so call it
/// before the first device opens.
pub fn set_pad_session(session: u32) {
    let _ = PAD_SESSION.set(session);
}

/// `SwDeviceCreate`'s `pProperties`: the profile's binary property, then
/// `DEVPKEY_Device_SessionId`. hidclass's children inherit the session from birth, which hides
/// them from every other session. Each `Buffer` points into `binary` or `session`.
fn devnode_properties(binary: Option<(u128, u32, &[u8])>, session: &[u8; 4]) -> Vec<DEVPROPERTY> {
    let prop = |key: DEVPROPKEY, ty: DEVPROPTYPE, bytes: &[u8]| DEVPROPERTY {
        CompKey: DEVPROPCOMPKEY {
            Key: key,
            Store: DEVPROP_STORE_SYSTEM,
            LocaleName: PCWSTR::null(),
        },
        Type: ty,
        BufferSize: bytes.len() as u32,
        Buffer: bytes.as_ptr() as *mut c_void,
    };
    let mut props = Vec::with_capacity(2);
    if let Some((fmtid, pid, bytes)) = binary {
        let key = DEVPROPKEY {
            fmtid: GUID::from_u128(fmtid),
            pid,
        };
        props.push(prop(key, DEVPROP_TYPE_BINARY, bytes));
    }
    props.push(prop(DEVPKEY_Device_SessionId, DEVPROP_TYPE_UINT32, session));
    props
}

/// Where a devnode's game-facing interfaces live, so [`verify_fence`] reads the session there.
pub(crate) enum FenceAt {
    /// hidclass's children. `pad` also marks the pad driver unattached when none appears.
    HidChildren { pad: bool },
    /// The devnode itself: `pf-xusb` owns its XUSB interface and has no child.
    Parent,
}

/// Read [`pad_session`] back where `at` says games look. A devnode that does not carry it is
/// [`PadCreateFault::SessionFence`], logged with what was read; the caller drops the
/// [`SwDevice`], which removes it. No HID child within 500 ms is a driver that never bound: an
/// unclassified error carrying the devnode's PnP status, so the driver hint still applies.
pub(crate) fn verify_fence(instance_id: Option<&str>, at: FenceAt) -> Result<()> {
    let (Some(id), Some(want)) = (instance_id, pad_session()) else {
        return Err(anyhow!(
            "the devnode's instance id or this host's session id is unknown"
        ))
        .context(PadCreateFault::SessionFence);
    };
    let nodes = match at {
        FenceAt::Parent => vec![id.to_owned()],
        FenceAt::HidChildren { pad } => hid_children(id, pad)?,
    };
    for node in nodes {
        let read = match channel_proof::get_devnode_session(&node) {
            Ok(Some(s)) if s == want => continue,
            Ok(Some(s)) => s.to_string(),
            Ok(None) => "none".to_owned(),
            Err(e) => format!("{e:#}"),
        };
        tracing::warn!(
            devnode = %node,
            read = %read,
            want,
            "virtual devnode without this host's session id — removing it"
        );
        return Err(anyhow!("{node} carries session {read}, not {want}"))
            .context(PadCreateFault::SessionFence);
    }
    Ok(())
}

/// `id`'s HID children. hidclass makes them ~40 ms after the driver starts; the caller has
/// already waited for that, so 500 ms more is the margin.
fn hid_children(id: &str, pad: bool) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let kids = channel_proof::child_device_ids(id)?;
        if !kids.is_empty() {
            return Ok(kids);
        }
        if Instant::now() >= deadline {
            if pad {
                crate::note_pad_driver(crate::PadDriverVerdict::NotAttached);
            }
            return Err(anyhow!(
                "no HID collection under the new devnode: {}",
                devnode_status_line(id, pad)
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Spawn a virtual devnode under `p.enumerator`, fenced to [`pad_session`], and return it with
/// its PnP instance id. No session, no devnode.
///
/// Game detection (`design/windows-dualsense-game-detection.md`): `HIDD_ATTRIBUTES` VID/PID
/// satisfies SDL/HIDAPI/RawInput, but a native PS5 path classifies connection type by walking
/// to the parent and matching `"USB"`/`"BTHENUM"` in `DEVPKEY_Device_CompatibleIds`. Set these
/// via `SW_DEVICE_CREATE_INFO` only — a later `DEVPROPERTY` write of bus/identity keys is ignored:
/// - `pszzCompatibleIds` starts with a `USB\` token so the parent walk resolves USB.
/// - `pszzHardwareIds` lists the INF id first, then `USB\VID_…[&REV_0100]`, so hidclass derives
///   `HID\VID_…` child ids a genuine USB DualSense exposes.
/// - a deterministic per-pad `pContainerId` (the null sentinel trips an `xinput1_4` slot skip).
///
/// `pCallback` is mandatory; without it the call yields `E_INVALIDARG`. The caller must be
/// Administrator (the host runs as LocalSystem).
pub(crate) fn create_swdevice(p: &SwDeviceProfile) -> Result<(SwDevice, Option<String>)> {
    let session = pad_session()
        .context("fence the devnode to this host's session: the session id is unknown")
        .context(PadCreateFault::SessionFence)?;
    let multi_sz = |ids: &[&str]| -> Vec<u16> {
        ids.iter()
            .flat_map(|s| s.encode_utf16().chain(std::iter::once(0)))
            .chain(std::iter::once(0))
            .collect()
    };
    let (hwids, compat) = match p.usb_vid_pid {
        Some(vid_pid) if p.bluetooth => {
            let bth = bthenum_id(vid_pid);
            // A `BTHENUM\` token and no `USB` one → native bus-type detection resolves Bluetooth.
            let compat = multi_sz(&[&bth, BTH_HID_SERVICE]);
            (multi_sz(&[p.hwid, &bth]), Some(compat))
        }
        Some(vid_pid) => {
            let mi = p.usb_mi.map(|n| format!("&MI_{n:02}")).unwrap_or_default();
            let usb_rev = format!("USB\\{vid_pid}&REV_0100{mi}");
            let usb = format!("USB\\{vid_pid}{mi}");
            let hwids = multi_sz(&[p.hwid, &usb_rev, &usb]);
            // A `USB\` token first → native bus-type detection resolves USB.
            let compat = multi_sz(&[&usb, "USB\\Class_03&SubClass_00&Prot_00", "USB\\Class_03"]);
            (hwids, Some(compat))
        }
        None => (multi_sz(&[p.hwid]), None),
    };
    let instid = HSTRING::from(p.instance);
    let desc = HSTRING::from(p.description);
    // Pad index in Location — the driver polls its bootstrap mailbox by it.
    let loc = HSTRING::from(p.container_index.to_string());
    let enumerator = HSTRING::from(p.enumerator);
    let container = GUID::from_values(
        p.container_tag,
        0x0000,
        0x0000,
        [0, 0, 0, 0, 0, 0, 0, p.container_index],
    );

    // The id buffers and `container` outlive SwDeviceCreate (we wait on the event before return).
    let info = SW_DEVICE_CREATE_INFO {
        cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32,
        pszInstanceId: PCWSTR(instid.as_ptr()),
        pszzHardwareIds: PCWSTR(hwids.as_ptr()),
        pszzCompatibleIds: compat
            .as_ref()
            .map_or(PCWSTR::null(), |c| PCWSTR(c.as_ptr())),
        pContainerId: &container,
        CapabilityFlags: 0x0000_000B, // DriverRequired | SilentInstall | Removable
        pszDeviceDescription: PCWSTR(desc.as_ptr()),
        pszDeviceLocation: PCWSTR(loc.as_ptr()),
        ..Default::default()
    };

    // `p.property`'s bytes and `session_bytes` outlive the call; SwDeviceCreate only reads them.
    let session_bytes = session.to_ne_bytes();
    let props = devnode_properties(p.property, &session_bytes);

    // SAFETY: a manual-reset, initially-unsignaled, unnamed event.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
    // `result` starts as E_FAIL: a timeout must not read a zeroed HRESULT as success.
    // Heap-allocated: `sw_create_cb` writes through this pointer then `SetEvent`s. The wait is
    // 10 s; on a wedged-PnP timeout the callback may still be pending, so we leak the box and
    // leave the event open rather than let a late write hit recycled memory or a reused handle.
    let ctx = Box::into_raw(Box::new(SwCreateCtx {
        event,
        result: E_FAIL,
        instance_id: [0; 128],
    }));
    // SAFETY: info, `props` + the buffers outlive the call; `ctx` is a live heap allocation that
    // outlives every path below (reclaimed only where the callback provably ran). windows-rs
    // returns the HSWDEVICE (the C out-param) as the Result value.
    let hsw = match unsafe {
        SwDeviceCreate(
            PCWSTR(enumerator.as_ptr()),
            w!("HTREE\\ROOT\\0"),
            &info,
            Some(props.as_slice()),
            Some(sw_create_cb),
            Some(ctx as *const c_void),
        )
    } {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: the call failed, so no callback was registered and `ctx` is ours to reclaim;
            // `event` is valid and unreferenced.
            unsafe {
                drop(Box::from_raw(ctx));
                let _ = CloseHandle(event);
            }
            return Err(anyhow!("SwDeviceCreate({}): {e}", p.instance));
        }
    };
    // From here the handle is ours: every early return removes the devnode.
    let sw = SwDevice(hsw);
    // SAFETY: event is valid.
    let wait = unsafe { WaitForSingleObject(event, 10_000) };
    if wait != WAIT_OBJECT_0 {
        // Timed out: leak `ctx` and leave `event` open so a late callback writes live memory.
        return Err(anyhow!(
            "SwDeviceCreate({}) enumeration callback never fired (10s) — PnP may be wedged",
            p.instance
        ));
    }
    // SAFETY: the callback signalled the event, so nothing else will touch `ctx`/`event`.
    // `ctx` came from `Box::into_raw` above and is reclaimed exactly once here; `event` is
    // valid and no longer referenced by a pending callback.
    let ctx = unsafe {
        let _ = CloseHandle(event);
        Box::from_raw(ctx)
    };
    if ctx.result.is_err() {
        return Err(anyhow!(
            "SwDeviceCreate({}) enumeration: {:?}",
            p.instance,
            ctx.result
        ));
    }
    let instance_id = ctx.instance_id();
    if let Some(id) = &instance_id {
        tracing::debug!(devnode = %id, session, "virtual devnode fenced to this session");
    }
    Ok((sw, instance_id))
}

impl Drop for SwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle `SwDeviceCreate` returned; `SwDeviceClose` removes the devnode.
        unsafe { SwDeviceClose(self.0) };
    }
}

#[cfg(test)]
mod bthenum_tests {
    use super::*;

    /// hidapi reports Bluetooth for a `BTHENUM` compatible id, and USB for any id that says `USB`.
    #[test]
    fn bluetooth_ids_name_the_pad_and_never_usb() {
        let id = bthenum_id("VID_2DC8&PID_6012");
        assert_eq!(
            id,
            "BTHENUM\\{00001124-0000-1000-8000-00805f9b34fb}_VID&00022DC8_PID&6012"
        );
        assert!(!id.to_ascii_uppercase().contains("USB"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every devnode carries a 4-byte UINT32 `SessionId`, after the profile's binary property
    /// when it has one.
    #[test]
    fn every_devnode_carries_the_session_property() {
        let session = 15u32.to_ne_bytes();
        let only = devnode_properties(None, &session);
        assert_eq!(only.len(), 1, "no binary property, only the fence");
        let fence = &only[0];
        assert_eq!(fence.CompKey.Key, DEVPKEY_Device_SessionId);
        assert_eq!(fence.Type, DEVPROP_TYPE_UINT32);
        assert_eq!(fence.BufferSize, 4);
        assert_eq!(fence.Buffer, session.as_ptr() as *mut c_void);

        let blob = [1u8, 2, 3];
        let both = devnode_properties(Some((0x1234, 7, &blob)), &session);
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].CompKey.Key.pid, 7);
        assert_eq!(both[0].Type, DEVPROP_TYPE_BINARY);
        assert_eq!(both[0].BufferSize, 3);
        assert_eq!(both[0].Buffer, blob.as_ptr() as *mut c_void);
        assert_eq!(both[1].CompKey.Key, DEVPKEY_Device_SessionId);
        assert_eq!(both[1].BufferSize, 4);
    }
}
