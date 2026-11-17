//! The desk fence: the service tags each physical game controller with the session that may
//! list it. A desk controller gets the console session, so a seat's HID, XUSB and XInput
//! listings and GameInput deliveries leave it alone. A controller lent to a seat ([`hand`])
//! gets that seat's session, or [`PARKED`] while the seat doesn't run, on every HID and XUSB
//! devnode under its USB device. Its keyboard and mouse collections are removed instead: a
//! controller's desktop mode types and points at the console whatever a tag says.
//!
//! The service is the one writer: a process in a session can't locate a devnode tagged for
//! another, and session 0 locates them all. A tag written after a device appears is what a
//! process can do: the session id belongs to the bus that creates the device, and a device's
//! start wipes it, whenever it was written. So the fence lands within tens of milliseconds of
//! a plug and is re-applied on every arrival. It hides a device from another session's
//! listings. It doesn't stop an open of a known path, the console's Raw Input or WGI's Xbox
//! listing, and it leaves `xinput1_4`'s box-wide index alone. A change of holder restarts the
//! USB device, which ends every open handle; the tags change in place only for a parked pad
//! going live or when the restart is refused ([`hand`]). Devices a Remote Desktop session
//! redirected in and the host's own `SWD` devices carry their session from creation and are
//! never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_DevNode_Status, CM_Get_Device_IDW, CM_Get_Device_ID_ListW,
    CM_Get_Device_ID_List_SizeW, CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_Locate_DevNodeW,
    CM_Query_And_Remove_SubTreeW, CM_Register_Notification, CM_Set_DevNode_PropertyW,
    CM_Setup_DevNode, CM_DEVNODE_STATUS_FLAGS, CM_GETIDLIST_FILTER_ENUMERATOR,
    CM_GETIDLIST_FILTER_PRESENT, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL,
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_EVENT_DATA,
    CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_0,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_PROB, CM_PROB_WILL_BE_REMOVED, CM_REMOVE_NO_RESTART,
    CM_SETUP_DEVNODE_READY, CR_SUCCESS, HCMNOTIFICATION, PNP_VETO_TYPE,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidP_GetCaps,
    GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_BusReportedDeviceDesc, DEVPKEY_Device_CompatibleIds, DEVPKEY_Device_DeviceDesc,
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_InstanceId, DEVPKEY_Device_SessionId, DEVPROPTYPE,
    DEVPROP_TYPE_EMPTY, DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32,
};
use windows::Win32::Foundation::{DEVPROPKEY, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::channel_proof::{child_device_ids, get_devnode_session};

/// `GUID_DEVINTERFACE_XUSB`: the class `xinput1_4`, WGI and GameInput walk for Xbox pads.
const GUID_DEVINTERFACE_XUSB: GUID = GUID::from_u128(0xec87f1e3_c13b_4100_b5f7_8b84d54260cb);
/// HID usage page 1 (generic desktop) and the usages a game controller's top collection has:
/// joystick, gamepad, multi-axis controller.
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const GAME_USAGES: [u16; 3] = [0x04, 0x05, 0x08];
/// The usages a controller's desktop mode types and points through: pointer, mouse, keyboard
/// and keypad.
const DESKTOP_USAGES: [u16; 4] = [0x01, 0x02, 0x06, 0x07];
/// The first vendor-defined HID usage page.
const USAGE_PAGE_VENDOR: u16 = 0xFF00;
/// Valve's vendor id and its controllers: the 2015 Steam Controller on a cable, on Bluetooth
/// and its dongle, the Deck's own pad, the Steam Controller 2 on a cable and on Bluetooth, and
/// its Puck.
const VALVE: u16 = 0x28DE;
const STEAM_PADS: [u16; 8] = [
    0x1102, 0x1106, 0x1142, 0x1205, 0x1302, 0x1303, 0x1304, 0x1305,
];

/// The session of a lent controller whose seat doesn't run: one no session ever has.
pub const PARKED: u32 = 0xFFFF_FFFE;
/// [`CONSOLE`] before the service names a console session.
const NO_SESSION: u32 = u32::MAX;

static WORK: OnceLock<Mutex<Sender<Work>>> = OnceLock::new();
static NOTIFICATION: OnceLock<usize> = OnceLock::new();
/// The console session, which desk controllers are fenced to.
static CONSOLE: AtomicU32 = AtomicU32::new(NO_SESSION);
/// Lent USB devices by upper-case instance id: the session each is fenced to.
static LENDS: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

/// What the fence thread is asked to do.
enum Work {
    /// An interface path that just arrived.
    Arrived(String),
    Sweep,
}

/// Fence every physical controller present, desk controllers to `console`, and keep fencing
/// the ones that arrive, for the life of the process. The service calls it with each console
/// session it starts a host in: a later call moves the desk's controllers to the new session.
pub fn start(console: u32) {
    CONSOLE.store(console, Ordering::SeqCst);
    run();
    if let Some(work) = WORK.get()
        && let Ok(work) = work.lock()
    {
        let _ = work.send(Work::Sweep);
    }
}

/// Starts the fence thread and its arrival watchers, once: with the first console session or
/// the first lend, whichever comes first. The watchers come before any sweep or restart, so
/// no arrival falls between the two.
fn run() {
    let (tx, rx) = channel::<Work>();
    if WORK.set(Mutex::new(tx)).is_err() {
        return;
    }
    for class in [GUID_DEVINTERFACE_HID, GUID_DEVINTERFACE_XUSB] {
        if let Err(error) = watch(class) {
            tracing::warn!(%error, "desk fence is not watching arrivals");
        }
    }
    let spawned = std::thread::Builder::new()
        .name("pf-desk-fence".into())
        .spawn(move || {
            for work in rx {
                match work {
                    Work::Sweep => sweep(),
                    Work::Arrived(path) => {
                        if let Err(error) = fence_interface(&path) {
                            tracing::warn!(%error, path = %path, "controller not fenced");
                        }
                    }
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "desk fence did not start");
    }
}

/// Give the USB device `instance` to `session`, a seat's or [`PARKED`]; `None` returns it to
/// the desk. With `restart` the device restarts, so every handle its last holder had open
/// dies and its devnodes arrive again under the new tag. Without, the tags change in place:
/// right only when nobody could have opened it. A device that isn't plugged in is the
/// holder's the moment it arrives. A refused restart changes nothing: the device runs on
/// under the tags it had. A device returned to the desk in place gets its keyboard and mouse
/// collections back.
pub fn hand(instance: &str, session: Option<u32>, restart: bool) -> Result<()> {
    run();
    let key = instance.to_ascii_uppercase();
    let set = |session: Option<u32>| {
        let mut lends = lends();
        match session {
            Some(session) => lends.insert(key.clone(), session),
            None => lends.remove(&key),
        }
    };
    let was = set(session);
    if !restart {
        if session.is_none() {
            revive(instance);
        }
        sweep();
        return Ok(());
    }
    let restarted = restart_device(instance);
    if restarted.is_err() {
        set(was);
    }
    restarted
}

fn lends() -> MutexGuard<'static, BTreeMap<String, u32>> {
    LENDS.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Take the fence off every physical controller that carries one. For the service's stop and
/// the uninstaller: a tag never outlives the device's next start, but a pad that stays up
/// through a punktfunk removal would otherwise keep hiding from a later console session. The
/// fence's own skips hold here too: a redirected pad's tag and our devices' are not the fence's.
/// Nothing is lent after it: a lent controller's keyboard and mouse collections come back, and
/// arrivals go untagged.
pub fn clear() {
    CONSOLE.store(NO_SESSION, Ordering::SeqCst);
    let lent: Vec<String> = std::mem::take(&mut *lends()).into_keys().collect();
    for (path, hid) in every_interface() {
        let Ok(id) = instance_id_of(&path) else {
            continue;
        };
        let Ok(line) = lineage(&id) else {
            continue;
        };
        if is_ours(&id) || line.iter().any(|id| is_ts_hub(id)) {
            continue;
        }
        let was_lent = line.iter().any(|id| lent.contains(id));
        if !was_lent && hid && !is_controller_collection(&path) {
            continue;
        }
        if let Ok(Some(_)) = get_devnode_session(&id)
            && let Err(error) = set_session(&id, None)
        {
            tracing::warn!(%error, devnode = %id, "desk fence not cleared");
        }
    }
    for instance in &lent {
        revive(instance);
    }
}

/// Every present HID and XUSB interface now: tag what the fence takes.
fn sweep() {
    let mut fenced = 0usize;
    for (path, hid) in every_interface() {
        match fence_path(&path, hid) {
            Ok(true) => fenced += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, path = %path, "controller not fenced"),
        }
    }
    let console = CONSOLE.load(Ordering::SeqCst);
    tracing::info!(console, fenced, "controllers fenced to their sessions");
}

/// `(interface path, is HID)` of every present HID and XUSB interface.
fn every_interface() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (class, hid) in [
        (GUID_DEVINTERFACE_HID, true),
        (GUID_DEVINTERFACE_XUSB, false),
    ] {
        for path in interfaces(&class).unwrap_or_default() {
            out.push((path, hid));
        }
    }
    out
}

/// One arrived interface: fence its devnode when the fence takes it.
fn fence_interface(path: &str) -> Result<()> {
    let is_hid = path
        .to_ascii_lowercase()
        .contains("4d1e55b2-f16f-11cf-88cb-001111000030");
    fence_path(path, is_hid).map(|_| ())
}

/// Tag the devnode behind `path` with its holder's session: a lent device's every HID and XUSB
/// devnode with its lend's, a desk controller with the console's. A lent device's keyboard
/// and mouse collections are removed, and tagged only when that is refused. Ours, redirected
/// ones and other HID devices are left alone. `Ok(true)` when the devnode changed.
fn fence_path(path: &str, hid: bool) -> Result<bool> {
    let id = instance_id_of(path)?;
    if is_ours(&id) {
        return Ok(false);
    }
    let line = lineage(&id)?;
    if line.iter().any(|id| is_ts_hub(id)) {
        return Ok(false);
    }
    let session = match lent_session(&line) {
        Some(session) if hid && is_desktop_collection(path) => match remove(&id, locate(&id)?) {
            Ok(()) => {
                tracing::info!(devnode = %id, "lent controller's desktop input removed");
                return Ok(true);
            }
            Err(error) => {
                tracing::warn!(%error, "lent controller's desktop input not removed");
                session
            }
        },
        Some(session) => session,
        None if hid && !is_controller_collection(path) => return Ok(false),
        None => match CONSOLE.load(Ordering::SeqCst) {
            NO_SESSION => return Ok(false),
            console => console,
        },
    };
    if get_devnode_session(&id)? == Some(session) {
        return Ok(false);
    }
    set_session(&id, Some(session))?;
    match get_devnode_session(&id)? {
        Some(read) if read == session => {
            tracing::info!(devnode = %id, path = %path, session, "controller fenced");
            Ok(true)
        }
        read => bail!("{id} reads {read:?} after the write"),
    }
}

/// The session the lend of a device in `line` names, a devnode's own id then its ancestors'.
fn lent_session(line: &[String]) -> Option<u32> {
    let lends = lends();
    line.iter().find_map(|id| lends.get(id).copied())
}

/// The upper-case instance ids of `instance_id` and its ancestors, nearest first.
fn lineage(instance_id: &str) -> Result<Vec<String>> {
    let mut devinst = locate(instance_id)?;
    let mut line = vec![instance_id.to_ascii_uppercase()];
    for _ in 0..16 {
        let mut parent = 0u32;
        // SAFETY: `devinst` is a live devnode handle; `parent` is a valid out-param.
        if unsafe { CM_Get_Parent(&mut parent, devinst, 0) } != CR_SUCCESS {
            break;
        }
        line.push(device_id(parent).to_ascii_uppercase());
        devinst = parent;
    }
    Ok(line)
}

/// A USB device plugged into the box, as the lending gate reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    /// `USB\VID_…&PID_…\<serial or port id>`.
    pub instance: String,
    pub vendor_id: u16,
    pub product_id: u16,
    /// What the device calls itself on the bus.
    pub name: String,
    /// Class, subclass and protocol of each interface.
    pub interfaces: Vec<[u8; 3]>,
    /// A HID controller collection or an XUSB interface is this device's own: none of a
    /// device plugged into it, as a hub or a Bluetooth radio has.
    pub controller: bool,
}

