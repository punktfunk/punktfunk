//! The session's enumerator: the stock endpoints plus one DualSense render endpoint per pad
//! slot the session owns, announced to registered clients as pads come and go.

use crate::endpoint::Endpoint;
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use pf_driver_proto::seat_audio::{self as proto, SLOTS};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::core::{implement, IUnknown, Interface, Ref, Result, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, HANDLE, PROPERTYKEY, STG_E_ACCESSDENIED,
    VARIANT_FALSE, VARIANT_TRUE, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eAll, eConsole, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceCollection,
    IMMDeviceCollection_Impl, IMMDeviceEnumerator, IMMDeviceEnumerator_Impl, IMMDevice_Impl,
    IMMEndpoint, IMMEndpoint_Impl, IMMNotificationClient, DEVICE_STATE, DEVICE_STATE_ACTIVE,
    DEVICE_STATE_NOTPRESENT,
};
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromCLSID, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, BLOB, CLSCTX, STGM, STGM_READ};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::{VARENUM, VT_BLOB, VT_BOOL, VT_LPWSTR, VT_UI4};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, IPropertyStore_Impl};

/// How often a watcher looks for pads arriving or leaving.
const POLL_MS: u32 = 250;
/// The endpoint class id the spike measured. Unregistered: the engine activates through
/// `IMMDevice::Activate`, never by class.
const ACTIVATOR: GUID = GUID::from_u128(0xbd646d45_d823_459a_98f2_4e64e627a21c);

const fn pkey(fmtid: u128, pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: GUID::from_u128(fmtid),
        pid,
    }
}
const FRIENDLY_NAME: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 14);
const DEVICE_DESC: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 2);
const INTERFACE_NAME: PROPERTYKEY = pkey(0x026e516e_b814_414b_83cd_856d6fef4822, 2);
const DEVICE_NAME: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 6);
const REMOTE: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 14);
const CONTAINER: PROPERTYKEY = pkey(0x8c7ed206_3f8a_4827_b3ab_ae9e1faefc6c, 2);
const FORM_FACTOR: PROPERTYKEY = pkey(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e, 0);
const ENDPOINT_GUID: PROPERTYKEY = pkey(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e, 4);
const EVENT_DRIVEN: PROPERTYKEY = pkey(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e, 7);
const DEVICE_FORMAT: PROPERTYKEY = pkey(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c, 0);
const HOST_FORMAT: PROPERTYKEY = pkey(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04, 0);
const OEM_FORMAT: PROPERTYKEY = pkey(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04, 3);
const ACTIVATOR_KEY: PROPERTYKEY = pkey(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04, 5);

/// The stock enumerator wrapped for `session`.
pub(crate) fn wrap(session: u32, base: IMMDeviceEnumerator) -> Result<IMMDeviceEnumerator> {
    // Remote Audio's record carries the icon and engine values a remote endpoint needs.
    let template = remote_audio_store(&base)
        .map(|store| copy_store(&store))
        .unwrap_or_default();
    let pads = (0..SLOTS)
        .map(|slot| Device::new(session, slot, &template).map(IMMDevice::from))
        .collect::<Result<Vec<_>>>()?;
    // SAFETY: an unnamed manual-reset event, owned below.
    let stop = unsafe { CreateEventW(None, true, false, None) }?;
    // SAFETY: `stop` is a fresh handle this process owns.
    let stop = unsafe { OwnedHandle::from_raw_handle(stop.0) };
    let watch = Arc::new(Watch {
        session,
        clients: Mutex::new(Clients::default()),
        seen: AtomicU32::new(present_mask(session)),
        stop,
    });
    Ok(Enumerator {
        base,
        pads,
        watch,
        watcher: Mutex::new(None),
    }
    .into())
}

