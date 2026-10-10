//! CUDA driver-API facade over `ffi` (`dlopen` of `libcuda.so.1`). The FFI is hand-rolled: no
//! crate exposes the GL-interop calls, and the load is runtime so one binary still runs where
//! `libcuda` is absent (AMD/Intel).
//!
//! Owns the process-wide `CUcontext` (lazy; shared by the EGL importer and NVENC; each thread
//! makes it current), pitched device memory (`BufferPool` / `DeviceBuffer` / IPC / plane copies),
//! and GL / external-memory interop (`RegisteredTexture`, `ExternalDmabuf`).
//!
//! GL interop, not EGL: `cuGraphicsEGLRegisterImage` is Tegra-only on the desktop driver
//! ([`super::egl`]). Cursor blend is the SPIR-V pass in [`super::vkslot`].

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{bail, Context as _, Result};
use std::os::fd::{AsRawFd as _, IntoRawFd as _, OwnedFd};
use std::os::raw::{c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};

#[path = "cuda/ffi.rs"]
mod ffi;
// `pub` (not `pub(crate)`): the raw driver-API vocabulary (`CUdeviceptr`, …) is consumed across
// the crate boundary by the encode backends' CUDA-frame paths.
pub use ffi::*;

/// Packed host readback of a pitched device plane. Synchronous; self-test only, not the hot path.
pub fn read_plane_to_host(
    src_ptr: CUdeviceptr,
    src_pitch: usize,
    width_bytes: usize,
    height: usize,
) -> Result<Vec<u8>> {
    let mut host = vec![0u8; width_bytes * height];
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src_ptr,
        srcPitch: src_pitch,
        dstMemoryType: 1, // CU_MEMORYTYPE_HOST
        dstHost: host.as_mut_ptr() as *mut c_void,
        dstPitch: width_bytes,
        WidthInBytes: width_bytes,
        Height: height,
        ..Default::default()
    };
    // SAFETY: `copy` outlives the synchronous copy. `srcDevice`/`srcPitch` are the caller's pitched
    // plane; `dstHost` is `host` (`width_bytes*height` bytes). Context current is the caller's job.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(dev->host)")? };
    Ok(host)
}

/// Packed host→pitched-device upload. Synchronous: the direct encoder's CPU-frame path, and
/// the benchmarks, where uninitialised device memory comes back zeroed and CBR has nothing
/// to code.
///
/// # Safety
/// The context is current, and `dst_ptr` is a live allocation of `height` rows of `dst_pitch`
/// bytes, each at least `width_bytes`.
pub unsafe fn write_plane_from_host(
    dst_ptr: CUdeviceptr,
    dst_pitch: usize,
    src: &[u8],
    width_bytes: usize,
    height: usize,
) -> Result<()> {
    anyhow::ensure!(
        src.len() >= width_bytes * height,
        "write_plane_from_host: source is {} bytes, need {}",
        src.len(),
        width_bytes * height
    );
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: 1, // CU_MEMORYTYPE_HOST
        srcHost: src.as_ptr() as *const c_void,
        srcPitch: width_bytes,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst_ptr,
        dstPitch: dst_pitch,
        WidthInBytes: width_bytes,
        Height: height,
        ..Default::default()
    };
    // SAFETY: `copy` outlives the synchronous copy. `srcHost` is `src` (≥ `width_bytes*height`);
    // the destination and the current context are this fn's contract. Sync, so `src` need not
    // outlive return.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(host->dev)") }
}

/// Export `ptr` as a 64-byte IPC handle. The allocation must outlive every importer; context current.
pub fn ipc_export(ptr: CUdeviceptr) -> Result<[u8; CU_IPC_HANDLE_SIZE]> {
    let mut handle = CUipcMemHandle {
        reserved: [0; CU_IPC_HANDLE_SIZE],
    };
    // SAFETY: `&mut handle` is a live out-param the driver fills; `ptr` is the caller's live
    // allocation. Synchronous; retains no Rust pointer. Context current.
    unsafe { ck(cuIpcGetMemHandle(&mut handle, ptr), "cuIpcGetMemHandle")? };
    Ok(handle.reserved)
}

/// Map an IPC handle from another process. Valid until [`ipc_close`]. Context current.
pub fn ipc_open(handle: &[u8; CU_IPC_HANDLE_SIZE]) -> Result<CUdeviceptr> {
    let h = CUipcMemHandle { reserved: *handle };
    let mut ptr: CUdeviceptr = 0;
    // SAFETY: `h` is passed by value (`CUipcMemHandle` ABI); `&mut ptr` is a live out-param for the
    // mapped address. Synchronous. Context current.
    unsafe {
        ck(
            cuIpcOpenMemHandle(&mut ptr, h, CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS),
            "cuIpcOpenMemHandle",
        )?
    };
    Ok(ptr)
}

/// Close an [`ipc_open`] mapping. Best-effort; makes the shared context current (Drop may be off-thread).
pub fn ipc_close(ptr: CUdeviceptr) {
    if ptr == 0 {
        return;
    }
    bind_shared_ctx();
    // SAFETY: `ptr` came from `cuIpcOpenMemHandle` and is closed once by the owning cache. Context
    // is set current first: this runs from `Drop` on whichever thread holds the last reference.
    unsafe {
        let _ = cuIpcCloseMemHandle(ptr);
    }
}

/// Process-wide CUDA context. `Send`/`Sync` so it can live in a `OnceLock`; the driver allows
/// `cuCtxSetCurrent` from any thread.
#[derive(Clone, Copy)]
pub struct Context(pub CUcontext);
// SAFETY: `CUcontext` is an opaque driver handle, not a Rust pointer. Created once, never
// destroyed (process lifetime). The only use is `cuCtxSetCurrent`, which the Driver API allows
// from any thread — transferring the handle cannot dangle or race.
unsafe impl Send for Context {}
// SAFETY: the wrapped handle is an immutable opaque address; the driver owns synchronization.
unsafe impl Sync for Context {}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// CUDA device 0, the one every context here runs on. The Vulkan and EGL devices follow it
/// ([`device_uuid`], [`device_pci_bus_id`]) so interop stays on one GPU.
fn device0() -> Result<CUdevice> {
    if cuda_api().is_none() {
        bail!("libcuda.so.1 not available — no NVIDIA driver (CUDA zero-copy disabled)");
    }
    let mut dev: CUdevice = 0;
    // SAFETY: `cuda_api()` is `Some` (checked above), so wrappers hit the live `libcuda` table.
    // `cuInit(0)`: flags 0 is the API-required value, and it is idempotent. `&mut dev` is a
    // live out-param that outlives the synchronous call.
    unsafe {
        ck(cuInit(0), "cuInit")?;
        ck(cuDeviceGet(&mut dev, 0), "cuDeviceGet")?;
    }
    Ok(dev)
}

