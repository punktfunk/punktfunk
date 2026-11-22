//! One virtual HID device on macOS: an `IOHIDUserDevice` this process creates, the transport
//! every macOS pad drives. The shape of the Linux `UhidDevice`: an identity and descriptor at
//! open, input reports in, output reports back through [`HidUserDevice::poll`].
//!
//! Get-report is a synchronous callback here, so the feature replies are fixed at open
//! ([`Identity::features`]) and answered inside it. A create needs the
//! `com.apple.developer.hid.virtual.device` entitlement in this process's signature; without it
//! the system returns no device.

use anyhow::{bail, Result};
use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{
    CFData, CFDictionary, CFIndex, CFNumber, CFRetained, CFString, CFType,
};
use objc2_io_kit::{
    kIOHIDManufacturerKey, kIOHIDProductIDKey, kIOHIDProductKey, kIOHIDReportDescriptorKey,
    kIOHIDSerialNumberKey, kIOHIDTransportKey, kIOHIDVendorIDKey, kIOHIDVersionNumberKey,
    kIOReturnSuccess, kIOReturnUnsupported, IOHIDReportType, IOHIDUserDevice, IOReturn,
};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

unsafe extern "C" {
    /// The clock HID report timestamps are in. libSystem's; no arguments, no preconditions.
    safe fn mach_absolute_time() -> u64;
}

/// What the system sees: the HID attributes, the descriptor, and the feature replies.
pub struct Identity {
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
    pub name: &'static str,
    pub manufacturer: &'static str,
    pub serial: String,
    pub rdesc: &'static [u8],
    /// Transport `Bluetooth` instead of the `Virtual` macOS stamps at create. SDL takes a
    /// Bluetooth Xbox id on any other transport for a wired pad and reads no input from it.
    pub bluetooth: bool,
    /// Feature-report replies, each led by its report id.
    pub features: Vec<Vec<u8>>,
}

impl Identity {
    /// `devtype`'s row of the pad-kind table under `rdesc`: no feature replies, transport
    /// `Virtual`.
    pub fn of(devtype: u8, serial: String, rdesc: &'static [u8]) -> Identity {
        let kind = pf_driver_proto::pad_kind::served_kind(devtype);
        Identity {
            vendor: kind.vid.into(),
            product: kind.pid.into(),
            version: kind.version.into(),
            name: kind.product,
            manufacturer: kind.manufacturer,
            serial,
            rdesc,
            bluetooth: false,
            features: Vec::new(),
        }
    }
}

/// The create-failure suffix every macOS pad logs: a refused create is a missing entitlement.
pub const CREATE_HINT: &str = " (the host's signature needs the HID virtual device entitlement)";

/// Output reports the callback queued that [`HidUserDevice::poll`] has not drained. A pump that
/// falls behind keeps the newest.
const OUTPUT_QUEUE: usize = 64;

type GetBlock = RcBlock<dyn Fn(IOHIDReportType, u32, NonNull<u8>, NonNull<CFIndex>) -> IOReturn>;
type SetBlock = RcBlock<dyn Fn(IOHIDReportType, u32, NonNull<u8>, CFIndex) -> IOReturn>;

/// What must outlive every callback: released only after the cancel handler ran.
struct Live {
    dev: CFRetained<IOHIDUserDevice>,
    _queue: DispatchRetained<DispatchQueue>,
    _get: GetBlock,
    _set: SetBlock,
    _cancel: RcBlock<dyn Fn()>,
}

/// One `IOHIDUserDevice`. Drop cancels it and releases it once the cancel completes.
pub struct HidUserDevice {
    live: ManuallyDrop<Live>,
    outputs: Arc<Mutex<VecDeque<Vec<u8>>>>,
    cancelled: mpsc::Receiver<()>,
    /// The system refused an input report: the device is gone.
    dead: bool,
}

fn key(k: &CStr) -> CFRetained<CFString> {
    CFString::from_str(k.to_str().unwrap_or_default())
}