/// Every USB device plugged into a real port, redirected ones aside.
pub fn usb_devices() -> Vec<UsbDevice> {
    let mut controllers = BTreeSet::new();
    for (path, hid) in every_interface() {
        if hid && !is_controller_collection(&path) {
            continue;
        }
        let line = instance_id_of(&path).and_then(|id| lineage(&id));
        let device = |line: Vec<String>| line.into_iter().find(|id| usb_ids(id).is_some());
        controllers.extend(line.ok().and_then(device));
    }
    let read = |instance: String| {
        let (vendor_id, product_id) = usb_ids(&instance)?;
        let line = lineage(&instance).ok()?;
        if line.iter().any(|id| is_ts_hub(id)) {
            return None;
        }
        let devinst = locate(&instance).ok()?;
        let compatible = strings(devinst, &DEVPKEY_Device_CompatibleIds);
        let interfaces = if compatible.iter().any(|id| is_composite(id)) {
            let children = child_device_ids(&instance).unwrap_or_default();
            let class = |child: &String| {
                let compatible = strings(locate(child).ok()?, &DEVPKEY_Device_CompatibleIds);
                compatible.iter().find_map(|id| interface_class(id))
            };
            children.iter().filter_map(class).collect()
        } else {
            Vec::from_iter(compatible.iter().find_map(|id| interface_class(id)))
        };
        let name = [
            &DEVPKEY_Device_BusReportedDeviceDesc,
            &DEVPKEY_Device_FriendlyName,
            &DEVPKEY_Device_DeviceDesc,
        ]
        .into_iter()
        .find_map(|key| strings(devinst, key).into_iter().next())
        .unwrap_or_default();
        Some(UsbDevice {
            controller: controllers.contains(&instance.to_ascii_uppercase()),
            instance,
            vendor_id,
            product_id,
            name,
            interfaces,
        })
    };
    present_devices("USB")
        .into_iter()
        .filter_map(read)
        .collect()
}