/// [`device0`]'s UUID, the bytes Vulkan reports as `deviceUUID`.
pub fn device_uuid() -> Result<[u8; 16]> {
    let dev = device0()?;
    let mut uuid = [0u8; 16];
    // SAFETY: `uuid` is a live 16-byte out-param, the size of `CUuuid`.
    unsafe { ck(cuDeviceGetUuid(&mut uuid, dev), "cuDeviceGetUuid")? };
    Ok(uuid)
}

/// [`device0`]'s PCI address, `domain:bus:device.function` as sysfs spells `PCI_SLOT_NAME`.
pub fn device_pci_bus_id() -> Result<String> {
    let dev = device0()?;
    let mut buf = [0u8; 32];
    // SAFETY: `buf` is a live out-param of the length passed; the driver writes a
    // NUL-terminated string within it.
    unsafe {
        ck(
            cuDeviceGetPCIBusId(buf.as_mut_ptr().cast(), buf.len() as i32, dev),
            "cuDeviceGetPCIBusId",
        )?
    };
    let id = std::ffi::CStr::from_bytes_until_nul(&buf).context("PCI bus id without a NUL")?;
    Ok(id.to_string_lossy().into_owned())
}

/// Shared CUDA context on device 0, created once.
pub fn context() -> Result<CUcontext> {
    if let Some(c) = CONTEXT.get() {
        return Ok(c.0);
    }
    let dev = device0()?;
    // SAFETY: `device0` confirmed the live `libcuda` table and a valid device. `&mut ctx` is a
    // live out-param that outlives the synchronous call. `ck` bails unless `ctx` is valid.
    let ctx = unsafe {
        let mut ctx: CUcontext = std::ptr::null_mut();
        ck(
            cuCtxCreate_v2(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev),
            "cuCtxCreate_v2",
        )?;
        ctx
    };
    // Racy first-init: the winner's context is used; a loser leaks one context (process lifetime).
    Ok(CONTEXT.get_or_init(|| Context(ctx)).0)
}

/// Bind the shared context to this thread. Required before any CUDA op here.
pub fn make_current() -> Result<()> {
    let ctx = context()?;
    // SAFETY: `ctx` is the live shared `CUcontext` from `context()?`. `cuCtxSetCurrent` binds it
    // to this thread only; it takes no Rust pointer.
    unsafe { ck(cuCtxSetCurrent(ctx), "cuCtxSetCurrent") }
}

/// Best-effort [`make_current`] for teardown: binds the shared context if one exists. `Drop`
/// paths call it first, since they may run on a thread where it is not current.
fn bind_shared_ctx() {
    if let Some(c) = CONTEXT.get() {
        // SAFETY: `c.0` is the shared context, created once and never destroyed.
        // `cuCtxSetCurrent` binds it to this thread and takes no Rust pointer.
        let _ = unsafe { cuCtxSetCurrent(c.0) };
    }
}

/// Run `probe` on a throwaway device-0 context, then restore the shared one — also when `probe`
/// panics. Diagnostic: splits a bad shared context from a driver-wide failure. Never a hot path.
pub fn with_fresh_context<R>(probe: impl FnOnce(CUcontext) -> R) -> Result<R> {
    /// Destroys the throwaway context and rebinds the shared one on every exit.
    struct Fresh(CUcontext);
    impl Drop for Fresh {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the context `cuCtxCreate_v2` returned below, owned by this
            // guard alone and destroyed once, here.
            let _ = unsafe { cuCtxDestroy_v2(self.0) };
            bind_shared_ctx();
        }
    }
    let dev = device0()?;
    // SAFETY: `device0` confirmed the driver table and a valid device. `&mut ctx` is a live
    // out-param. `ck` bails unless `ctx` is a valid context, which `Fresh` then owns.
    let fresh = unsafe {
        let mut ctx: CUcontext = std::ptr::null_mut();
        ck(
            cuCtxCreate_v2(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev),
            "cuCtxCreate_v2 (diagnostic)",
        )?;
        Fresh(ctx)
    };
    Ok(probe(fresh.0))
}

thread_local! {
    /// Per-thread copy stream. `None` until first use; `Some(null)` = creation failed, use the
    /// NULL stream. Per-thread so `cuStreamSynchronize` waits only this worker's copies.
    static COPY_STREAM: std::cell::Cell<Option<CUstream>> = const { std::cell::Cell::new(None) };
}

/// This thread's highest-priority copy stream (lazy; context must be current). `greatest` from
/// `cuCtxGetStreamPriorityRange` is the numerically lowest value. Intra-process hint only; the
/// Linux driver may ignore it. Falls back to the NULL stream.
fn copy_stream() -> CUstream {
    COPY_STREAM.with(|cell| {
        if let Some(s) = cell.get() {
            return s;
        }
        // SAFETY: context is current (doc contract). `&mut least`/`&mut greatest`/`&mut s` are live
        // out-params that outlive their synchronous calls. Non-zero result → null stream; never
        // read an uninitialized handle.
        let stream = unsafe {
            let (mut least, mut greatest) = (0i32, 0i32);
            if cuCtxGetStreamPriorityRange(&mut least, &mut greatest) != 0 {
                std::ptr::null_mut()
            } else {
                let mut s: CUstream = std::ptr::null_mut();
                if cuStreamCreateWithPriority(&mut s, CU_STREAM_NON_BLOCKING, greatest) != 0 {
                    std::ptr::null_mut()
                } else {
                    tracing::debug!(
                        priority = greatest,
                        "CUDA high-priority copy stream created"
                    );
                    s
                }
            }
        };
        cell.set(Some(stream));
        stream
    })
}

/// Enqueue `copy` on this thread's priority stream and wait. The source is safe to recycle once
/// this returns; the wait is this stream only.
unsafe fn copy_blocking(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory. `&copy` outlives
    // the synchronous call.
    unsafe {
        let stream = copy_stream();
        ck(cuMemcpy2DAsync_v2(copy, stream), what)?;
        ck(cuStreamSynchronize(stream), "cuStreamSynchronize")
    }
}

