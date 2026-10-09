//! CUDA Driver API FFI: opaque handles, `cuda.h` structs, the `dlopen`'d `libcuda.so.1`
//! table ([`CudaApi`] / [`cuda_api`]), `unsafe` `cuXxx` wrappers, and [`ck`].
//!
//! No higher-level state. Shared `CUcontext`, device buffers, GL/dmabuf interop, and
//! cursor blend live in [`super`] and drive this layer.
//!
//! Flattened struct layout is pinned by the `const` asserts below. A transcription slip
//! compiles and does not crash — the driver reads a pitch, size, or fd from the wrong
//! bytes. `Library` is leaked so the fn pointers stay process-lifetime.

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{bail, Result};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

pub type CUresult = c_uint; // CUDA_SUCCESS == 0
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void; // opaque CUctx_st*
pub type CUstream = *mut c_void; // opaque CUstream_st*
pub type CUdeviceptr = u64;
pub type CUgraphicsResource = *mut c_void;
pub type CUarray = *mut c_void;
pub type CUexternalMemory = *mut c_void; // opaque CUextMemory_st*
pub type CUexternalSemaphore = *mut c_void; // opaque CUextSemaphore_st*
pub type CUevent = *mut c_void; // opaque CUevent_st*

/// `CUDA_ERROR_NOT_READY` (cuda.h): the queried work has not finished yet.
pub const CUDA_ERROR_NOT_READY: CUresult = 600;

/// `CUmemorytype` (cuda.h): HOST=1, DEVICE=2, ARRAY=3, UNIFIED=4.
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;
pub const CU_MEMORYTYPE_ARRAY: c_uint = 3;

/// `CUctx_flags` (cuda.h). Spin would steal compositor/send cores while waiting on a copy.
/// AUTO (0) picks SPIN vs YIELD by core count; we force BLOCKING_SYNC.
pub(crate) const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x04;

/// `cuStreamCreateWithPriority` flag: don't implicitly synchronize with the legacy NULL stream.
pub(crate) const CU_STREAM_NON_BLOCKING: c_uint = 0x01;

