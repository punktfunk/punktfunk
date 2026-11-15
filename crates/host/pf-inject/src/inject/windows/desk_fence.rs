//! The desk fence: the box's host tags the desk's physical game controllers with the console
//! session, the way every host tags its own virtual pads, so a seat's HID, XUSB and XInput
//! listings and GameInput deliveries leave them alone.
//!
//! A tag written after a device appears is what a process can do; nothing a shipped component
//! writes is earlier, because the session id belongs to the bus that creates the device
//! (`design/desk-pads-and-seats-spike/results.md`, S7). So the fence lands within tens of
//! milliseconds of a plug, is wiped by the device's next start and re-applied on its arrival,
//! does not reach WGI's Xbox listing in a seat, and leaves `xinput1_4`'s box-wide index alone.
//! Devices a Remote Desktop session redirected in carry that session's id already and are
//! never touched; the host's own `SWD` devices carry theirs from creation.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_Locate_DevNodeW, CM_Register_Notification,
    CM_Set_DevNode_PropertyW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL,
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_EVENT_DATA,
    CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_0,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidP_GetCaps,
    GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_InstanceId, DEVPKEY_Device_SessionId, DEVPROPTYPE, DEVPROP_TYPE_EMPTY,
    DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::channel_proof::get_devnode_session;

/// `GUID_DEVINTERFACE_XUSB`: the class `xinput1_4`, WGI and GameInput walk for Xbox pads.
const GUID_DEVINTERFACE_XUSB: GUID = GUID::from_u128(0xec87f1e3_c13b_4100_b5f7_8b84d54260cb);
/// HID usage page 1 (generic desktop) and the usages a game controller's top collection has:
/// joystick, gamepad, multi-axis controller.
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const GAME_USAGES: [u16; 3] = [0x04, 0x05, 0x08];
/// The first vendor-defined HID usage page.
const USAGE_PAGE_VENDOR: u16 = 0xFF00;
/// Valve's vendor id and its controllers: the 2015 Steam Controller on a cable, on Bluetooth
/// and its dongle, the Deck's own pad, the Steam Controller 2 on a cable and on Bluetooth, and
/// its Puck.
const VALVE: u16 = 0x28DE;
const STEAM_PADS: [u16; 8] = [
    0x1102, 0x1106, 0x1142, 0x1205, 0x1302, 0x1303, 0x1304, 0x1305,
];

static ARRIVALS: OnceLock<Mutex<Sender<String>>> = OnceLock::new();
static NOTIFICATION: OnceLock<usize> = OnceLock::new();

/// Fence every physical controller present to `session` and keep fencing the ones that
/// arrive, for the life of the process. Called once by the box's console host.
pub fn start(session: u32) {
    let (tx, rx) = channel::<String>();
    if ARRIVALS.set(Mutex::new(tx)).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pf-desk-fence".into())
        .spawn(move || {
            sweep(session);
            for path in rx {
                if let Err(error) = fence_interface(&path, session) {
                    tracing::warn!(%error, path = %path, "desk controller not fenced");
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "desk fence did not start");
        return;
    }
    for class in [GUID_DEVINTERFACE_HID, GUID_DEVINTERFACE_XUSB] {
        if let Err(error) = watch(class) {
            tracing::warn!(%error, "desk fence is not watching arrivals");
        }
    }
}

/// Take the fence off every physical controller that carries one. For the service's stop and
/// the uninstaller: a tag never outlives the device's next start, but a pad that stays up
/// through a punktfunk removal would otherwise keep hiding from a later console session. The
/// fence's own skips hold here too: a redirected pad's tag and our devices' are not the fence's.
pub fn clear() {
    for (id, _) in candidates() {
        if is_ours(&id) || is_redirected(&id).unwrap_or(true) {
            continue;
        }
        if let Ok(Some(_)) = get_devnode_session(&id)
            && let Err(error) = set_session(&id, None)
        {
            tracing::warn!(%error, devnode = %id, "desk fence not cleared");
        }
    }
}

/// Every present controller now: tag what is not ours and not redirected.
fn sweep(session: u32) {
    let mut fenced = 0usize;
    for (id, path) in candidates() {
        match fence_devnode(&id, &path, session) {
            Ok(true) => fenced += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, devnode = %id, "desk controller not fenced"),
        }
    }
    tracing::info!(
        session,
        fenced,
        "desk controllers fenced to the console session"
    );
}

/// `(instance id, interface path)` of every present HID controller collection
/// ([`is_controller`]) and every present XUSB interface.
fn candidates() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (class, hid) in [
        (GUID_DEVINTERFACE_HID, true),
        (GUID_DEVINTERFACE_XUSB, false),
    ] {
        for path in interfaces(&class).unwrap_or_default() {
            if hid && !is_controller_collection(&path) {
                continue;
            }
            match instance_id_of(&path) {
                Ok(id) => out.push((id, path)),
                Err(error) => tracing::debug!(%error, path = %path, "interface without a devnode"),
            }
        }
    }
    out
}