/// Enqueue `copy` with no CPU wait. Stream-ordered consumers only: `src` must stay valid until
/// downstream stream work (the encode) finishes.
unsafe fn copy_async(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory that stays valid
    // until the stream work completes.
    unsafe { ck(cuMemcpy2DAsync_v2(copy, copy_stream()), what) }
}

/// Wait for this thread's copy stream. One sync after the last enqueue covers every plane (FIFO).
/// Context must be current.
unsafe fn sync_copy_stream() -> Result<()> {
    // SAFETY: caller: context current. Stream sync touches no Rust memory.
    unsafe { ck(cuStreamSynchronize(copy_stream()), "cuStreamSynchronize") }
}

/// CPU wait for this thread's copy stream, a semaphore wait enqueued ahead included. Context
/// must be current.
pub fn copy_stream_sync() -> Result<()> {
    // SAFETY: context current (doc contract); a stream sync touches no Rust memory.
    unsafe { sync_copy_stream() }
}

/// [`copy_stream_sync`] with a ceiling. `cuStreamSynchronize` has no timeout, and a wait on an
/// external semaphore whose signaller died can never be satisfied — polling an event instead
/// costs the encode thread a bounded stall rather than the whole session.
pub fn copy_stream_sync_deadline(budget: std::time::Duration) -> Result<()> {
    let mut event: CUevent = std::ptr::null_mut();
    // SAFETY: context current (doc contract). `&mut event` is a live out-param; the event is
    // destroyed on every path below, and `cuEventRecord`/`cuEventQuery` take it by value.
    unsafe {
        ck(cuEventCreate(&mut event, 0), "cuEventCreate")?;
        if let Err(e) = ck(cuEventRecord(event, copy_stream()), "cuEventRecord") {
            cuEventDestroy_v2(event);
            return Err(e);
        }
        let deadline = std::time::Instant::now() + budget;
        loop {
            let r = cuEventQuery(event);
            if r == 0 {
                cuEventDestroy_v2(event);
                return Ok(());
            }
            if r != CUDA_ERROR_NOT_READY {
                cuEventDestroy_v2(event);
                return ck(r, "cuEventQuery");
            }
            if std::time::Instant::now() >= deadline {
                cuEventDestroy_v2(event);
                bail!(
                    "the copy stream did not drain within {:?} — the fused pass never signalled \
                     (a dead convert worker leaves its timeline value unreachable)",
                    budget
                );
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
    }
}

/// `sync: false` carries `copy_async`'s source-lifetime contract.
unsafe fn copy_issue(copy: &CUDA_MEMCPY2D, what: &str, sync: bool) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory.
    unsafe {
        if sync {
            copy_blocking(copy, what)
        } else {
            copy_async(copy, what)
        }
    }
}

/// This thread's copy stream as a raw handle, for `NvEncSetIOCudaStreams`. Null means ordering
/// is unavailable — keep blocking copies. Context must be current.
pub fn copy_stream_handle() -> *mut c_void {
    copy_stream() // CUstream is *mut c_void (opaque CUstream_st*)
}

/// Max cursor-overlay bitmap edge (px) uploaded to the device blend buffer — matches the Vulkan path.
pub const CURSOR_MAX: u32 = 256;

/// How a pitched surface holds its planes. Every allocator and copy sizes planes from
/// [`planes`](Self::planes), so an allocation always covers what a copy writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneLayout {
    /// One plane of 4-byte pixels (BGRx, 2:10:10:10).
    Packed32,
    /// 8-bit 4:2:0: luma, then interleaved U,V pairs at ⌈W/2⌉ × ⌈H/2⌉.
    Nv12,
    /// Planar 4:4:4: three full-res 1-byte planes.
    Yuv444,
}

impl PlaneLayout {
    /// `(row_bytes, rows)` of each plane for `width`×`height` luma; absent planes are `(0, 0)`.
    /// Chroma rounds up, so an odd edge keeps its last chroma sample.
    pub const fn planes(self, width: u32, height: u32) -> [(usize, usize); 3] {
        let (w, h) = (width as usize, height as usize);
        match self {
            PlaneLayout::Packed32 => [(w * 4, h), (0, 0), (0, 0)],
            PlaneLayout::Nv12 => [(w, h), (w.div_ceil(2) * 2, h.div_ceil(2)), (0, 0)],
            PlaneLayout::Yuv444 => [(w, h), (w, h), (w, h)],
        }
    }

    /// `(row_bytes, rows)` of one allocation holding every plane under one pitch.
    pub fn stacked(self, width: u32, height: u32) -> (usize, usize) {
        self.planes(width, height)
            .iter()
            .fold((0, 0), |(bytes, rows), &(b, r)| (bytes.max(b), rows + r))
    }
}

/// One pitched allocation of `rows` rows of `row_bytes`. Returns `(ptr, pitch)`.
fn alloc_rows(row_bytes: usize, rows: usize, what: &str) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: `&mut ptr`/`&mut pitch` are live out-params that outlive the synchronous alloc.
    // Row bytes, rows, and element size are by value.
    unsafe {
        ck(
            cuMemAllocPitch_v2(&mut ptr, &mut pitch, row_bytes, rows, 16),
            what,
        )?;
    }
    Ok((ptr, pitch))
}

/// A pitched plane: `(ptr, pitch)`.
pub type PlaneSpan = (CUdeviceptr, usize);

/// One buffer in `layout`: `(ptr, pitch)`, plus NV12's chroma as its own allocation.
/// YUV444 stacks Y, U and V in the one allocation, so the wire carries it like one plane.
fn alloc_planes(
    layout: PlaneLayout,
    width: u32,
    height: u32,
) -> Result<(PlaneSpan, Option<PlaneSpan>)> {
    if layout != PlaneLayout::Nv12 {
        let (row_bytes, rows) = layout.stacked(width, height);
        return Ok((alloc_rows(row_bytes, rows, "cuMemAllocPitch_v2")?, None));
    }
    let [(y_bytes, y_rows), (uv_bytes, uv_rows), _] = layout.planes(width, height);
    let y = alloc_rows(y_bytes, y_rows, "cuMemAllocPitch_v2(Y)")?;
    match alloc_rows(uv_bytes, uv_rows, "cuMemAllocPitch_v2(UV)") {
        Ok(uv) => Ok((y, Some(uv))),
        Err(e) => {
            // SAFETY: `y.0` is the allocation just made and owned by nobody else. A leak here
            // would be a per-frame `BufferPool::get` miss.
            unsafe {
                let _ = cuMemFree_v2(y.0);
            }
            Err(e)
        }
    }
}

