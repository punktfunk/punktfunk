//! Safe(ly-contracted) helpers over the WDF request/memory/property DDIs the pad drivers use. The
//! pattern: a framework callback converts its raw `WDFREQUEST` into a [`Request`] token **once**
//! (`unsafe`, the framework's validity guarantee is the contract); every operation after that is a
//! safe method, and completion consumes the token so a request cannot be completed twice or used
//! after completion from safe code.

use wdk_sys::{
    NTSTATUS, WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFFILEOBJECT, WDFMEMORY, WDFQUEUE, WDFREQUEST,
    call_unsafe_wdf_function_binding,
};

const STATUS_INVALID_BUFFER_SIZE: NTSTATUS = 0xC000_0206u32 as NTSTATUS;
/// DEVICE_REGISTRY_PROPERTY: DevicePropertyLocationInformation (the const isn't re-exported at the
/// wdk_sys root; the value is stable WDM).
const DEVICE_PROPERTY_LOCATION_INFORMATION: i32 = 10;
/// DEVICE_REGISTRY_PROPERTY: DevicePropertyHardwareID — the devnode's `REG_MULTI_SZ` hardware-id
/// list (same caveat: stable WDM value, not re-exported).
const DEVICE_PROPERTY_HARDWARE_ID: i32 = 1;

#[inline]
fn nt_success(s: NTSTATUS) -> bool {
    s >= 0
}

/// A validity token for one framework-delivered `WDFREQUEST`. Not `Copy`/`Clone`: completing or
/// forwarding consumes it, so safe code cannot touch a request the framework already owns again.
pub struct Request(WDFREQUEST);

impl Request {
    /// Wrap the raw request handed to the current framework callback.
    ///
    // unsafe-fn-no-op-ok: contract-deferring constructor — the body only wraps the handle; every
    // later `complete`/`forward` call trusts the framework-liveness promised here.
    /// # Safety
    /// `raw` must be the live, framework-provided `WDFREQUEST` of the callback invocation this is
    /// called from (WDF owns handle validity; a forged/dangling handle is framework UB).
    pub unsafe fn new(raw: WDFREQUEST) -> Request {
        Request(raw)
    }

    /// The process that sent this request. What a control IOCTL creates belongs to it, and a
    /// later IOCTL from another process must not reach it.
    pub fn requestor_pid(&self) -> u32 {
        // SAFETY: `self.0` is the live callback request per `Request::new`'s contract.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetRequestorProcessId, self.0) }
    }

    /// The file object — the caller's device handle — this request arrived on.
    pub fn file_object(&self) -> WDFFILEOBJECT {
        // SAFETY: `self.0` is the live callback request per `Request::new`'s contract.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, self.0) }
    }