/// The vendor and product of a USB device's own instance id, `USB\VID_vvvv&PID_pppp\…`. An
/// interface's (`…&MI_00\…`) and a hub's own (`USB\ROOT_HUB30\…`) are no device's.
fn usb_ids(instance_id: &str) -> Option<(u16, u16)> {
    let upper = instance_id.to_ascii_uppercase();
    let mut parts = upper.split('\\');
    let (bus, ids, tail) = (parts.next()?, parts.next()?, parts.next()?);
    let (vendor, product) = ids.strip_prefix("VID_")?.split_once("&PID_")?;
    let hex = |text: &str| {
        u16::from_str_radix(text, 16)
            .ok()
            .filter(|_| text.len() == 4)
    };
    let device = bus == "USB" && !tail.is_empty() && parts.next().is_none();
    device.then_some((hex(vendor)?, hex(product)?))
}

/// The class, subclass and protocol a compatible id of the form
/// `USB\Class_cc&SubClass_ss&Prot_pp` names.
fn interface_class(compatible_id: &str) -> Option<[u8; 3]> {
    let upper = compatible_id.to_ascii_uppercase();
    let rest = upper.strip_prefix("USB\\CLASS_")?;
    let (class, rest) = rest.split_once("&SUBCLASS_")?;
    let (subclass, protocol) = rest.split_once("&PROT_")?;
    let hex = |text: &str| u8::from_str_radix(text, 16).ok();
    Some([hex(class)?, hex(subclass)?, hex(protocol)?])
}