/// Encoder-owned contiguous pitched CUDA surface. Registered once as
/// `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR` (`encode/linux/nvenc_cuda.rs`). Layout matches
/// NVENC's single-pointer register: NV12 = Y then UV, YUV444 = Y|U|V stacked, RGB = packed 4-byte.
/// Never pooled or sent on the wire. Frees on drop (context made current; drop may be off-thread).
pub struct InputSurface {
    /// NVENC register pointer. NV12 chroma at `ptr + pitch*height`; YUV444 U/V at `1*`/`2*`.
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    /// Luma rows — the plane-stride multiplier for NVENC and the copy helpers.
    pub height: u32,
}

impl InputSurface {
    /// Every plane of `layout` under one pitch: NV12 chroma at `ptr + pitch*height`, YUV444
    /// U/V at `1*`/`2*`. Packed RGB/BGRx is what NVENC CSCs as `ABGR`/`ARGB`.
    pub fn alloc(layout: PlaneLayout, width: u32, height: u32) -> Result<InputSurface> {
        let (row_bytes, rows) = layout.stacked(width, height);
        let (ptr, pitch) = alloc_rows(row_bytes, rows, "cuMemAllocPitch_v2(input surface)")?;
        Ok(InputSurface { ptr, pitch, height })
    }
}

impl Drop for InputSurface {
    fn drop(&mut self) {
        if self.ptr == 0 {
            return;
        }
        bind_shared_ctx();
        // SAFETY: this surface exclusively owns `self.ptr`, freed once (`ptr == 0` skips empty).
        // Context is set current first: drop may run on a thread where it isn't.
        unsafe {
            let _ = cuMemFree_v2(self.ptr);
        }
    }
}

/// Free-list of recycled device allocations for one resolution. Shared (`Arc`) between capture
/// (hands out) and encode (`DeviceBuffer` drop returns here). NV12: Y and UV stay paired.
struct PoolInner {
    free: Vec<CUdeviceptr>,
    /// NV12: UV plane paired with each Y in `free` (same index, same length).
    free_uv: Vec<CUdeviceptr>,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        bind_shared_ctx();
        // SAFETY: drops only after every `DeviceBuffer` `Arc` is gone, so `free`/`free_uv` hold
        // each allocation once and nothing still uses them. Context is set current first: drop
        // may run off the allocating thread. Each `p` came from `cuMemAllocPitch_v2`.
        unsafe {
            for &p in &self.free {
                let _ = cuMemFree_v2(p);
            }
            for &p in &self.free_uv {
                let _ = cuMemFree_v2(p);
            }
        }
    }
}

/// Reusable pitched device buffers at a fixed resolution. Avoids per-frame `cuMemAllocPitch` /
/// `cuMemFree`, which take the device allocator lock.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Mutex<PoolInner>>,
    width: u32,
    height: u32,
    layout: PlaneLayout,
    pitch: usize,
    /// NV12 chroma pitch; unused by the other layouts.
    uv_pitch: usize,
}