impl HidUserDevice {
    /// Create the device and start answering it.
    pub fn open(id: &Identity) -> Result<HidUserDevice> {
        let num = |v: u32| CFNumber::new_i32(v as i32);
        let (vendor, product, version) = (num(id.vendor), num(id.product), num(id.version));
        let rdesc = CFData::from_bytes(id.rdesc);
        let (name, maker) = (
            CFString::from_str(id.name),
            CFString::from_str(id.manufacturer),
        );
        let serial = CFString::from_str(&id.serial);
        let keys = [
            key(kIOHIDReportDescriptorKey),
            key(kIOHIDVendorIDKey),
            key(kIOHIDProductIDKey),
            key(kIOHIDVersionNumberKey),
            key(kIOHIDProductKey),
            key(kIOHIDManufacturerKey),
            key(kIOHIDSerialNumberKey),
        ];
        let values: [&CFType; 7] = [&rdesc, &vendor, &product, &version, &name, &maker, &serial];
        let keys: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
        let props = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        // SAFETY: `props` maps CFString keys of IOHIDKeys.h to values of the types those keys
        // name: CFData for the descriptor, CFNumber for ids, CFString for strings.
        let Some(dev) = (unsafe { IOHIDUserDevice::with_properties(None, props.as_opaque(), 0) })
        else {
            bail!(
                "create virtual HID device {:04x}:{:04x}: refused",
                id.vendor,
                id.product
            );
        };

        let features = id.features.clone();
        let get: GetBlock = RcBlock::new(
            move |_ty, report_id, buf: NonNull<u8>, len: NonNull<CFIndex>| -> IOReturn {
                let Some(reply) = features
                    .iter()
                    .find(|f| f.first().is_some_and(|&b| u32::from(b) == report_id))
                else {
                    return kIOReturnUnsupported as IOReturn;
                };
                // SAFETY: for the length of this call IOKit hands a buffer writable for `*len`
                // bytes and a writable length cell; at most `*len` bytes are written.
                unsafe {
                    let n = reply.len().min(usize::try_from(*len.as_ptr()).unwrap_or(0));
                    std::ptr::copy_nonoverlapping(reply.as_ptr(), buf.as_ptr(), n);
                    *len.as_ptr() = n as CFIndex;
                }
                kIOReturnSuccess
            },
        );
        let outputs = Arc::new(Mutex::new(VecDeque::new()));
        let queued = Arc::clone(&outputs);
        let set: SetBlock = RcBlock::new(
            move |ty: IOHIDReportType, _id, buf: NonNull<u8>, len: CFIndex| -> IOReturn {
                if ty == IOHIDReportType::Output {
                    // SAFETY: for the length of this call IOKit hands `len` readable bytes.
                    let data = unsafe {
                        std::slice::from_raw_parts(buf.as_ptr(), usize::try_from(len).unwrap_or(0))
                    };
                    let mut q = queued.lock().unwrap_or_else(|e| e.into_inner());
                    if q.len() == OUTPUT_QUEUE {
                        q.pop_front();
                    }
                    q.push_back(data.to_vec());
                }
                kIOReturnSuccess
            },
        );
        let (tx, cancelled) = mpsc::channel();
        let cancel: RcBlock<dyn Fn()> = RcBlock::new(move || {
            let _ = tx.send(());
        });
        let queue = DispatchQueue::new("io.unom.punktfunk.pad", None);
        // SAFETY: the blocks match IOHIDUserDevice.h's callback signatures, touch only Send and
        // Sync state they own from the device's queue, and live in `Live` until the cancel
        // handler ran, after which IOKit calls none. Each is set once, before `activate`.
        unsafe {
            dev.register_get_report_block(RcBlock::as_ptr(&get) as *mut DynBlock<_>);
            dev.register_set_report_block(RcBlock::as_ptr(&set) as *mut DynBlock<_>);
            dev.set_dispatch_queue(&queue);
            dev.set_cancel_handler(RcBlock::as_ptr(&cancel) as *mut DynBlock<_>);
        }
        dev.activate();
        // macOS stamps `Virtual` at create whatever the properties ask; the property set after
        // activation sticks.
        if id.bluetooth {
            // SAFETY: `Transport` takes a CFString.
            let ok = unsafe {
                dev.set_property(&key(kIOHIDTransportKey), &CFString::from_str("Bluetooth"))
            };
            if !ok {
                tracing::warn!(product = id.name, "virtual pad transport stays Virtual");
            }
        }
        Ok(HidUserDevice {
            live: ManuallyDrop::new(Live {
                dev,
                _queue: queue,
                _get: get,
                _set: set,
                _cancel: cancel,
            }),
            outputs,
            cancelled,
            dead: false,
        })
    }

    /// One input report, led by its report id where the descriptor numbers them. A refused
    /// report marks the device gone ([`Self::alive`]).
    pub fn write_input(&mut self, data: &[u8]) -> Result<()> {
        // SAFETY: the pointer is `data`, readable for `data.len()` bytes; IOKit copies the
        // report before returning.
        let rc = unsafe {
            self.live.dev.handle_report_with_time_stamp(
                mach_absolute_time(),
                NonNull::from(data).cast(),
                data.len() as CFIndex,
            )
        };
        if rc != kIOReturnSuccess {
            self.dead = true;
            bail!("write virtual HID input report: IOReturn {rc:#x}");
        }
        Ok(())
    }

    /// Hand every output report queued since the last poll to `on_output`, oldest first, each
    /// led by its report id. Feature writes are acknowledged and dropped.
    pub fn poll(&mut self, mut on_output: impl FnMut(&mut HidUserDevice, &[u8])) {
        let drained = std::mem::take(&mut *self.outputs.lock().unwrap_or_else(|e| e.into_inner()));
        for report in &drained {
            on_output(self, report);
        }
    }

    /// `false` once the system refused an input report.
    pub fn alive(&self) -> bool {
        !self.dead
    }
}

impl Drop for HidUserDevice {
    fn drop(&mut self) {
        self.live.dev.cancel();
        if self.cancelled.recv_timeout(Duration::from_secs(1)).is_err() {
            // Releasing now could free blocks a pending callback still runs; leak instead.
            tracing::warn!("virtual HID device cancel did not complete — leaking it");
            return;
        }
        // SAFETY: the cancel handler ran, so IOKit runs no callback of this device again, and
        // `live` is dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.live) };
    }
}

/// A vendor-defined one-byte input report: no driver binds it.
const PROBE_RDESC: &[u8] = &[
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x02, 0xC0,
];

/// Whether this process may create a virtual HID device: one create of a neutral device on the
/// first call, removed at once; later calls return that answer.
pub fn probe() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        let probe = Identity {
            vendor: 0,
            product: 0,
            version: 0,
            name: "Punktfunk probe",
            manufacturer: "Punktfunk",
            serial: "punktfunk-probe".into(),
            rdesc: PROBE_RDESC,
            bluetooth: false,
            features: Vec::new(),
        };
        HidUserDevice::open(&probe).is_ok()
    })
}