/// The compatible id of a device whose interfaces are devnodes of their own.
fn is_composite(compatible_id: &str) -> bool {
    compatible_id.eq_ignore_ascii_case("USB\\COMPOSITE")
}

/// The instance id of every present device `enumerator` made.
fn present_devices(enumerator: &str) -> Vec<String> {
    let filter = HSTRING::from(enumerator);
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let mut len = 0u32;
    // SAFETY: `len` is a valid out-param; `filter` is a NUL-terminated name valid for the call.
    let cr = unsafe { CM_Get_Device_ID_List_SizeW(&mut len, PCWSTR(filter.as_ptr()), flags) };
    if cr != CR_SUCCESS || len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is `len` UTF-16 units as the size call just reported.
    let cr = unsafe { CM_Get_Device_ID_ListW(PCWSTR(filter.as_ptr()), &mut buf, flags) };
    if cr != CR_SUCCESS {
        return Vec::new();
    }
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// A devnode's string property, one string or a list; empty when it has none.
fn strings(devinst: u32, key: &DEVPROPKEY) -> Vec<String> {
    let mut ty = DEVPROPTYPE(0);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `devinst` is a located devnode; the key is a static; `buf` holds `size` bytes and
    // `ty` / `size` are valid out-params.
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            key,
            &mut ty,
            Some(buf.as_mut_ptr().cast::<u8>()),
            &mut size,
            0,
        )
    };
    if cr != CR_SUCCESS {
        return Vec::new();
    }
    let units = (size as usize / 2).min(buf.len());
    buf[..units]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// Remove the device's subtree and set it up again: its drivers unload, every open handle on