impl BufferPool {
    /// Pool of `width`×`height` buffers in `layout`. Allocates one to learn the driver's pitches.
    pub fn new(layout: PlaneLayout, width: u32, height: u32) -> Result<BufferPool> {
        let ((ptr, pitch), uv) = alloc_planes(layout, width, height)?;
        Ok(BufferPool {
            inner: Arc::new(Mutex::new(PoolInner {
                free: vec![ptr],
                free_uv: uv.map(|(p, _)| p).into_iter().collect(),
            })),
            width,
            height,
            layout,
            pitch,
            uv_pitch: uv.map_or(0, |(_, pitch)| pitch),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Recycled if free, else freshly allocated. Returns to this pool on drop (consumer must have
    /// synced). NV12: Y and its paired UV.
    pub fn get(&self) -> Result<DeviceBuffer> {
        let reuse = {
            let mut g = self.inner.lock().unwrap();
            g.free.pop().map(|y| (y, g.free_uv.pop()))
        };
        // Pushed and popped together, so a recycled NV12 Y always has its UV.
        let (ptr, uv) = match reuse {
            Some(pair) => pair,
            None => {
                let ((ptr, _), uv) = alloc_planes(self.layout, self.width, self.height)?;
                (ptr, uv.map(|(uv, _)| uv))
            }
        };
        Ok(DeviceBuffer {
            ptr,
            pitch: self.pitch,
            width: self.width,
            height: self.height,
            planes: Planes::new(self.layout, uv.map(|uv| (uv, self.uv_pitch))),
            pool: Some(self.inner.clone()),
            remote_release: None,
        })
    }
}

/// Where a [`DeviceBuffer`]'s chroma lives: one field, so no layout disagrees with its planes.
#[derive(Clone, Copy)]
enum Planes {
    Packed32,
    /// NV12 chroma `(ptr, pitch)`, its own allocation.
    Nv12(CUdeviceptr, usize),
    /// U and V stacked under Y in the one allocation.
    Yuv444,
}

impl Planes {
    /// `uv` is the chroma allocation, which only NV12 has.
    fn new(layout: PlaneLayout, uv: Option<(CUdeviceptr, usize)>) -> Planes {
        match (layout, uv) {
            (PlaneLayout::Yuv444, _) => Planes::Yuv444,
            (_, Some((uv, pitch))) => Planes::Nv12(uv, pitch),
            (_, None) => Planes::Packed32,
        }
    }
}

/// Pitched device buffer for one captured frame. Filled from the EGL-mapped dmabuf so the dmabuf
/// can return to the compositor immediately. Pooled buffers recycle on drop; others free.
pub struct DeviceBuffer {
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    planes: Planes,
    pool: Option<Arc<Mutex<PoolInner>>>,
    /// IPC import: drop runs this once (owner recycles). Must not free or pool-recycle locally.
    remote_release: Option<Box<dyn FnOnce() + Send>>,
}

impl DeviceBuffer {
    /// Standalone buffer in `layout`. Prefer [`BufferPool`] on the hot path.
    pub fn alloc(layout: PlaneLayout, width: u32, height: u32) -> Result<DeviceBuffer> {
        let ((ptr, pitch), uv) = alloc_planes(layout, width, height)?;
        Ok(DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
            planes: Planes::new(layout, uv),
            pool: None,
            remote_release: None,
        })
    }

    pub fn layout(&self) -> PlaneLayout {
        match self.planes {
            Planes::Packed32 => PlaneLayout::Packed32,
            Planes::Nv12(..) => PlaneLayout::Nv12,
            Planes::Yuv444 => PlaneLayout::Yuv444,
        }
    }

    /// NV12 chroma `(ptr, pitch)`; `None` for every other layout.
    pub fn uv(&self) -> Option<(CUdeviceptr, usize)> {
        match self.planes {
            Planes::Nv12(uv, pitch) => Some((uv, pitch)),
            _ => None,
        }
    }

    pub fn is_nv12(&self) -> bool {
        self.layout() == PlaneLayout::Nv12
    }

    /// Each plane as `((ptr, pitch), (row_bytes, rows))`, in layout order: NV12 chroma from its
    /// own allocation, stacked planes one after another under [`ptr`](Self::ptr).
    pub fn plane_spans(&self) -> impl Iterator<Item = (PlaneSpan, (usize, usize))> + '_ {
        let mut stacked = self.ptr;
        let planes = self.layout().planes(self.width, self.height);
        planes
            .into_iter()
            .enumerate()
            .filter(|&(_, (_, rows))| rows > 0)
            .map(move |(i, (row_bytes, rows))| {
                let at = match (i, self.uv()) {
                    (1, Some(uv)) => uv,
                    _ => (stacked, self.pitch),
                };
                stacked += (self.pitch * rows) as CUdeviceptr;
                (at, (row_bytes, rows))
            })
    }

    /// [`ptr`](Self::ptr) holds 3·[`height`](Self::height) stacked Y, U, V rows.
    pub fn is_yuv444(&self) -> bool {
        self.layout() == PlaneLayout::Yuv444
    }

    // unsafe-fn-no-op-ok: every copy helper trusts `ptr`/`pitch`/`uv` as a live mapping.
    /// Wrap planes owned by another process ([`ipc_open`]). `release` runs once on drop; nothing
    /// is freed or pooled here (the IPC cache closes the mapping after the last remote buffer).
    /// `uv` makes it NV12; otherwise `yuv444` marks stacked 3-plane YUV444 — the wire carries
    /// no format (`ImportKind::Tiled444`).
    ///
    /// # Safety
    /// `ptr` (and `uv`'s pointer, when set) must address a live device mapping with the layout
    /// `pitch`/`width`/`height`/`uv`/`yuv444` describe, and stay mapped until `release` runs.
    pub unsafe fn remote(
        ptr: CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        uv: Option<(CUdeviceptr, usize)>,
        yuv444: bool,
        release: Box<dyn FnOnce() + Send>,
    ) -> DeviceBuffer {
        DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
            planes: match (uv, yuv444) {
                (Some((uv, uv_pitch)), _) => Planes::Nv12(uv, uv_pitch),
                (None, true) => Planes::Yuv444,
                (None, false) => Planes::Packed32,
            },
            pool: None,
            remote_release: Some(release),
        }
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if let Some(release) = self.remote_release.take() {
            release();
            return;
        }
        if self.ptr == 0 {
            return;
        }
        if let Some(pool) = &self.pool {
            // Consumer synced before drop. Y and UV go back together so `get` can pop them as a unit.
            let mut g = pool.lock().unwrap();
            g.free.push(self.ptr);
            if let Some((uv_ptr, _)) = self.uv() {
                g.free_uv.push(uv_ptr);
            }
        } else {
            bind_shared_ctx();
            // SAFETY: un-pooled: this buffer exclusively owns `self.ptr` and its chroma, each from
            // `cuMemAllocPitch_v2`, freed once (`ptr == 0` skipped above). Context is set current
            // first: drop may run on the encode thread, where it isn't.
            unsafe {
                let _ = cuMemFree_v2(self.ptr);
                if let Some((uv_ptr, _)) = self.uv() {
                    let _ = cuMemFree_v2(uv_ptr);
                }
            }
        }
    }
}

/// Persistent GL-texture→CUDA registration. Desktop NVIDIA CUDA interop is GL textures, not
/// dmabuf EGLImages: the importer renders the dmabuf into a reusable `GL_RGBA8` texture, registers
/// once, then each frame maps → copies → unmaps (the map/unmap pair is the GL↔CUDA sync).
pub struct RegisteredTexture {
    resource: CUgraphicsResource,
}

impl RegisteredTexture {
    /// # Safety
    /// The GL context and the shared CUDA context must both be current on this thread, and
    /// `texture` must be a valid `GL_TEXTURE_2D`.
    pub unsafe fn register_gl(texture: u32) -> Result<RegisteredTexture> {
        // SAFETY: caller: GL context owning `texture` and the shared CUDA context are current;
        // `texture` is a live `GL_TEXTURE_2D`. Out-param is a live stack local.
        unsafe {
            const GL_TEXTURE_2D: c_uint = 0x0DE1;
            const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: c_uint = 0x01;
            let mut resource: CUgraphicsResource = std::ptr::null_mut();
            ck(
                cuGraphicsGLRegisterImage(
                    &mut resource,
                    texture,
                    GL_TEXTURE_2D,
                    CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
                ),
                "cuGraphicsGLRegisterImage",
            )?;
            Ok(RegisteredTexture { resource })
        }
    }