/// Whether `session`'s host serves `slot` now: its presence section holds the magic. Only a
/// privileged process can make that section and only SYSTEM may write it, so a user of another
/// session can neither fake a pad here nor keep one listed.
fn present(session: u32, slot: u8) -> bool {
    let name = HSTRING::from(proto::presence_name(session, slot));
    // SAFETY: opens a named section for reading; the handle is closed below.
    let Ok(mapping) = (unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, &name) }) else {
        return false;
    };
    // SAFETY: maps the section's first PRESENCE_BYTES read-only; unmapped below.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, proto::PRESENCE_BYTES) };
    let magic = (!view.Value.is_null()).then(|| {
        // SAFETY: the view is page-aligned and mapped; the host stores the u32 atomically.
        let magic = unsafe { AtomicU32::from_ptr(view.Value.cast()) }.load(Ordering::Acquire);
        // SAFETY: the view was mapped above and is released once.
        let _ = unsafe { UnmapViewOfFile(view) };
        magic
    });
    // SAFETY: the handle was opened above and is not used again.
    let _ = unsafe { CloseHandle(mapping) };
    magic == Some(proto::PRESENT_MAGIC)
}

fn present_mask(session: u32) -> u32 {
    (0..SLOTS)
        .filter(|&slot| present(session, slot))
        .fold(0, |mask, slot| mask | 1 << slot)
}

/// Registered notification clients.
#[derive(Clone, Default)]
struct Clients(Vec<IMMNotificationClient>);

// SAFETY: MMDevAPI calls notification clients from its own worker threads, so a registered
// client is free-threaded by contract; the watcher thread is one more such caller.
unsafe impl Send for Clients {}
// SAFETY: as above.
unsafe impl Sync for Clients {}

/// What the watcher thread and the enumerator share.
struct Watch {
    session: u32,
    clients: Mutex<Clients>,
    seen: AtomicU32,
    stop: OwnedHandle,
}

impl Watch {
    fn run(&self) {
        let stop = HANDLE(self.stop.as_raw_handle());
        // SAFETY: `stop` lives as long as `self`, which this thread holds.
        while unsafe { WaitForSingleObject(stop, POLL_MS) } == WAIT_TIMEOUT {
            self.poll();
        }
    }

    /// Tell every registered client about each slot that arrived or left since the last look.
    fn poll(&self) {
        let now = present_mask(self.session);
        let changed = now ^ self.seen.swap(now, Ordering::AcqRel);
        if changed == 0 {
            return;
        }
        let clients = self
            .clients
            .lock()
            .map(|clients| clients.clone())
            .unwrap_or_default();
        for slot in (0..SLOTS).filter(|slot| changed >> slot & 1 == 1) {
            let id = HSTRING::from(proto::endpoint_guid(slot));
            let id = PCWSTR(id.as_ptr());
            for client in &clients.0 {
                // SAFETY: notification calls with a live, nul-terminated id string.
                unsafe {
                    if now >> slot & 1 == 1 {
                        let _ = client.OnDeviceAdded(id);
                        let _ = client.OnDeviceStateChanged(id, DEVICE_STATE_ACTIVE);
                    } else {
                        let _ = client.OnDeviceStateChanged(id, DEVICE_STATE_NOTPRESENT);
                        let _ = client.OnDeviceRemoved(id);
                    }
                }
            }
        }
    }
}