    /// Complete the request with `status` (consumes the token — the framework owns it afterwards).
    pub fn complete(self, status: NTSTATUS) {
        // SAFETY: `self.0` is the live callback request per `Request::new`'s contract, not yet
        // completed or forwarded (both consume the token).
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, self.0, status) };
    }

    /// Copy `src` into the request's (buffered) output buffer and set the completed byte count.
    /// Returns the status to complete with (`STATUS_INVALID_BUFFER_SIZE` if the buffer is short).
    pub fn copy_to_output(&self, src: &[u8]) -> NTSTATUS {
        let mut mem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; `mem` receives the memory handle.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, self.0, &mut mem)
        };
        if !nt_success(st) {
            return st;
        }
        let mut outlen: usize = 0;
        // SAFETY: `mem` is the valid memory object just retrieved; `outlen` receives its size.
        let _ = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut outlen) };
        if outlen < src.len() {
            return STATUS_INVALID_BUFFER_SIZE;
        }
        // SAFETY: `mem` is valid and at least `src.len()` bytes; `src` is a live borrow.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCopyFromBuffer,
                mem,
                0usize,
                src.as_ptr() as *mut core::ffi::c_void,
                src.len()
            )
        };
        if !nt_success(st) {
            return st;
        }
        // SAFETY: `self.0` is the live callback request.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestSetInformation, self.0, src.len() as u64)
        };
        0 // STATUS_SUCCESS
    }

    /// The request's input buffer: up to `cap` bytes copied out, plus the buffer's TRUE length.
    /// `Err(status)` if the input memory can't be retrieved (propagate as the completion status).
    pub fn input_bytes(&self, cap: usize) -> Result<(Vec<u8>, usize), NTSTATUS> {
        let mut inmem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; `inmem` receives the memory handle.
        let st = unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, self.0, &mut inmem)
        };
        if !nt_success(st) {
            return Err(st);
        }
        let mut len: usize = 0;
        // SAFETY: `inmem` is the valid memory object just retrieved; `len` receives its size.
        let p = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut len) }
            as *const u8;
        if p.is_null() {
            return Ok((Vec::new(), 0));
        }
        let n = len.min(cap);
        // SAFETY: `p` is valid for `len` bytes per `WdfMemoryGetBuffer`; we read `n <= len`.
        let bytes = unsafe { core::slice::from_raw_parts(p, n) }.to_vec();
        Ok((bytes, len))
    }

    /// Answer a `GET_STRING` with `s` as NUL-terminated UTF-16 ([`crate::hid::utf16z`]).
    pub fn copy_utf16z_to_output(&self, s: &str) -> NTSTATUS {
        self.copy_to_output(&crate::hid::utf16z(s))
    }

    /// The request's output-buffer LENGTH (0 if unavailable) — UMDF HID marshalling carries the
    /// output-report id in it.
    pub fn output_buffer_len(&self) -> usize {
        let mut outmem: WDFMEMORY = core::ptr::null_mut();
        // SAFETY: `self.0` is the live callback request; output memory is optional here.
        if !nt_success(unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, self.0, &mut outmem)
        }) {
            return 0;
        }
        let mut outlen: usize = 0;
        // SAFETY: `outmem` is the valid memory object just retrieved; `outlen` receives its size.
        let _ =
            unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, outmem, &mut outlen) };
        outlen
    }

    /// Set the completed-bytes information field (for paths that complete with a length but no
    /// output copy, e.g. echoing an output report's length).
    pub fn set_information(&self, info: u64) {
        // SAFETY: `self.0` is the live callback request.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestSetInformation, self.0, info) };
    }

    /// Forward the request to a manual queue. On success the framework owns it (the token is
    /// consumed by value — the caller cannot touch the request again); on failure the token is
    /// handed back with the status so the caller completes it. (`Request` has no `Drop`, so the
    /// consumed-on-success token simply falls out of scope — nothing to run.)
    ///
    /// # Safety
    /// `queue` must be a live manual `WDFQUEUE` of the same device (e.g. the one created in
    /// `EvtDeviceAdd` and stashed in a static).
    pub unsafe fn forward_to_queue(self, queue: WDFQUEUE) -> Result<(), (Request, NTSTATUS)> {
        // SAFETY: `self.0` is the live callback request; `queue` is live per this fn's contract.
        let st =
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, self.0, queue) };
        if nt_success(st) {
            Ok(())
        } else {
            Err((self, st))
        }
    }
}

/// Pop the next pended request off a manual queue (`None` when empty).
///
/// # Safety
/// `queue` must be a live manual `WDFQUEUE` (e.g. the timer's parent object).
pub unsafe fn retrieve_next_request(queue: WDFQUEUE) -> Option<Request> {
    let mut request: WDFREQUEST = core::ptr::null_mut();
    // SAFETY: `queue` is live per this fn's contract; `request` receives the next pended request.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, queue, &mut request)
    };
    // SAFETY: on success `request` is a live framework request this caller now services — the
    // exact contract `Request::new` requires.
    nt_success(st).then(|| unsafe { Request::new(request) })
}

/// Read the pad index the host stamped into the device Location (`pszDeviceLocation`), a
/// NUL-terminated UTF-16 decimal string. Defaults to 0 (single-pad) if absent. (The WDFMEMORY is
/// device-parented and freed by the framework at device teardown — one small alloc per device add.)
///
/// # Safety
/// `device` must be the live `WDFDEVICE` created in the current `EvtDeviceAdd`.
pub unsafe fn query_location_index(device: WDFDEVICE) -> u32 {
    let mut mem: wdk_sys::WDFMEMORY = core::ptr::null_mut();
    // SAFETY: `device` is live per this fn's contract; property = LocationInformation; pool ignored
    // in UMDF; `mem` receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceAllocAndQueryProperty,
            device,
            DEVICE_PROPERTY_LOCATION_INFORMATION,
            0,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut mem
        )
    };
    if !nt_success(st) || mem.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    // SAFETY: `mem` is the valid memory object just allocated; `len` receives its size.
    let buf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut len) }
        as *const u16;
    if buf.is_null() {
        return 0;
    }
    let units = (len / 2).min(8);
    // SAFETY: `buf` is valid for `len` bytes per `WdfMemoryGetBuffer`; we read `units * 2 <= len`.
    let chars = unsafe { core::slice::from_raw_parts(buf, units) };
    let mut idx: u32 = 0;
    let mut any = false;
    for &c in chars {
        if c == 0 {
            break;
        }
        if (0x30..=0x39).contains(&c) {
            idx = idx.wrapping_mul(10).wrapping_add((c - 0x30) as u32);
            any = true;
        }
    }
    if any { idx } else { 0 }
}