/// One arrived interface: fence its devnode when it is a controller.
fn fence_interface(path: &str, session: u32) -> Result<()> {
    let is_hid = path
        .to_ascii_lowercase()
        .contains("4d1e55b2-f16f-11cf-88cb-001111000030");
    if is_hid && !is_controller_collection(path) {
        return Ok(());
    }
    let id = instance_id_of(path)?;
    fence_devnode(&id, path, session).map(|_| ())
}

/// Tag one devnode unless it is ours, redirected, or already tagged. `Ok(true)` when a tag was
/// written and read back.
fn fence_devnode(id: &str, path: &str, session: u32) -> Result<bool> {
    if is_ours(id) || is_redirected(id)? {
        return Ok(false);
    }
    if get_devnode_session(id)? == Some(session) {
        return Ok(false);
    }
    set_session(id, Some(session))?;
    match get_devnode_session(id)? {
        Some(read) if read == session => {
            tracing::info!(devnode = %id, path = %path, session, "desk controller fenced to the console session");
            Ok(true)
        }
        read => bail!("{id} reads {read:?} after the write"),
    }
}

/// The host's own virtual devices: `SWD\PUNKTFUNK\…` and `SWD\VID_…\PF_…`, tagged at creation.
fn is_ours(instance_id: &str) -> bool {
    instance_id.to_ascii_uppercase().starts_with("SWD\\")
}

/// Whether an ancestor is the Remote Desktop USB hub: such a device carries its seat's session
/// and must keep it.
fn is_redirected(instance_id: &str) -> Result<bool> {
    let mut devinst = locate(instance_id)?;
    for _ in 0..16 {
        let mut parent = 0u32;
        // SAFETY: `devinst` is a live devnode handle; `parent` is a valid out-param.
        if unsafe { CM_Get_Parent(&mut parent, devinst, 0) } != CR_SUCCESS {
            return Ok(false);
        }
        if is_ts_hub(&device_id(parent)) {
            return Ok(true);
        }
        devinst = parent;
    }
    Ok(false)
}

/// The Remote Desktop USB redirection bus and hub, by the ids Windows gives them.
fn is_ts_hub(instance_id: &str) -> bool {
    let upper = instance_id.to_ascii_uppercase();
    upper.starts_with("ROOT\\RDPBUS") || upper.contains("TS_USB_HUB") || upper.contains("TSUSBHUB")
}