#[implement(IMMDeviceEnumerator)]
struct Enumerator {
    base: IMMDeviceEnumerator,
    pads: Vec<IMMDevice>,
    watch: Arc<Watch>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Enumerator {
    // The watcher holds its own `Arc<Watch>` and leaves within `POLL_MS` of the event. Never
    // join it: at process exit Windows has already ended it, and `join` then panics inside COM
    // `Release`, which cannot unwind — a fast-fail in the audio service or the game.
    fn drop(&mut self) {
        // SAFETY: the stop event is owned by `watch`, alive here.
        let _ = unsafe { SetEvent(HANDLE(self.watch.stop.as_raw_handle())) };
    }
}

impl IMMDeviceEnumerator_Impl for Enumerator_Impl {
    fn EnumAudioEndpoints(
        &self,
        flow: EDataFlow,
        mask: DEVICE_STATE,
    ) -> Result<IMMDeviceCollection> {
        // SAFETY: forwarded unchanged to the stock enumerator.
        let base = unsafe { self.base.EnumAudioEndpoints(flow, mask) }?;
        let wanted = (flow == eRender || flow == eAll) && mask.0 & DEVICE_STATE_ACTIVE.0 != 0;
        let session = self.watch.session;
        let pads = self
            .pads
            .iter()
            .zip(0..SLOTS)
            .filter(|(_, slot)| wanted && present(session, *slot))
            .map(|(pad, _)| pad.clone())
            .collect();
        // SAFETY: plain call on the stock collection.
        let base_count = unsafe { base.GetCount() }?;
        Ok(Collection {
            base,
            base_count,
            pads,
        }
        .into())
    }

    fn GetDefaultAudioEndpoint(&self, flow: EDataFlow, role: ERole) -> Result<IMMDevice> {
        // SAFETY: forwarded unchanged; the session's default stays Remote Audio.
        unsafe { self.base.GetDefaultAudioEndpoint(flow, role) }
    }

    fn GetDevice(&self, id: &PCWSTR) -> Result<IMMDevice> {
        if id.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: a non-null, nul-terminated id from the caller.
        let text = unsafe { id.to_string() }.map_err(|_| E_INVALIDARG)?;
        match proto::slot_of(&text).and_then(|slot| self.pads.get(usize::from(slot))) {
            Some(pad) => Ok(pad.clone()),
            // SAFETY: forwarded unchanged to the stock enumerator.
            None => unsafe { self.base.GetDevice(*id) },
        }
    }

    fn RegisterEndpointNotificationCallback(
        &self,
        client: Ref<IMMNotificationClient>,
    ) -> Result<()> {
        let client = client.ok()?;
        // SAFETY: forwarded unchanged to the stock enumerator.
        unsafe { self.base.RegisterEndpointNotificationCallback(client) }?;
        if let Ok(mut clients) = self.watch.clients.lock() {
            clients.0.push(client.clone());
        }
        if let Ok(mut watcher) = self.watcher.lock() {
            if watcher.is_none() {
                let watch = self.watch.clone();
                *watcher = std::thread::Builder::new()
                    .name("pf-seat-audio-watch".into())
                    .spawn(move || watch.run())
                    .ok();
            }
        }
        Ok(())
    }

    fn UnregisterEndpointNotificationCallback(
        &self,
        client: Ref<IMMNotificationClient>,
    ) -> Result<()> {
        let client = client.ok()?;
        if let Ok(mut clients) = self.watch.clients.lock() {
            clients.0.retain(|known| known != client);
        }
        // SAFETY: forwarded unchanged to the stock enumerator.
        unsafe { self.base.UnregisterEndpointNotificationCallback(client) }
    }
}

#[implement(IMMDeviceCollection)]
struct Collection {
    base: IMMDeviceCollection,
    base_count: u32,
    pads: Vec<IMMDevice>,
}

impl IMMDeviceCollection_Impl for Collection_Impl {
    fn GetCount(&self) -> Result<u32> {
        let pads = u32::try_from(self.pads.len()).map_err(|_| E_INVALIDARG)?;
        Ok(self.base_count.saturating_add(pads))
    }