/// it dies, and its devnodes arrive anew. A device that isn't plugged in has nothing to end.
fn restart_device(instance_id: &str) -> Result<()> {
    let Ok(devinst) = locate(instance_id) else {
        return Ok(());
    };
    let removed = remove(instance_id, devinst);
    // A refused removal left it running; one that went through needs this to come back.
    // SAFETY: `devinst` stays a valid devnode handle across its removal.
    let ready = unsafe { CM_Setup_DevNode(devinst, CM_SETUP_DEVNODE_READY) };
    removed?;
    if ready != CR_SUCCESS {
        bail!(
            "restart {instance_id}: CONFIGRET {} setting it up again",
            ready.0
        );
    }
    Ok(())
}

/// Query-remove the devnode's subtree: its drivers unload and every open handle on it dies.
/// It stays out until it is set up again, its parent restarts, a rescan or a reboot.
fn remove(instance_id: &str, devinst: u32) -> Result<()> {
    let mut veto = PNP_VETO_TYPE(0);
    let mut by = [0u16; 260];
    // SAFETY: `devinst` is a located devnode; `veto` and `by` are valid out-params for the call.
    let removed = unsafe {
        CM_Query_And_Remove_SubTreeW(
            devinst,
            Some(&mut veto),
            Some(&mut by),
            CM_REMOVE_NO_RESTART,
        )
    };
    if removed != CR_SUCCESS {
        let end = by.iter().position(|&c| c == 0).unwrap_or(by.len());
        let by = String::from_utf16_lossy(&by[..end]);
        bail!(
            "remove {instance_id}: refused (CONFIGRET {}, veto {} by {by:?})",
            removed.0,
            veto.0
        );
    }
    Ok(())
}