    /// Map and copy into `(dst_ptr, dst_pitch)` for `width_bytes`×`height`, one plane of
    /// [`PlaneLayout::planes`]. Syncs before unmap; always unmaps, even on copy error.
    fn copy_mapped_plane(
        &mut self,
        dst_ptr: CUdeviceptr,
        dst_pitch: usize,
        width_bytes: usize,
        height: usize,
    ) -> Result<()> {
        // SAFETY: `self.resource` is from `register_gl`; the caller holds GL+CUDA current. Map,
        // copy and unmap all use `copy_stream()`: map orders prior GL work only before CUDA work
        // issued *in that stream*, and `copy_stream` is `CU_STREAM_NON_BLOCKING`, so a NULL-stream
        // map would let the copy race the GL de-tile. `array` is mip 0; unmap on GetMappedArray
        // failure. `copy` outlives `copy_blocking`; `srcArray` is valid while mapped; the dest
        // plane is live and `width_bytes`×`height` fit it. Always unmap after the copy.
        unsafe {
            ck(
                cuGraphicsMapResources(1, &mut self.resource, copy_stream()),
                "cuGraphicsMapResources",
            )?;
            let mut array: CUarray = std::ptr::null_mut();
            if cuGraphicsSubResourceGetMappedArray(&mut array, self.resource, 0, 0) != 0 {
                let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
                bail!("cuGraphicsSubResourceGetMappedArray failed");
            }
            let copy = CUDA_MEMCPY2D {
                srcMemoryType: CU_MEMORYTYPE_ARRAY,
                srcArray: array,
                dstMemoryType: CU_MEMORYTYPE_DEVICE,
                dstDevice: dst_ptr,
                dstPitch: dst_pitch,
                WidthInBytes: width_bytes,
                Height: height,
                ..Default::default()
            };
            let res = copy_blocking(&copy, "cuMemcpy2DAsync_v2(plane)");
            let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
            res
        }
    }
}

/// Copy each registered texture into the matching plane of `dst`, in layout order. Each copy
/// syncs before return.
pub fn copy_mapped_planes<'a>(
    textures: impl IntoIterator<Item = &'a mut RegisteredTexture>,
    dst: &DeviceBuffer,
) -> Result<()> {
    for (tex, ((ptr, pitch), (row_bytes, rows))) in textures.into_iter().zip(dst.plane_spans()) {
        tex.copy_mapped_plane(ptr, pitch, row_bytes, rows)?;
    }
    Ok(())
}

/// Device→device copy of one pitched surface into another of the same layout: `rows` rows of
/// `pitch` bytes. A repeat frame's slot is cloned this way instead of being converted again.
/// `sync: false` enqueues with no CPU wait.
///
/// # Safety
/// The context is current, both surfaces are live allocations of `rows` rows of `pitch` bytes,
/// and with `sync: false` both stay valid until downstream stream work completes.
pub unsafe fn copy_surface_to_surface(
    src_ptr: CUdeviceptr,
    dst_ptr: CUdeviceptr,
    pitch: usize,
    rows: usize,
    sync: bool,
) -> Result<()> {
    let copy = device_copy((src_ptr, pitch), (dst_ptr, pitch), pitch, rows);
    // SAFETY: this fn's contract: context current, both surfaces hold `pitch × rows` bytes and
    // outlive the copy. `copy` outlives the enqueue.
    unsafe { copy_issue(&copy, "cuMemcpy2DAsync_v2(slot->slot)", sync) }
}

/// Device→device copy of a 4-byte (BGRx) [`DeviceBuffer`] into `dst_ptr`. `sync: false`
/// enqueues with no CPU wait.
///
/// # Safety
/// The context is current, `src` describes a live allocation, `dst_ptr` is a live allocation of
/// `src.height` rows of `dst_pitch` ≥ `src.width * 4` bytes, and with `sync: false` `src` stays
/// valid until downstream stream work completes.
pub unsafe fn copy_device_to_device(
    src: &DeviceBuffer,
    dst_ptr: CUdeviceptr,
    dst_pitch: usize,
    sync: bool,
) -> Result<()> {
    let [(row_bytes, rows), ..] = PlaneLayout::Packed32.planes(src.width, src.height);
    let copy = device_copy((src.ptr, src.pitch), (dst_ptr, dst_pitch), row_bytes, rows);
    // SAFETY: this fn's contract: context current, `src` and `dst` live, `width*4`×`height` fit
    // both, and the source outlives an unsynced copy. `copy` outlives the enqueue.
    unsafe { copy_issue(&copy, "cuMemcpy2DAsync_v2(dev->dev)", sync) }
}

/// A device→device 2D copy of `rows` rows of `row_bytes`, each side `(ptr, pitch)`.
fn device_copy(
    src: (CUdeviceptr, usize),
    dst: (CUdeviceptr, usize),
    row_bytes: usize,
    rows: usize,
) -> CUDA_MEMCPY2D {
    CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src.0,
        srcPitch: src.1,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst.0,
        dstPitch: dst.1,
        WidthInBytes: row_bytes,
        Height: rows,
        ..Default::default()
    }
}

/// Copy a planar `src` into NVENC's planes (`data[0..]`), one `(ptr, pitch)` per plane of its
/// layout. `sync: false` enqueues with no CPU wait.
///
/// # Safety
/// The context is current, `src` describes a live allocation, each `dsts` plane is live with
/// the rows its layout's plane table gives it at that pitch, and with `sync: false` `src` stays
/// valid until downstream stream work completes.
unsafe fn copy_planes_to_device(
    src: &DeviceBuffer,
    dsts: &[(CUdeviceptr, usize)],
    sync: bool,
) -> Result<()> {
    for (&dst, (from, (row_bytes, rows))) in dsts.iter().zip(src.plane_spans()) {
        let copy = device_copy(from, dst, row_bytes, rows);
        // SAFETY: this fn's contract covers the context and `dst`; `from` is a plane of the live
        // `src` (its layout's plane table). `copy` outlives the enqueue. A failed enqueue drains:
        // the caller recycles `src` on `Err`, so a copy in flight would race the next frame.
        unsafe {
            if let Err(e) = copy_async(&copy, "cuMemcpy2DAsync_v2(plane dev->dev)") {
                let _ = sync_copy_stream();
                return Err(e);
            }
        }
    }
    if sync {
        // SAFETY: one stream sync after the last enqueue covers every plane (FIFO); the context
        // is current per this fn's contract.
        unsafe { sync_copy_stream()? };
    }
    Ok(())
}

/// Copy imported NV12 into NVENC's two-plane surface (`data[0]`/`data[1]`). `sync: false`
/// enqueues with no CPU wait.
///
/// # Safety
/// The context is current, `src` describes a live allocation, `y_dst`/`uv_dst` are live planes
/// at `y_pitch`/`uv_pitch` holding [`PlaneLayout::Nv12`]'s rows for `src`, and with
/// `sync: false` `src` stays valid until downstream stream work completes.
pub unsafe fn copy_nv12_to_device(
    src: &DeviceBuffer,
    y_dst: CUdeviceptr,
    y_pitch: usize,
    uv_dst: CUdeviceptr,
    uv_pitch: usize,
    sync: bool,
) -> Result<()> {
    anyhow::ensure!(src.is_nv12(), "copy_nv12_to_device on a non-NV12 buffer");
    // SAFETY: this fn's contract is `copy_planes_to_device`'s for NV12's two planes.
    unsafe { copy_planes_to_device(src, &[(y_dst, y_pitch), (uv_dst, uv_pitch)], sync) }
}