    fn Item(&self, index: u32) -> Result<IMMDevice> {
        match index.checked_sub(self.base_count) {
            // SAFETY: an index the stock collection counted.
            None => unsafe { self.base.Item(index) },
            Some(pad) => usize::try_from(pad)
                .ok()
                .and_then(|pad| self.pads.get(pad))
                .cloned()
                .ok_or_else(|| E_INVALIDARG.into()),
        }
    }
}

/// One pad slot's render endpoint in one session.
#[implement(IMMDevice, IMMEndpoint)]
struct Device {
    session: u32,
    slot: u8,
    id: HSTRING,
    store: IPropertyStore,
}

impl Device {
    fn new(session: u32, slot: u8, template: &[(PROPERTYKEY, PROPVARIANT)]) -> Result<Self> {
        let id = HSTRING::from(proto::endpoint_guid(slot));
        let ours = [
            (
                FRIENDLY_NAME,
                text("Wireless Controller (DualSense Wireless Controller)")?,
            ),
            (DEVICE_DESC, text("Wireless Controller")?),
            (INTERFACE_NAME, text("DualSense Wireless Controller")?),
            (DEVICE_NAME, text("DualSense Wireless Controller")?),
            (CONTAINER, guid(&container(slot))?),
            (ACTIVATOR_KEY, guid(&ACTIVATOR)?),
            (ENDPOINT_GUID, text(&id.to_string())?),
            (FORM_FACTOR, raw(VT_UI4, PROPVARIANT_0_0_0 { ulVal: 0 })),
            (REMOTE, boolean(true)),
            (EVENT_DRIVEN, boolean(false)),
            (DEVICE_FORMAT, blob(&proto::FORMAT_PCM16)?),
            (OEM_FORMAT, blob(&proto::FORMAT_PCM16)?),
            (HOST_FORMAT, blob(&proto::FORMAT_F32)?),
        ];
        let mut values: Vec<(PROPERTYKEY, PROPVARIANT)> = template
            .iter()
            .filter(|(key, _)| !ours.iter().any(|(own, _)| own == key))
            .cloned()
            .collect();
        values.extend(ours);
        Ok(Self {
            session,
            slot,
            id,
            store: Store(values).into(),
        })
    }
}

impl IMMDevice_Impl for Device_Impl {
    fn Activate(
        &self,
        iid: *const GUID,
        _context: CLSCTX,
        _params: *const PROPVARIANT,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if iid.is_null() || out.is_null() {
            return Err(E_POINTER.into());
        }
        let endpoint: IUnknown = Endpoint::new(self.session, self.slot).into();
        // SAFETY: both pointers are the caller's and checked non-null.
        unsafe { endpoint.query(iid, out) }.ok()
    }

    fn OpenPropertyStore(&self, access: STGM) -> Result<IPropertyStore> {
        if access != STGM_READ {
            return Err(STG_E_ACCESSDENIED.into());
        }
        Ok(self.store.clone())
    }

    fn GetId(&self) -> Result<PWSTR> {
        let wide: &[u16] = &self.id;
        let bytes = (wide.len() + 1) * 2;
        // SAFETY: a fresh allocation the caller frees with CoTaskMemFree.
        let out = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
        if out.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        // SAFETY: `out` holds `wide.len() + 1` u16s.
        unsafe {
            core::ptr::copy_nonoverlapping(wide.as_ptr(), out, wide.len());
            out.add(wide.len()).write(0);
        }
        Ok(PWSTR(out))
    }

    fn GetState(&self) -> Result<DEVICE_STATE> {
        Ok(if present(self.session, self.slot) {
            DEVICE_STATE_ACTIVE
        } else {
            DEVICE_STATE_NOTPRESENT
        })
    }
}

impl IMMEndpoint_Impl for Device_Impl {
    fn GetDataFlow(&self) -> Result<EDataFlow> {
        Ok(eRender)
    }
}

/// A read-only property store. Values never change after construction.
#[implement(IPropertyStore)]
struct Store(Vec<(PROPERTYKEY, PROPVARIANT)>);

impl IPropertyStore_Impl for Store_Impl {
    fn GetCount(&self) -> Result<u32> {
        u32::try_from(self.0.len()).map_err(|_| E_INVALIDARG.into())
    }