/// Whether the HID collection behind `path` is a controller's ([`is_controller`]). A device
/// that doesn't name its ids is judged by its usage alone.
fn is_controller_collection(path: &str) -> bool {
    let Ok(handle) = open_query(path) else {
        return false;
    };
    let h = HANDLE(handle.as_raw_handle());
    let mut ids = HIDD_ATTRIBUTES {
        Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    // SAFETY: `h` is the live HID interface handle; `ids` is a valid out-param that states its
    // own size.
    if !unsafe { HidD_GetAttributes(h, &mut ids) } {
        ids = HIDD_ATTRIBUTES::default();
    }
    let mut pp = PHIDP_PREPARSED_DATA::default();
    // SAFETY: `h` is the live HID interface handle; `pp` receives an owned preparsed-data handle.
    if !unsafe { HidD_GetPreparsedData(h, &mut pp) } {
        return false;
    }
    let mut caps = HIDP_CAPS::default();
    // SAFETY: `pp` is the handle just obtained (freed below); `caps` is a valid out-param.
    let st = unsafe { HidP_GetCaps(pp, &mut caps) };
    // SAFETY: `pp` came from `HidD_GetPreparsedData` and is not used after this.
    let _ = unsafe { HidD_FreePreparsedData(pp) };
    st.0 >= 0 && is_controller(ids.VendorID, ids.ProductID, caps.UsagePage, caps.Usage)
}

/// Whether the fence takes a HID collection: a game controller's by its usage, or the
/// vendor-defined one a Steam controller talks through. A Steam controller's other collections
/// are a mouse and a keyboard, so its usage never says game controller.
fn is_controller(vendor: u16, product: u16, page: u16, usage: u16) -> bool {
    let steam_pad = vendor == VALVE && STEAM_PADS.contains(&product);
    is_game_usage(page, usage) || (steam_pad && page >= USAGE_PAGE_VENDOR)
}

/// A top-level usage that names a game controller.
fn is_game_usage(page: u16, usage: u16) -> bool {
    page == USAGE_PAGE_GENERIC_DESKTOP && GAME_USAGES.contains(&usage)
}

/// Open an interface path for queries only: no access rights, shared, so a game's open is
/// never disturbed.
fn open_query(path: &str) -> Result<OwnedHandle> {
    let wide = HSTRING::from(path);
    // SAFETY: `wide` is a NUL-terminated path valid for the call; the handle returned is owned
    // by the caller and wrapped at once.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .with_context(|| format!("open {path}"))?;
    // SAFETY: a valid handle we own, closed by the OwnedHandle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// Present interfaces of `class`, every device.
fn interfaces(class: &GUID) -> Result<Vec<String>> {
    let mut len = 0u32;
    // SAFETY: `len` is a valid out-param; a null device id means every device.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            class,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is `len` UTF-16 units as the size call just reported.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            PCWSTR::null(),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        bail!("CM_Get_Device_Interface_ListW failed (CONFIGRET {})", cr.0);
    }
    Ok(buf
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// The devnode an interface belongs to.
fn instance_id_of(path: &str) -> Result<String> {
    let wide = HSTRING::from(path);
    let mut ty = DEVPROPTYPE(0);
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `wide` is a NUL-terminated path; the key is a static; `buf` holds `size` bytes
    // and `ty` / `size` are valid out-params.
    let cr = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(wide.as_ptr()),
            &DEVPKEY_Device_InstanceId,
            &mut ty,
            Some(buf.as_mut_ptr().cast::<u8>()),
            &mut size,
            0,
        )
    };
    if cr != CR_SUCCESS || ty != DEVPROP_TYPE_STRING {
        bail!("instance id of {path}: CONFIGRET {}, type {}", cr.0, ty.0);
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..end]))
}

fn locate(instance_id: &str) -> Result<u32> {
    let wide = HSTRING::from(instance_id);
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
    Ok(devinst)
}