/// Copy stacked YUV444 into NVENC's three-plane surface (`data[0..3]`). `sync: false` enqueues
/// with no CPU wait.
///
/// # Safety
/// The context is current, `src` describes a live stacked allocation, each `dsts` plane is live
/// with `src.height` rows of its pitch ≥ `src.width`, and with `sync: false` `src` stays valid
/// until downstream stream work completes.
pub unsafe fn copy_yuv444_to_device(
    src: &DeviceBuffer,
    dsts: [(CUdeviceptr, usize); 3],
    sync: bool,
) -> Result<()> {
    anyhow::ensure!(
        src.is_yuv444(),
        "copy_yuv444_to_device on a non-YUV444 buffer"
    );
    // SAFETY: this fn's contract is `copy_planes_to_device`'s for YUV444's three planes.
    unsafe { copy_planes_to_device(src, &dsts, sync) }
}

impl RegisteredTexture {
    /// Unregister now (idempotent; later `Drop` no-ops). Call before deleting the GL texture:
    /// a still-registered texture leaves the driver holding a registration onto freed GL state.
    pub fn release(&mut self) {
        if self.resource.is_null() {
            return;
        }
        bind_shared_ctx();
        // SAFETY: `self.resource` is the exclusive `CUgraphicsResource` from `register_gl`;
        // nulling it after unregister makes Drop a no-op. Context is set current first: teardown
        // may run on a thread where it isn't.
        unsafe {
            let _ = cuGraphicsUnregisterResource(self.resource);
        }
        self.resource = std::ptr::null_mut();
    }
}

impl Drop for RegisteredTexture {
    fn drop(&mut self) {
        self.release();
    }
}

/// Dmabuf fd imported as CUDA external memory and mapped to a device pointer. LINEAR path
/// (gamescope): bytes are directly addressable, no GL de-tiling. Cached per PipeWire buffer.
pub struct ExternalDmabuf {
    ext: CUexternalMemory,
    pub ptr: CUdeviceptr,
    pub size: u64,
}

// SAFETY: opaque driver handles, uniquely owned (no `Clone`), destroyed once in `Drop`. Moved
// between threads with the importer; `Send` not `Sync` matches single-thread use.
unsafe impl Send for ExternalDmabuf {}

impl ExternalDmabuf {
    /// Import an `OPAQUE_FD` (Vulkan-exported) as `size` bytes of mapped device memory. The
    /// driver owns `fd` on success; a failed import drops (closes) it. Context must be current.
    pub fn import_owned_fd(fd: OwnedFd, size: u64) -> Result<ExternalDmabuf> {
        let mut desc = CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
            type_: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            size,
            ..Default::default()
        };
        desc.handle[0] = fd.as_raw_fd() as u32 as u64; // union member `int fd` (LE low bytes)
        let mut ext: CUexternalMemory = std::ptr::null_mut();
        // SAFETY: `&desc` outlives the call (`OPAQUE_FD`, fd in union `int fd` low bytes, `size`
        // set). `&mut ext` is a live out-param. Driver takes the fd only on success. Context current.
        let r = unsafe { cuImportExternalMemory(&mut ext, &desc) };
        if r != 0 {
            bail!("cuImportExternalMemory failed ({r}) — LINEAR dmabuf import unsupported?");
        }
        let _ = fd.into_raw_fd(); // the driver owns it now
        let buf = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
            offset: 0,
            size,
            ..Default::default()
        };
        let mut ptr: CUdeviceptr = 0;
        // SAFETY: `ext` is the just-imported handle. `&buf` (offset 0, full `size`) outlives the
        // call. `&mut ptr` is a live out-param. Context current.
        let r = unsafe { cuExternalMemoryGetMappedBuffer(&mut ptr, ext, &buf) };
        if r != 0 {
            // SAFETY: mapping failed; we exclusively own `ext`. Destroy once here; success moves it
            // into `ExternalDmabuf`, whose `Drop` destroys it.
            unsafe {
                let _ = cuDestroyExternalMemory(ext);
            }
            bail!("cuExternalMemoryGetMappedBuffer failed ({r})");
        }
        Ok(ExternalDmabuf { ext, ptr, size })
    }
}

impl Drop for ExternalDmabuf {
    fn drop(&mut self) {
        bind_shared_ctx();
        // SAFETY: exclusive owner of `self.ptr` and `self.ext`, torn down once (`!= 0` / `!null`).
        // Context is set current first: drop may run off the import thread. Free the mapped buffer
        // before destroying its backing external memory.
        unsafe {
            if self.ptr != 0 {
                let _ = cuMemFree_v2(self.ptr); // mapped buffers free like device memory
            }
            if !self.ext.is_null() {
                let _ = cuDestroyExternalMemory(self.ext);
            }
        }
    }
}

/// Vulkan timeline semaphore imported as a CUDA external semaphore. CUDA [`signal`]s a value on
/// this thread's copy stream after the input copy; Vulkan blend waits then advances it; CUDA
/// [`wait`]s that value before encode. One handle per [`VkSlotBlend`](super::vkslot::VkSlotBlend);
/// values monotonic for its life.
pub struct ExternalSemaphore {
    sem: CUexternalSemaphore,
}

// SAFETY: opaque driver handle, uniquely owned, destroyed once in `Drop`. Moved with `VkSlotBlend`;
// `Send` not `Sync` matches single-thread-at-a-time use.
unsafe impl Send for ExternalSemaphore {}