    fn GetAt(&self, index: u32, key: *mut PROPERTYKEY) -> Result<()> {
        let found = usize::try_from(index)
            .ok()
            .and_then(|index| self.0.get(index))
            .ok_or(E_INVALIDARG)?;
        if key.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `key` is the caller's, checked non-null.
        unsafe { key.write(found.0) };
        Ok(())
    }

    fn GetValue(&self, key: *const PROPERTYKEY) -> Result<PROPVARIANT> {
        if key.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `key` is the caller's, checked non-null.
        let key = unsafe { *key };
        Ok(self
            .0
            .iter()
            .find(|(own, _)| *own == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_default())
    }

    fn SetValue(&self, _key: *const PROPERTYKEY, _value: *const PROPVARIANT) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn Commit(&self) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
}

fn remote_audio_store(base: &IMMDeviceEnumerator) -> Result<IPropertyStore> {
    // SAFETY: plain call on the stock enumerator.
    let device = unsafe { base.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    // SAFETY: plain call on the stock device.
    unsafe { device.OpenPropertyStore(STGM_READ) }
}

fn copy_store(store: &IPropertyStore) -> Vec<(PROPERTYKEY, PROPVARIANT)> {
    // SAFETY: reads of the stock store through its own count and indices.
    let count = unsafe { store.GetCount() }.unwrap_or(0);
    (0..count)
        .filter_map(|index| {
            let mut key = PROPERTYKEY::default();
            // SAFETY: an index below the store's count, and a key to receive.
            unsafe { store.GetAt(index, &mut key) }.ok()?;
            // SAFETY: a key the store just named.
            let value = unsafe { store.GetValue(&key) }.ok()?;
            Some((key, value))
        })
        .collect()
}

/// `{50464453-0000-0000-0000-0000000000NN}`: the pad's own container.
fn container(slot: u8) -> GUID {
    GUID::from_values(0x5046_4453, 0, 0, [0, 0, 0, 0, 0, 0, 0, slot])
}

fn raw(vt: VARENUM, value: PROPVARIANT_0_0_0) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

/// `VT_LPWSTR`, as MMDevAPI's own string properties are.
fn text(value: &str) -> Result<PROPVARIANT> {
    let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
    // SAFETY: a fresh allocation the PROPVARIANT owns; PropVariantClear frees it.
    let out = unsafe { CoTaskMemAlloc(wide.len() * 2) }.cast::<u16>();
    if out.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    // SAFETY: `out` holds `wide.len()` u16s.
    unsafe { core::ptr::copy_nonoverlapping(wide.as_ptr(), out, wide.len()) };
    Ok(raw(
        VT_LPWSTR,
        PROPVARIANT_0_0_0 {
            pwszVal: PWSTR(out),
        },
    ))
}

fn guid(value: &GUID) -> Result<PROPVARIANT> {
    // SAFETY: copies the GUID into a PROPVARIANT that owns the copy.
    unsafe { InitPropVariantFromCLSID(value) }
}

fn boolean(value: bool) -> PROPVARIANT {
    raw(
        VT_BOOL,
        PROPVARIANT_0_0_0 {
            boolVal: if value { VARIANT_TRUE } else { VARIANT_FALSE },
        },
    )
}

/// `VT_BLOB`, as the stored format properties are.
fn blob(bytes: &[u8]) -> Result<PROPVARIANT> {
    // SAFETY: a fresh allocation the PROPVARIANT owns; PropVariantClear frees it.
    let out = unsafe { CoTaskMemAlloc(bytes.len()) }.cast::<u8>();
    if out.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    // SAFETY: `out` holds `bytes.len()` bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    Ok(raw(
        VT_BLOB,
        PROPVARIANT_0_0_0 {
            blob: BLOB {
                cbSize: u32::try_from(bytes.len()).map_err(|_| E_INVALIDARG)?,
                pBlobData: out,
            },
        },
    ))
}