/// Set up again each devnode under the USB device `instance` that a removal took out, two
/// levels down: an interface's collections lie under the interface.
fn revive(instance: &str) {
    let children = child_device_ids(instance).unwrap_or_default();
    let below = |id: &String| child_device_ids(id).unwrap_or_default();
    let grandchildren: Vec<String> = children.iter().flat_map(below).collect();
    for id in children.iter().chain(&grandchildren) {
        let Ok(devinst) = locate(id) else {
            continue;
        };
        let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        // SAFETY: `devinst` was just located; both out-params are valid for the call.
        let read = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
        if read != CR_SUCCESS || problem != CM_PROB_WILL_BE_REMOVED {
            continue;
        }
        // SAFETY: `devinst` is the located devnode a removal left out.
        let ready = unsafe { CM_Setup_DevNode(devinst, CM_SETUP_DEVNODE_READY) };
        if ready != CR_SUCCESS {
            tracing::warn!(devnode = %id, configret = ready.0, "removed collection not set up");
        }
    }
}

/// The host's own virtual devices: `SWD\PUNKTFUNK\…` and `SWD\VID_…\PF_…`, tagged at creation.
fn is_ours(instance_id: &str) -> bool {
    instance_id.to_ascii_uppercase().starts_with("SWD\\")
}

/// The Remote Desktop USB redirection bus and hub, by the ids Windows gives them.
fn is_ts_hub(instance_id: &str) -> bool {
    let upper = instance_id.to_ascii_uppercase();
    upper.starts_with("ROOT\\RDPBUS") || upper.contains("TS_USB_HUB") || upper.contains("TSUSBHUB")
}

/// Whether the HID collection behind `path` is a controller's ([`is_controller`]). A device
/// that doesn't name its ids is judged by its usage alone.
fn is_controller_collection(path: &str) -> bool {
    collection(path)
        .is_some_and(|[vendor, product, page, usage]| is_controller(vendor, product, page, usage))
}

/// Whether the HID collection behind `path` is desktop input ([`is_desktop_input`]).
fn is_desktop_collection(path: &str) -> bool {
    collection(path).is_some_and(|[_, _, page, usage]| is_desktop_input(page, usage))
}

/// The vendor id, product id, usage page and usage of the HID collection behind `path`. Ids
/// the device doesn't name read zero.
fn collection(path: &str) -> Option<[u16; 4]> {
    let handle = open_query(path).ok()?;
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
        return None;
    }
    let mut caps = HIDP_CAPS::default();
    // SAFETY: `pp` is the handle just obtained (freed below); `caps` is a valid out-param.
    let st = unsafe { HidP_GetCaps(pp, &mut caps) };
    // SAFETY: `pp` came from `HidD_GetPreparsedData` and is not used after this.
    let _ = unsafe { HidD_FreePreparsedData(pp) };
    (st.0 >= 0).then_some([ids.VendorID, ids.ProductID, caps.UsagePage, caps.Usage])
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