/// Read a binary device property the host set at `SwDeviceCreate`; `None` when it is absent.
///
/// # Safety
/// `device` must be a live `WDFDEVICE`.
pub unsafe fn query_binary_property(device: WDFDEVICE, fmtid: u128, pid: u32) -> Option<Vec<u8>> {
    let key = wdk_sys::DEVPROPKEY {
        fmtid: wdk_sys::GUID {
            Data1: (fmtid >> 96) as _,
            Data2: (fmtid >> 80) as _,
            Data3: (fmtid >> 64) as _,
            Data4: (fmtid as u64).to_be_bytes(),
        },
        pid,
    };
    let mut data = wdk_sys::WDF_DEVICE_PROPERTY_DATA {
        Size: core::mem::size_of::<wdk_sys::WDF_DEVICE_PROPERTY_DATA>() as _,
        PropertyKey: &key,
        ..Default::default()
    };
    let mut mem: wdk_sys::WDFMEMORY = core::ptr::null_mut();
    let mut ty: wdk_sys::DEVPROPTYPE = 0;
    // SAFETY: `device` is live per this fn's contract; `data` points at `key`, both outliving the
    // call; pool ignored in UMDF; `mem` receives a device-parented handle the framework frees.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceAllocAndQueryPropertyEx,
            device,
            &mut data,
            0,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut mem,
            &mut ty
        )
    };
    if !nt_success(st) || mem.is_null() {
        return None;
    }
    let mut len: usize = 0;
    // SAFETY: `mem` is the valid memory object just allocated; `len` receives its size.
    let buf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut len) }
        as *const u8;
    if buf.is_null() {
        return None;
    }
    // SAFETY: `buf` is valid for `len` bytes per `WdfMemoryGetBuffer`.
    Some(unsafe { core::slice::from_raw_parts(buf, len) }.to_vec())
}

/// Read the devnode's hardware-id list (`pszzHardwareIds`) as one lowercase ASCII string, ids
/// separated by `;` (e.g. `"pf_steamdeck;usb\\vid_28de&pid_1205&rev_0100&mi_02;…"`). Empty if the
/// property is absent.
///
/// This is the ONE identity surface available at `EvtDeviceAdd` — before hidclass asks for the
/// report descriptor and attributes, and long before the sealed channel can deliver (delivery goes
/// through the HID device interface, which only exists once those very queries are answered). A
/// driver that must know *which* device it is at descriptor time has to read it here.
///
/// # Safety
/// `device` must be a live `WDFDEVICE`.
pub unsafe fn query_hardware_ids(device: WDFDEVICE) -> String {
    let mut mem: wdk_sys::WDFMEMORY = core::ptr::null_mut();
    // SAFETY: `device` is live per this fn's contract; property = HardwareID; pool ignored in UMDF;
    // `mem` receives the handle (device-parented — the framework frees it at device teardown).
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceAllocAndQueryProperty,
            device,
            DEVICE_PROPERTY_HARDWARE_ID,
            0,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut mem
        )
    };
    if !nt_success(st) || mem.is_null() {
        return String::new();
    }
    let mut len: usize = 0;
    // SAFETY: `mem` is the valid memory object just allocated; `len` receives its size.
    let buf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut len) }
        as *const u16;
    if buf.is_null() {
        return String::new();
    }
    // SAFETY: `buf` is valid for `len` bytes per `WdfMemoryGetBuffer`; we read exactly `len / 2` u16.
    let chars = unsafe { core::slice::from_raw_parts(buf, len / 2) };
    // REG_MULTI_SZ: NUL-separated, double-NUL-terminated. Ids are ASCII in practice; anything else
    // is dropped rather than lossily transliterated (this string is only ever substring-matched).
    let mut out = String::with_capacity(chars.len());
    for &c in chars {
        match c {
            0 => {
                if out.ends_with(';') || out.is_empty() {
                    break; // the terminating second NUL
                }
                out.push(';');
            }
            0x20..=0x7E => out.push((c as u8 as char).to_ascii_lowercase()),
            _ => {}
        }
    }
    out
}