fn device_id(devinst: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: `devinst` is a live devnode handle; `buf` is a valid out-buffer.
    if unsafe { CM_Get_Device_IDW(devinst, &mut buf, 0) } != CR_SUCCESS {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Write `session` into the devnode, or delete the property with `None`.
fn set_session(instance_id: &str, session: Option<u32>) -> Result<()> {
    let devinst = locate(instance_id)?;
    let value = session.map(u32::to_ne_bytes);
    let (ty, buf) = match &value {
        Some(bytes) => (DEVPROP_TYPE_UINT32, Some(&bytes[..])),
        None => (DEVPROP_TYPE_EMPTY, None),
    };
    // SAFETY: `devinst` was just located; the key is a static; `buf` is the live value or
    // `None`, which with the empty type deletes the property.
    let cr = unsafe { CM_Set_DevNode_PropertyW(devinst, &DEVPKEY_Device_SessionId, ty, buf, 0) };
    if cr != CR_SUCCESS {
        bail!(
            "CM_Set_DevNode_PropertyW({instance_id}) failed (CONFIGRET {})",
            cr.0
        );
    }
    Ok(())
}

/// Arrivals of `class` interfaces go to the fence thread.
fn watch(class: GUID) -> Result<()> {
    let filter = CM_NOTIFY_FILTER {
        cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
        Flags: 0,
        FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        Reserved: 0,
        u: CM_NOTIFY_FILTER_0 {
            DeviceInterface: CM_NOTIFY_FILTER_0_0 { ClassGuid: class },
        },
    };
    let mut handle = HCMNOTIFICATION::default();
    // SAFETY: `filter` is fully initialised and read synchronously; the callback takes no
    // context; `handle` receives the registration, kept for the life of the process.
    let cr = unsafe { CM_Register_Notification(&filter, None, Some(on_notify), &mut handle) };
    if cr != CR_SUCCESS {
        return Err(anyhow!(
            "CM_Register_Notification failed (CONFIGRET {})",
            cr.0
        ));
    }
    let _ = NOTIFICATION.set(handle.0 as usize);
    Ok(())
}

unsafe extern "system" fn on_notify(
    _notify: HCMNOTIFICATION,
    _context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    size: u32,
) -> u32 {
    if action != CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL || data.is_null() {
        return 0;
    }
    // SAFETY: `data` is the live event record of `size` bytes for this call; the symbolic link
    // is a NUL-terminated UTF-16 string that starts at `SymbolicLink` and ends inside `size`.
    let link = unsafe {
        let start = std::ptr::addr_of!((*data).u.DeviceInterface.SymbolicLink).cast::<u16>();
        let head = (start as usize).saturating_sub(data as usize);
        let max = (size as usize).saturating_sub(head) / 2;
        let units = std::slice::from_raw_parts(start, max);
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    };
    if let Some(tx) = ARRIVALS.get()
        && let Ok(tx) = tx.lock()
    {
        let _ = tx.send(link);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_game_collections_and_physical_devnodes_are_fenced() {
        assert!(is_game_usage(1, 5), "gamepad");
        assert!(is_game_usage(1, 4), "joystick");
        assert!(is_game_usage(1, 8), "multi-axis");
        assert!(!is_game_usage(1, 6), "keyboard");
        assert!(!is_game_usage(1, 2), "mouse");
        assert!(!is_game_usage(0x0C, 1), "consumer control");
        // A Steam controller reports through a vendor collection, beside a mouse and a keyboard.
        for product in STEAM_PADS {
            assert!(is_controller(VALVE, product, 0xFF00, 1), "{product:04x}");
            assert!(!is_controller(VALVE, product, 1, 2), "{product:04x} mouse");
            assert!(
                !is_controller(VALVE, product, 1, 6),
                "{product:04x} keyboard"
            );
        }
        assert!(
            is_controller(0x054C, 0x0CE6, 1, 5),
            "a DualSense, by its usage"
        );
        assert!(
            !is_controller(0x054C, 0x0CE6, 0xFF00, 1),
            "its vendor collection"
        );
        assert!(!is_controller(VALVE, 0x2000, 0xFF00, 1), "a Valve headset");
        assert!(
            !is_controller(0x046D, 0xC52B, 0xFF00, 1),
            "a keyboard receiver"
        );
        assert!(
            !is_controller(0, 0, 0xFF00, 1),
            "a device that names no ids"
        );
        assert!(is_ours("SWD\\PUNKTFUNK\\PF_MOUSE_0"));
        assert!(is_ours("swd\\VID_045E&PID_02FD\\PF_XBOX_ONES_0"));
        assert!(!is_ours("USB\\VID_054C&PID_0CE6&MI_03\\8&185D5297&4&0003"));
        assert!(is_ts_hub("ROOT\\RDPBUS\\0000"));
        assert!(is_ts_hub("USB\\TS_USB_HUB_ENUMERATOR\\0000"));
        assert!(!is_ts_hub("USB\\ROOT_HUB30\\9&2EC1B4C8&0&0"));
    }
}