/// A top-level usage that types or points: what the console takes from any session's device.
fn is_desktop_input(page: u16, usage: u16) -> bool {
    page == USAGE_PAGE_GENERIC_DESKTOP && DESKTOP_USAGES.contains(&usage)
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
    if let Some(work) = WORK.get()
        && let Ok(work) = work.lock()
    {
        let _ = work.send(Work::Arrived(link));
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
        for (usage, what) in [(1, "pointer"), (2, "mouse"), (6, "keyboard"), (7, "keypad")] {
            assert!(is_desktop_input(1, usage), "{what}");
        }
        assert!(!is_desktop_input(1, 5), "gamepad");
        assert!(!is_desktop_input(0xFF00, 1), "a vendor collection");
        assert!(!is_desktop_input(0x0C, 1), "consumer control");
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

    #[test]
    fn a_usb_device_is_read_from_its_ids() {
        assert_eq!(
            usb_ids("USB\\VID_28DE&PID_1304\\FXB995480095E"),
            Some((0x28DE, 0x1304))
        );
        assert_eq!(
            usb_ids("usb\\vid_054c&pid_0ce6\\7&3f94d01&0&2"),
            Some((0x054C, 0x0CE6))
        );
        let interface = "USB\\VID_054C&PID_0CE6&MI_03\\8&185D5297&4&0003";
        assert_eq!(usb_ids(interface), None, "an interface");
        assert_eq!(usb_ids("USB\\ROOT_HUB30\\9&2EC1B4C8&0&0"), None, "a hub");
        let child = "HID\\VID_054C&PID_0CE6\\9&25E0AEA4&0&0000";
        assert_eq!(usb_ids(child), None, "a HID child");
        assert_eq!(usb_ids("USB\\VID_054C&PID_0CE6"), None, "no instance");

        let hid = interface_class("USB\\Class_03&SubClass_00&Prot_00");
        assert_eq!(hid, Some([3, 0, 0]));
        let xbox = interface_class("usb\\class_ff&subclass_47&prot_d0");
        assert_eq!(xbox, Some([0xFF, 0x47, 0xD0]));
        assert_eq!(interface_class("USB\\Class_03&SubClass_00"), None);
        assert_eq!(interface_class("USB\\COMPOSITE"), None);
        assert!(is_composite("USB\\COMPOSITE"));
        assert!(!is_composite("USB\\Class_03"));
    }

    /// The HID and XUSB devnodes under the USB device `instance`, each with its session.
    fn tags_under(instance: &str) -> Vec<(String, Option<u32>)> {
        let top = instance.to_ascii_uppercase();
        let under = |id: &String| lineage(id).is_ok_and(|line| line.contains(&top));
        every_interface()
            .into_iter()
            .filter_map(|(path, _)| instance_id_of(&path).ok())
            .filter(under)
            .map(|id| {
                let session = get_devnode_session(&id).ok().flatten();
                (id, session)
            })
            .collect()
    }

    /// Whether every devnode under `instance` is back and carries `want` within eight seconds.
    fn settles(instance: &str, want: Option<u32>) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let tags = tags_under(instance);
            if !tags.is_empty() && tags.iter().all(|(_, session)| *session == want) {
                return true;
            }
            if std::time::Instant::now() > deadline {
                println!("still {tags:?}");
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// On a Windows box, elevated in session 0: `PF_FENCE_LIVE_DEVICE` names the instance id
    /// of a USB HID device nobody needs for the next seconds. It restarts twice.
    #[test]
    #[ignore = "restarts a real USB device"]
    fn live_a_hand_over_tags_every_devnode_and_restarts_the_device() {
        let instance = std::env::var("PF_FENCE_LIVE_DEVICE").expect("PF_FENCE_LIVE_DEVICE");
        for usb in usb_devices() {
            println!("{usb:?}");
        }
        let listed = usb_devices();
        let named = |usb: &&UsbDevice| usb.instance.eq_ignore_ascii_case(&instance);
        let device = listed.iter().find(named).expect("the device is plugged in");
        assert!(!device.interfaces.is_empty(), "its interfaces are read");
        assert!(
            !tags_under(&instance).is_empty(),
            "it has a HID or XUSB devnode"
        );

        let started = std::time::Instant::now();
        hand(&instance, Some(PARKED), true).expect("parked, with a restart");
        println!("restart took {:?}", started.elapsed());
        assert!(settles(&instance, Some(PARKED)), "parked on arrival");
        println!("parked after {:?}", started.elapsed());
        hand(&instance, Some(4242), false).expect("live, in place");
        assert!(settles(&instance, Some(4242)), "live in place");
        hand(&instance, None, true).expect("the desk's, with a restart");
        assert!(settles(&instance, None), "untagged once it is the desk's");
    }
}