impl ExternalSemaphore {
    /// Import a Vulkan timeline semaphore (`vkGetSemaphoreFdKHR` OPAQUE_FD). The driver owns
    /// `fd` on success; a failed import drops (closes) it. Context must be current.
    pub fn import_timeline_fd(fd: OwnedFd) -> Result<ExternalSemaphore> {
        let mut desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
            type_: CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD,
            ..Default::default()
        };
        desc.handle[0] = fd.as_raw_fd() as u32 as u64; // union member `int fd` (LE low bytes)
        let mut sem: CUexternalSemaphore = std::ptr::null_mut();
        // SAFETY: `&desc` outlives the call (`TIMELINE_SEMAPHORE_FD`, fd in union `int fd` low
        // bytes). `&mut sem` is a live out-param. Context current.
        let r = unsafe { cuImportExternalSemaphore(&mut sem, &desc) };
        if r != 0 {
            bail!("cuImportExternalSemaphore failed ({r}) — timeline-semaphore fd export/import unsupported?");
        }
        let _ = fd.into_raw_fd(); // the driver owns it now
        Ok(ExternalSemaphore { sem })
    }

    /// Enqueue a signal to `value` after prior work on this thread's copy stream. No CPU wait.
    pub fn signal(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: `self.sem` is the live imported handle (destroyed only in `Drop`). `&self.sem`/
        // `&params` outlive the enqueue; driver retains no Rust pointer. This thread's copy stream.
        unsafe {
            ck(
                cuSignalExternalSemaphoresAsync(&self.sem, &params, 1, copy_stream()),
                "cuSignalExternalSemaphoresAsync",
            )
        }
    }

    /// Signal `value` on a throwaway stream, outside every copy stream. Only for a wait whose
    /// signaller is gone: it releases a copy stream queued behind that wait. No CPU wait.
    pub fn signal_detached(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
            value,
            ..Default::default()
        };
        let mut stream: CUstream = std::ptr::null_mut();
        // SAFETY: context current (as `signal`). `&mut stream` is a live out-param; the stream
        // is non-blocking, so the signal does not queue behind the legacy NULL stream. A stream
        // destroyed with work pending is released once that work completes.
        unsafe {
            ck(
                cuStreamCreateWithPriority(&mut stream, CU_STREAM_NON_BLOCKING, 0),
                "cuStreamCreateWithPriority",
            )?;
            let r = ck(
                cuSignalExternalSemaphoresAsync(&self.sem, &params, 1, stream),
                "cuSignalExternalSemaphoresAsync",
            );
            cuStreamDestroy_v2(stream);
            r
        }
    }

    /// Enqueue a wait: later work on this thread's copy stream runs only once the timeline
    /// reaches `value`. No CPU wait.
    pub fn wait(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: same as `signal` — live handle, live locals across the enqueue, this thread's
        // copy stream.
        unsafe {
            ck(
                cuWaitExternalSemaphoresAsync(&self.sem, &params, 1, copy_stream()),
                "cuWaitExternalSemaphoresAsync",
            )
        }
    }
}

impl Drop for ExternalSemaphore {
    fn drop(&mut self) {
        bind_shared_ctx();
        // SAFETY: exclusive owner, destroyed once. Context is set current first: drop may run off
        // the import thread (`VkSlotBlend` quiesces the GPU first, so no in-flight signal/wait).
        unsafe {
            let _ = cuDestroyExternalSemaphore(self.sem);
        }
    }
}

/// Copy a pitched span at `src_ptr` (e.g. an [`ExternalDmabuf`] mapping) into `dst`. Context
/// must be current.
///
/// # Safety
/// `src_ptr` must address live device memory holding `dst.height` rows of `src_pitch` bytes
/// (the last row needs only `dst.width * 4`).
pub unsafe fn copy_pitched_to_buffer(
    src_ptr: CUdeviceptr,
    src_pitch: usize,
    dst: &DeviceBuffer,
) -> Result<()> {
    let [(row_bytes, rows), ..] = PlaneLayout::Packed32.planes(dst.width, dst.height);
    let copy = device_copy((src_ptr, src_pitch), (dst.ptr, dst.pitch), row_bytes, rows);
    // SAFETY: the source span is live and large enough (this fn's contract); `dst` is a live
    // buffer of `width*4`×`height`. `copy` outlives the synchronous call, which completes before
    // the dmabuf is requeued.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(ext->dev)") }
}

/// De-stride an NV12 pair from an external mapping into a pooled two-plane [`DeviceBuffer`],
/// each plane from `src_pitch` to the pool pitch. Context must be current.
///
/// # Safety
/// `y_src` and `uv_src` must address live device memory holding `dst`'s [`PlaneLayout::Nv12`]
/// rows at `src_pitch`.
pub unsafe fn copy_pitched_nv12_to_buffer(
    y_src: CUdeviceptr,
    uv_src: CUdeviceptr,
    src_pitch: usize,
    dst: &DeviceBuffer,
) -> Result<()> {
    let Some((uv_ptr, uv_pitch)) = dst.uv() else {
        anyhow::bail!("copy_pitched_nv12_to_buffer: destination is not an NV12 buffer");
    };
    let [(y_bytes, y_rows), (uv_bytes, uv_rows), _] =
        PlaneLayout::Nv12.planes(dst.width, dst.height);
    let y = device_copy((y_src, src_pitch), (dst.ptr, dst.pitch), y_bytes, y_rows);
    let uv = device_copy((uv_src, src_pitch), (uv_ptr, uv_pitch), uv_bytes, uv_rows);
    // SAFETY: both sources are live and large enough (this fn's contract); `dst`'s planes are
    // its live pooled allocations, which the same plane table sized. Each `copy_blocking` syncs
    // before return.
    unsafe {
        copy_blocking(&y, "cuMemcpy2DAsync_v2(ext->dev nv12 Y)")?;
        copy_blocking(&uv, "cuMemcpy2DAsync_v2(ext->dev nv12 UV)")
    }
}

// `cuda.h` layouts these calls need are compile-time asserted in `ffi.rs` (`const _`).

#[cfg(test)]
mod tests {
    use super::PlaneLayout;

    /// An odd height keeps its last chroma row, and one stacked allocation covers every plane.
    #[test]
    fn nv12_chroma_rounds_up_and_the_stack_covers_it() {
        assert_eq!(PlaneLayout::Nv12.planes(1920, 1080)[1], (1920, 540));
        assert_eq!(PlaneLayout::Nv12.planes(6, 3)[1], (6, 2));
        assert_eq!(PlaneLayout::Nv12.planes(5, 1)[1], (6, 1));
        assert_eq!(PlaneLayout::Nv12.stacked(6, 3), (6, 5));
        assert_eq!(PlaneLayout::Yuv444.stacked(8, 4), (8, 12));
        assert_eq!(PlaneLayout::Packed32.stacked(8, 4), (32, 4));
    }
}