/// `CUDA_MEMCPY2D` (`cuda.h` `_v2` ABI). Field order is load-bearing; the driver reads by offset.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_MEMCPY2D {
    pub srcXInBytes: usize,
    pub srcY: usize,
    pub srcMemoryType: c_uint,
    pub srcHost: *const c_void,
    pub srcDevice: CUdeviceptr,
    pub srcArray: CUarray,
    pub srcPitch: usize,
    pub dstXInBytes: usize,
    pub dstY: usize,
    pub dstMemoryType: c_uint,
    pub dstHost: *mut c_void,
    pub dstDevice: CUdeviceptr,
    pub dstArray: CUarray,
    pub dstPitch: usize,
    pub WidthInBytes: usize,
    pub Height: usize,
}

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` (`cuda.h`, 64-bit). `handle` is a union sized to the
/// win32 two-pointer struct (16 bytes, align 8); OPAQUE_FD reads only the first 4 (`int fd`).
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
    pub type_: c_uint,
    pub(crate) _pad: u32,
    pub handle: [u64; 2], // union { int fd; {void*,void*} win32; void* nvSciBufObject }
    pub size: u64,
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad2: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
    pub offset: u64,
    pub size: u64,
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
}

pub const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_uint = 1;

/// `CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC` (`cuda.h`, 64-bit). Same union flatten as the
/// memory desc; fd types read the first 4 bytes. No `size` — a semaphore has none.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
    pub type_: c_uint,
    pub(crate) _pad: u32,
    pub handle: [u64; 2], // union { int fd; {void*,void*} win32; const void* nvSciSyncObj }
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad2: u32,
}

/// `CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS` (`cuda.h`, 64-bit), flattened. Only
/// `params.fence.value` is set; `flags` sits at offset 72. 144 bytes.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
    pub value: u64, // params.fence.value — timeline value this signal sets
    pub(crate) _nv_sci_sync: u64,
    pub(crate) _keyed_mutex_key: u64,
    pub(crate) _params_reserved: [c_uint; 12],
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
}

/// `CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS` (`cuda.h`, 64-bit), flattened like the signal
/// params. C `keyedMutex` is `{ u64 key; u32 timeoutMs; }` = 16 with tail padding, hence
/// the explicit pad before the 10 reserved words. 144 bytes.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS {
    pub value: u64, // params.fence.value — wait until the timeline reaches this value
    pub(crate) _nv_sci_sync: u64,
    pub(crate) _keyed_mutex_key: u64,
    pub(crate) _keyed_mutex_timeout: c_uint,
    pub(crate) _keyed_mutex_pad: u32,
    pub(crate) _params_reserved: [c_uint; 10],
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
}

/// `CUexternalSemaphoreHandleType` (`cuda.h`): Vulkan **timeline** semaphore exported as
/// OPAQUE_FD (`vkGetSemaphoreFdKHR`). Not a binary semaphore.
pub const CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD: c_uint = 9;

// Driver reads these by offset. A transcription slip compiles and does not crash;
// it copies the wrong pitch/size/fd. `const` so a release build cannot skip them.
const _: () = {
    use std::mem::{offset_of, size_of};

    // 16 pointer-sized slots = 128. Each `c_uint` memory-type is padded to align the
    // pointer after it — the part a hand transcription gets wrong.
    assert!(size_of::<CUDA_MEMCPY2D>() == 128);
    assert!(offset_of!(CUDA_MEMCPY2D, srcMemoryType) == 16);
    assert!(offset_of!(CUDA_MEMCPY2D, srcHost) == 24);
    assert!(offset_of!(CUDA_MEMCPY2D, srcPitch) == 48);
    assert!(offset_of!(CUDA_MEMCPY2D, dstMemoryType) == 72);
    assert!(offset_of!(CUDA_MEMCPY2D, dstHost) == 80);
    assert!(offset_of!(CUDA_MEMCPY2D, dstPitch) == 104);
    assert!(offset_of!(CUDA_MEMCPY2D, WidthInBytes) == 112);
    assert!(offset_of!(CUDA_MEMCPY2D, Height) == 120);

    // type(4)+pad(4)+union(16)+size(8)+flags(4)+reserved[16](64) = 104.
    assert!(size_of::<CUDA_EXTERNAL_MEMORY_HANDLE_DESC>() == 104);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, handle) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, size) == 24);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, flags) == 32);

    // offset(8)+size(8)+flags(4)+reserved[16](64), tail-padded to 8 = 88.
    assert!(size_of::<CUDA_EXTERNAL_MEMORY_BUFFER_DESC>() == 88);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_BUFFER_DESC, size) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_BUFFER_DESC, flags) == 16);

    // type(4)+pad(4)+union(16)+flags(4)+reserved[16](64) = 96. No `size` — a semaphore has none.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC>() == 96);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC, handle) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC, flags) == 24);

    // params{fence(8)+nvSciSync(8)+keyedMutex(8)+reserved[12](48)} = 72, flags at 72 → 144.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS>() == 144);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS, value) == 0);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS, flags) == 72);

    // As signal, but `keyedMutex` is `{u64 key; u32 timeoutMs;}` = 16 with tail padding
    // (explicit pad before the 10 reserved words). Still 72 to `flags` → 144.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS>() == 144);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS, value) == 0);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS, flags) == 72);
};

/// `CUipcMemHandle` (`cuda.h`): 64-byte opaque id for a device allocation across processes.
/// Passed **by value** (`struct { char reserved[64]; }`). Plain bytes; ship over a socket.
pub const CU_IPC_HANDLE_SIZE: usize = 64;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUipcMemHandle {
    pub reserved: [u8; CU_IPC_HANDLE_SIZE],
}

/// `CUipcMem_flags`: lazy peer access on `cuIpcOpenMemHandle`. No-op for same-device open
/// (our only case).
pub(crate) const CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS: c_uint = 0x1;

/// Runtime Driver API table from `dlopen("libcuda.so.1")`, not `#[link(name = "cuda")]`.
/// A hard link would refuse to start the binary where `libcuda` is absent. `cuda_api()`
/// leaks the `Library` so the fn pointers stay process-lifetime.
pub(crate) struct CudaApi {
    cuInit: unsafe extern "C" fn(c_uint) -> CUresult,
    cuDeviceGet: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    /// `CUuuid` is `struct { char bytes[16]; }`: the device's UUID, as Vulkan reports it.
    cuDeviceGetUuid: unsafe extern "C" fn(*mut [u8; 16], CUdevice) -> CUresult,
    cuDeviceGetPCIBusId: unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult,
    cuCtxCreate_v2: unsafe extern "C" fn(*mut CUcontext, c_uint, CUdevice) -> CUresult,
    cuCtxDestroy_v2: unsafe extern "C" fn(CUcontext) -> CUresult,
    cuCtxSetCurrent: unsafe extern "C" fn(CUcontext) -> CUresult,
    cuMemAllocPitch_v2:
        unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult,
    cuMemFree_v2: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
    cuMemcpy2DAsync_v2: unsafe extern "C" fn(*const CUDA_MEMCPY2D, CUstream) -> CUresult,
    cuStreamSynchronize: unsafe extern "C" fn(CUstream) -> CUresult,
    cuEventCreate: unsafe extern "C" fn(*mut CUevent, c_uint) -> CUresult,
    cuEventRecord: unsafe extern "C" fn(CUevent, CUstream) -> CUresult,
    cuEventQuery: unsafe extern "C" fn(CUevent) -> CUresult,
    cuEventDestroy_v2: unsafe extern "C" fn(CUevent) -> CUresult,
    cuCtxGetStreamPriorityRange: unsafe extern "C" fn(*mut c_int, *mut c_int) -> CUresult,
    cuStreamCreateWithPriority: unsafe extern "C" fn(*mut CUstream, c_uint, c_int) -> CUresult,
    cuStreamDestroy_v2: unsafe extern "C" fn(CUstream) -> CUresult,
    cuGraphicsGLRegisterImage:
        unsafe extern "C" fn(*mut CUgraphicsResource, c_uint, c_uint, c_uint) -> CUresult,
    cuGraphicsMapResources:
        unsafe extern "C" fn(c_uint, *mut CUgraphicsResource, *mut c_void) -> CUresult,
    cuGraphicsUnmapResources:
        unsafe extern "C" fn(c_uint, *mut CUgraphicsResource, *mut c_void) -> CUresult,
    cuGraphicsSubResourceGetMappedArray:
        unsafe extern "C" fn(*mut CUarray, CUgraphicsResource, c_uint, c_uint) -> CUresult,
    cuGraphicsUnregisterResource: unsafe extern "C" fn(CUgraphicsResource) -> CUresult,
    cuImportExternalMemory: unsafe extern "C" fn(
        *mut CUexternalMemory,
        *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
    ) -> CUresult,
    cuExternalMemoryGetMappedBuffer: unsafe extern "C" fn(
        *mut CUdeviceptr,
        CUexternalMemory,
        *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
    ) -> CUresult,
    cuDestroyExternalMemory: unsafe extern "C" fn(CUexternalMemory) -> CUresult,
    cuImportExternalSemaphore: unsafe extern "C" fn(
        *mut CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC,
    ) -> CUresult,
    cuDestroyExternalSemaphore: unsafe extern "C" fn(CUexternalSemaphore) -> CUresult,
    cuSignalExternalSemaphoresAsync: unsafe extern "C" fn(
        *const CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS,
        c_uint,
        CUstream,
    ) -> CUresult,
    cuWaitExternalSemaphoresAsync: unsafe extern "C" fn(
        *const CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS,
        c_uint,
        CUstream,
    ) -> CUresult,
    cuIpcGetMemHandle: unsafe extern "C" fn(*mut CUipcMemHandle, CUdeviceptr) -> CUresult,
    cuIpcOpenMemHandle: unsafe extern "C" fn(*mut CUdeviceptr, CUipcMemHandle, c_uint) -> CUresult,
    cuIpcCloseMemHandle: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
}
// `Send`/`Sync` need no `unsafe impl`: every field is a bare fn pointer, already both.
// Addresses stay valid because `cuda_api` `forget`s the `Library`.

