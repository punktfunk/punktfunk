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
    CFBoolean, CFData, CFDictionary, CFIndex, CFNumber, CFRetained, CFString, CFType,
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
use std::sync::{mpsc, Arc, Mutex};
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

/// The entitlement every create needs.
const ENTITLEMENT: &str = "com.apple.developer.hid.virtual.device";

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    /// `SecTaskRef` for this process, +1. A NULL allocator is the default one.
    fn SecTaskCreateFromSelf(allocator: *const std::ffi::c_void) -> *mut CFType;
    /// The entitlement's value in `task`'s signature, +1; NULL when absent.
    fn SecTaskCopyValueForEntitlement(
        task: &CFType,
        entitlement: &CFString,
        error: *mut *mut CFType,
    ) -> *mut CFType;
}

/// Whether this process's signature carries [`ENTITLEMENT`]. Reads the signature and creates no
/// device. macOS kills at launch a process that claims this restricted entitlement without a
/// profile allowing it, so a claim in a running process is a granted one.
pub fn entitled() -> bool {
    let name = CFString::from_str(ENTITLEMENT);
    // SAFETY: both calls follow the Create/Copy rule, so each non-NULL return is +1 and
    // `CFRetained::from_raw` adopts it exactly once. `task` and `name` are live CF objects for
    // the second call, and a NULL error out-pointer is allowed.
    let value = unsafe {
        let Some(task) = NonNull::new(SecTaskCreateFromSelf(std::ptr::null())) else {
            return false;
        };
        let task = CFRetained::from_raw(task);
        NonNull::new(SecTaskCopyValueForEntitlement(
            &task,
            &name,
            std::ptr::null_mut(),
        ))
        .map(|v| CFRetained::from_raw(v))
    };
    value.is_some_and(|v| v.downcast_ref::<CFBoolean>().is_some_and(|b| b.as_bool()))
}

#[cfg(test)]
mod tests {
    /// A test binary is linker-signed with no entitlements: the read answers, and answers no.
    #[test]
    fn a_test_binary_is_not_entitled() {
        assert!(!super::entitled());
    }
}