/// Wrapper `CUresult` when `libcuda` is not loaded. Non-zero so `ck()`/`!= 0` treat it as
/// a driver error; distinct from real `CUDA_ERROR_*` (all < 1000). The driver never returns it.
pub(crate) const CU_ERROR_NOT_LOADED: CUresult = 999;

pub(crate) static CUDA_API: OnceLock<Option<CudaApi>> = OnceLock::new();

/// Resolve `libcuda.so.1` once. `None` when the NVIDIA driver is absent (expected on
/// AMD/Intel) — debug, not an error.
pub(crate) fn cuda_api() -> Option<&'static CudaApi> {
    CUDA_API
        // SAFETY: `Library::new` runs `libcuda.so.1` initializers (trusted NVIDIA driver);
        // absence is `None`. Each `get::<T>(name)` requires the symbol ABI to match `T`:
        // names are documented Driver API entry points and `T` is the cuda.h/`cudaGL.h`
        // `unsafe extern "C" fn` (`_v2` for ctx/mem). Deref-copy the fn pointer out of
        // `Symbol`, then `forget(lib)` so the mapping outlives the borrow. `OnceLock`
        // runs this once — no aliasing.
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libcuda.so.1")
                .or_else(|_| libloading::Library::new("libcuda.so"))
                .map_err(|e| {
                    tracing::debug!(error = %e, "libcuda not loadable — CUDA zero-copy unavailable (expected on AMD/Intel)");
                })
                .ok()?;
            let api = CudaApi {
                cuInit: *lib.get(b"cuInit\0").ok()?,
                cuDeviceGet: *lib.get(b"cuDeviceGet\0").ok()?,
                cuDeviceGetUuid: *lib.get(b"cuDeviceGetUuid\0").ok()?,
                cuDeviceGetPCIBusId: *lib.get(b"cuDeviceGetPCIBusId\0").ok()?,
                cuCtxCreate_v2: *lib.get(b"cuCtxCreate_v2\0").ok()?,
                cuCtxDestroy_v2: *lib.get(b"cuCtxDestroy_v2\0").ok()?,
                cuCtxSetCurrent: *lib.get(b"cuCtxSetCurrent\0").ok()?,
                cuMemAllocPitch_v2: *lib.get(b"cuMemAllocPitch_v2\0").ok()?,
                cuMemFree_v2: *lib.get(b"cuMemFree_v2\0").ok()?,
                cuMemcpy2DAsync_v2: *lib.get(b"cuMemcpy2DAsync_v2\0").ok()?,
                cuStreamSynchronize: *lib.get(b"cuStreamSynchronize\0").ok()?,
                cuEventCreate: *lib.get(b"cuEventCreate\0").ok()?,
                cuEventRecord: *lib.get(b"cuEventRecord\0").ok()?,
                cuEventQuery: *lib.get(b"cuEventQuery\0").ok()?,
                cuEventDestroy_v2: *lib.get(b"cuEventDestroy_v2\0").ok()?,
                cuCtxGetStreamPriorityRange: *lib.get(b"cuCtxGetStreamPriorityRange\0").ok()?,
                cuStreamCreateWithPriority: *lib.get(b"cuStreamCreateWithPriority\0").ok()?,
                cuStreamDestroy_v2: *lib.get(b"cuStreamDestroy_v2\0").ok()?,
                cuGraphicsGLRegisterImage: *lib.get(b"cuGraphicsGLRegisterImage\0").ok()?,
                cuGraphicsMapResources: *lib.get(b"cuGraphicsMapResources\0").ok()?,
                cuGraphicsUnmapResources: *lib.get(b"cuGraphicsUnmapResources\0").ok()?,
                cuGraphicsSubResourceGetMappedArray: *lib
                    .get(b"cuGraphicsSubResourceGetMappedArray\0")
                    .ok()?,
                cuGraphicsUnregisterResource: *lib.get(b"cuGraphicsUnregisterResource\0").ok()?,
                cuImportExternalMemory: *lib.get(b"cuImportExternalMemory\0").ok()?,
                cuExternalMemoryGetMappedBuffer: *lib
                    .get(b"cuExternalMemoryGetMappedBuffer\0")
                    .ok()?,
                cuDestroyExternalMemory: *lib.get(b"cuDestroyExternalMemory\0").ok()?,
                cuImportExternalSemaphore: *lib.get(b"cuImportExternalSemaphore\0").ok()?,
                cuDestroyExternalSemaphore: *lib.get(b"cuDestroyExternalSemaphore\0").ok()?,
                cuSignalExternalSemaphoresAsync: *lib
                    .get(b"cuSignalExternalSemaphoresAsync\0")
                    .ok()?,
                cuWaitExternalSemaphoresAsync: *lib.get(b"cuWaitExternalSemaphoresAsync\0").ok()?,
                cuIpcGetMemHandle: *lib.get(b"cuIpcGetMemHandle\0").ok()?,
                // CUDA 11 split the ABI; try `_v2` first, then the unsuffixed name (same signature).
                cuIpcOpenMemHandle: *lib
                    .get(b"cuIpcOpenMemHandle_v2\0")
                    .or_else(|_| lib.get(b"cuIpcOpenMemHandle\0"))
                    .ok()?,
                cuIpcCloseMemHandle: *lib.get(b"cuIpcCloseMemHandle\0").ok()?,
            };
            std::mem::forget(lib); // leak the mapping so the fn pointers stay process-lifetime
            Some(api)
        })
        .as_ref()
}

/// One `unsafe fn` per driver entry point, forwarding through the dlopen'd table.
/// `CU_ERROR_NOT_LOADED` when the driver is absent; only `context()` runs before the table
/// is confirmed, so later wrappers always hit `Some`.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),* $(,)?);)*) => {$(
        pub(crate) unsafe fn $name($($arg: $ty),*) -> CUresult {
            match cuda_api() {
                // SAFETY: the entry of the same name is the live dlopen'd symbol (never
                // unloaded — `mem::forget(lib)`), typed to its cuda.h signature; the caller
                // upholds the driver-API contract, with the proof at each call site.
                Some(a) => unsafe { (a.$name)($($arg),*) },
                None => CU_ERROR_NOT_LOADED,
            }
        }
    )*};
}

forward! {
    cuInit(flags: c_uint);
    cuDeviceGet(device: *mut CUdevice, ordinal: c_int);
    cuDeviceGetUuid(uuid: *mut [u8; 16], dev: CUdevice);
    cuDeviceGetPCIBusId(bus_id: *mut c_char, len: c_int, dev: CUdevice);
    cuCtxCreate_v2(pctx: *mut CUcontext, flags: c_uint, dev: CUdevice);
    cuCtxDestroy_v2(ctx: CUcontext);
    cuCtxSetCurrent(ctx: CUcontext);
    cuMemAllocPitch_v2(
        dptr: *mut CUdeviceptr,
        pitch: *mut usize,
        width_bytes: usize,
        height: usize,
        element_size: c_uint,
    );
    cuMemFree_v2(dptr: CUdeviceptr);
    cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream);
    cuStreamSynchronize(stream: CUstream);
    cuEventCreate(event: *mut CUevent, flags: c_uint);
    cuEventRecord(event: CUevent, stream: CUstream);
    cuEventQuery(event: CUevent);
    cuEventDestroy_v2(event: CUevent);
    cuCtxGetStreamPriorityRange(least: *mut c_int, greatest: *mut c_int);
    cuStreamCreateWithPriority(stream: *mut CUstream, flags: c_uint, priority: c_int);
    cuStreamDestroy_v2(stream: CUstream);
    cuGraphicsGLRegisterImage(
        resource: *mut CUgraphicsResource,
        texture: c_uint,
        target: c_uint,
        flags: c_uint,
    );
    cuGraphicsMapResources(count: c_uint, resources: *mut CUgraphicsResource, stream: *mut c_void);
    cuGraphicsUnmapResources(
        count: c_uint,
        resources: *mut CUgraphicsResource,
        stream: *mut c_void,
    );
    cuGraphicsSubResourceGetMappedArray(
        array: *mut CUarray,
        resource: CUgraphicsResource,
        array_index: c_uint,
        mip_level: c_uint,
    );
    cuGraphicsUnregisterResource(resource: CUgraphicsResource);
    cuImportExternalMemory(
        ext_mem_out: *mut CUexternalMemory,
        mem_handle_desc: *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
    );
    cuExternalMemoryGetMappedBuffer(
        dev_ptr: *mut CUdeviceptr,
        ext_mem: CUexternalMemory,
        buffer_desc: *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
    );
    cuDestroyExternalMemory(ext_mem: CUexternalMemory);
    cuImportExternalSemaphore(
        ext_sem_out: *mut CUexternalSemaphore,
        sem_handle_desc: *const CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC,
    );
    cuDestroyExternalSemaphore(ext_sem: CUexternalSemaphore);
    cuSignalExternalSemaphoresAsync(
        ext_sems: *const CUexternalSemaphore,
        params: *const CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS,
        count: c_uint,
        stream: CUstream,
    );
    cuWaitExternalSemaphoresAsync(
        ext_sems: *const CUexternalSemaphore,
        params: *const CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS,
        count: c_uint,
        stream: CUstream,
    );
    cuIpcGetMemHandle(handle: *mut CUipcMemHandle, dptr: CUdeviceptr);
    cuIpcOpenMemHandle(dptr: *mut CUdeviceptr, handle: CUipcMemHandle, flags: c_uint);
    cuIpcCloseMemHandle(dptr: CUdeviceptr);
}

#[inline]
pub(crate) fn ck(r: CUresult, what: &str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        bail!("CUDA driver error {r} in {what}")
    }
}
