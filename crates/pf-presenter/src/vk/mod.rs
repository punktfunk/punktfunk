//! Swapchain presenter: every decode lane writes one device-local [`VIDEO_FORMAT`] image,
//! then a `vkCmdBlitImage` composite placed by `punktfunk_core::video_fit`.
//!
//! CPU frames stage tightly-packed I420 into three R8 images (`CpuPlanes`) and share
//! the planar CSC pass (`csc.rs`, `csc_rows`) with PyroWave. Linux dmabuf imports NV12
//! per-plane (`dmabuf.rs`); without the four import extensions `supports_dmabuf()` is
//! false and the caller keeps software decode. `NativeVk` and `D3d11` already live on
//! this device.
//!
//! One frame in flight: wait the submit fence before recording. MAILBOX when offered,
//! FIFO otherwise; `PUNKTFUNK_PRESENT_MODE=fifo|mailbox|immediate` pins the mode
//! (`pick_present_mode` — FIFO's present queue must not block an arrival-paced caller).
//! `FrameInput::Redraw` re-blits the retained image on expose/resize.

use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf::HwFrame;
use crate::overlay::SharedDevice;
use ash::vk;
#[cfg(target_os = "linux")]
use pf_client_core::video::DmabufFrame;
use pf_client_core::video::{CpuPlanarFrame, DecodedImage, NativeVkFrame};

#[cfg(target_os = "linux")]
mod export_ring;
pub(crate) mod gpu;
mod overlay_pipe;
mod present;
mod present_timing;
#[cfg(windows)]
mod vblank_timing;
pub(crate) use present_timing::PresentedSample;
mod reconfig;
mod resources;
mod setup;
#[cfg(target_os = "linux")]
mod sync_timeline;
mod timing_ext;

pub use setup::{list_adapters, probe_decode, AdapterDecode, PresentPref};

/// Vulkan version every instance this crate creates puts in `VkApplicationInfo::apiVersion`.
///
/// 1.3 is the floor (Vulkan Video and PyroWave compute) and the ceiling: the loader may
/// be newer, but entry points above 1.3 were never promised. Overlay renderers size
/// their tables from [`crate::overlay::SharedDevice::api_version`], not the loader.
pub const INSTANCE_API_VERSION: u32 = vk::API_VERSION_1_3;

/// The video intermediate every lane's CSC writes, for every stream: PQ in 8 bits bands, and
/// a 10-bit SDR stream would lose the gradients it pays for. Same 32 bpp as RGBA8.
const VIDEO_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;

/// Clamp behind [`Presenter::overlay_api_version`], split out so tests can prove it
/// without a device: min(declared, loader), and a loader that cannot answer is 1.0.
fn overlay_api_version_of(declared: u32, loader: Option<u32>) -> u32 {
    declared.min(loader.unwrap_or(vk::API_VERSION_1_0))
}

/// Video-format probe behind [`AdapterDecode::formats`]. Re-exported so a printer
/// cannot pick up a different `pf-vkdecode` version's flag names.
pub use pf_vkdecode::probe;

impl FrameInput<'_> {
    /// The decoded image back out of a frame the presenter did not consume. The CPU lane
    /// borrows its frame, so the caller still holds that one; `Redraw` carries nothing.
    pub(crate) fn into_image(self) -> Option<DecodedImage> {
        match self {
            FrameInput::Redraw | FrameInput::Cpu(_) => None,
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => Some(DecodedImage::NativeDmabuf(d)),
            #[cfg(windows)]
            FrameInput::D3d11(d) => Some(DecodedImage::D3d11(d)),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => Some(DecodedImage::PyroWave(f)),
            FrameInput::NativeVk(f) => Some(DecodedImage::NativeVk(f)),
        }
    }
}

/// What [`Presenter::present`] did with a frame.
pub enum Presented<'a> {
    Shown,
    /// Swapchain out of date; recreated, frame dropped.
    Stale,
    /// No swapchain image yet: the frame comes back for a retry, unconsumed.
    Busy(FrameInput<'a>, BusyOn),
}

/// What a non-blocking present found busy. The ledger counts each per window: the
/// fence means the GPU still renders the last frame, the acquire means the swapchain
/// holds every image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyOn {
    Fence = 0,
    Acquire = 1,
}

pub enum FrameInput<'a> {
    /// Re-blit the retained video image (expose / resize); no new decode.
    Redraw,
    /// Tightly-packed I420 planes, staged into three R8 images and converted by the
    /// planar CSC pass — same shader, range, matrix, and PQ tone-map as the hardware lanes.
    Cpu(&'a CpuPlanarFrame),
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
    /// Shareable NT-handle texture; imported in `d3d11.rs`.
    #[cfg(windows)]
    D3d11(pf_client_core::video::D3d11Frame),
    /// Three R8 plane views already on this device, decode fence-complete, GENERAL layout.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(pf_client_core::video_pyrowave::PyroWavePlanarFrame),
    /// NV12 image + plane views already on this device. Wait the frame's timeline on
    /// submit, sample, then transition back to the decode layout; drop after the
    /// sampling fence to release the decoder slot.
    NativeVk(NativeVkFrame),
}

#[cfg(target_os = "linux")]
struct HwCtx {
    ext_mem_fd: ash::khr::external_memory_fd::Device,
    /// (format, modifier) importability answers — immutable per device, so the
    /// driver queries run once, not per frame.
    modifier_cache: crate::dmabuf::ModifierCache,
    /// Plane images per decoder surface, imported once per pool generation.
    imports: crate::dmabuf::ImportCache,
    /// Decode sync_file → semaphore. `None`: the frame's fences are polled instead.
    sync: Option<crate::dmabuf::SyncImport>,
    /// Exportable timelines for the native lane's explicit sync; `None` keeps it implicit.
    timelines: Option<sync_timeline::TimelineMaker>,
}

/// Win32 external-memory + keyed-mutex table; present only when both extensions exist.
#[cfg(windows)]
struct HwCtxWin {
    ext_mem_win32: ash::khr::external_memory_win32::Device,
    /// Ring slots imported once per ring generation, not per frame.
    imports: crate::d3d11::ImportCache,
}

/// Hardware frame held until the in-flight fence proves GPU reads are done.
enum Retired {
    #[cfg(target_os = "linux")]
    Dmabuf(HwFrame),
    /// Decoder-owned image + views: destroy nothing; drop after the fence to return the slot.
    NativeVk(NativeVkFrame),
    /// The frame holds its plane ring: a `Redraw` samples planes that are still there.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    Pyro(pf_client_core::video_pyrowave::PyroWavePlanarFrame),
}

/// Which planes the direct pass sampled last, and how. A `Redraw` replays this: the
/// descriptor set still points at those planes and the frame behind them is still held.
#[derive(Clone, Copy)]
struct DirectLast {
    src: DirectSrc,
    uv_scale: [f32; 2],
    color: pf_client_core::video::ColorDesc,
    depth: u8,
    msb_packed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum DirectSrc {
    /// `retired_hw` holds the `NativeVk` frame.
    Native,
    /// `retired_hw` holds the `Dmabuf` frame.
    #[cfg(target_os = "linux")]
    Dmabuf,
    /// The software rung's plane images, which persist.
    Cpu,
    /// `retired_hw` holds the wavelet frame.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    Pyro,
}

/// Premultiplied-alpha quad blended over the swapchain image after the video blit.
/// Recorded only when an overlay frame arrives.
struct OverlayPipe {
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
}

/// Three R8 images the CPU I420 is uploaded into. Owned here, not in `Retired`: nothing
/// outside this device refers to them, and the in-flight fence is waited before each
/// record, so re-uploading into the same images is safe without a ring.
struct CpuPlanes {
    images: [vk::Image; 3],
    memory: [vk::DeviceMemory; 3],
    views: [vk::ImageView; 3],
    /// Luma size; chroma is `div_ceil(2)`, matching the frame.
    width: u32,
    height: u32,
    /// False until the first upload (src UNDEFINED). Later uploads src from
    /// SHADER_READ_ONLY_OPTIMAL, where the previous CSC pass left the images.
    initialized: bool,
}

/// Device-local RGBA the size of the decoded stream; every lane's CSC target before the
/// placed blit.
struct VideoImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
}

/// Host-visible upload buffer for the CPU planes. Grows, never shrinks.
struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    capacity: usize,
}

pub struct Presenter {
    // Field order is not drop order; teardown is explicit in `Drop`.
    entry: ash::Entry,
    instance: ash::Instance,
    surface_i: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    device: ash::Device,
    swap_d: ash::khr::swapchain::Device,
    queue: vk::Queue,
    qfi: u32,
    /// Dmabuf import. `None` without the import extensions; the CSC pass is still built
    /// (Vulkan Video needs it on every device).
    #[cfg(target_os = "linux")]
    hw: Option<HwCtx>,
    /// D3D11 import. `None` without win32 external-memory / keyed-mutex.
    #[cfg(windows)]
    hw_win: Option<HwCtxWin>,
    csc: CscPass,
    /// Planar (3-plane) CSC. Always built: the CPU rung is the last decode fallback.
    csc_planar: CscPass,
    /// CPU-rung Y/Cb/Cr R8 images. `None` until the first CPU frame.
    cpu_planes: Option<CpuPlanes>,
    /// Selected presenter-device facts plus shared handles for the decode lanes.
    /// `video_decode` inside says whether Vulkan Video is usable; the rest of the
    /// bundle answers vendor/import gates without it, so Linux always has a
    /// `Some` here. On Windows `None` means no decode lane could use the device.
    video_export: Option<pf_client_core::video::VulkanDecodeDevice>,
    overlay_pipe: OverlayPipe,
    /// Filtered video scale into the swapchain; its output pass shares the overlay's
    /// framebuffers. Rebuilt with the overlay pipe on an HDR flip.
    scale: crate::scale::ScalePass,
    /// CSC straight into the swapchain image; rebuilt with the overlay pipe on an HDR flip.
    direct: crate::csc::DirectPass,
    /// What the last real frame drew through the direct pass, so a `Redraw` can sample it
    /// again from the retired frame. `None`: the last frame went through the video image.
    direct_last: Option<DirectLast>,
    /// In-flight hardware frame; released after the next fence wait. A `Redraw` keeps it:
    /// the direct path samples it again, so it lives until the next real frame's fence.
    retired_hw: Option<Retired>,
    /// D3D11 lane: the slot last composited and its picture size, which `Redraw` blits
    /// again. The import cache owns the objects; a newer picture in that slot (six
    /// decodes later) is not a visual error.
    #[cfg(windows)]
    retained_slot: Option<(crate::d3d11::Imported, u32, u32)>,
    /// Wall time of this present's D3D11 import lookup and of `vkQueueSubmit`, for the
    /// presenter window line. A submit that blocks on a keyed-mutex acquire shows here.
    last_import_us: u32,
    last_submit_us: u32,
    /// Wall time of the in-flight fence wait, `vkAcquireNextImageKHR`, and
    /// `vkQueuePresentKHR`: where a present blocks on the GPU or the swapchain.
    last_fence_us: u32,
    last_acquire_us: u32,
    last_present_us: u32,
    /// External-sync lock over this device's queues, shared with decode and the overlay.
    /// The decoder submits on this same graphics queue from the pump thread; every
    /// `vkQueueSubmit` / `vkQueuePresentKHR` / wait-idle here must hold it or the
    /// overlap is `VK_ERROR_DEVICE_LOST`.
    queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    format: vk::SurfaceFormatKHR,
    hdr10_format: Option<vk::SurfaceFormatKHR>,
    hdr_active: bool,
    /// One-shot: a PQ frame arrived and the surface has no HDR10 colorspace, so CSC
    /// tone-maps to SDR. Distinguishes "surface cannot advertise HDR" from "host sent SDR".
    hdr_downgrade_warned: bool,
    hdr_metadata_d: Option<ash::ext::hdr_metadata::Device>,
    /// Latest ST.2086/CLL metadata (0xCE plane). Pushed while HDR10 is live; until the
    /// first datagram, a generic HDR10 baseline is pushed instead.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    present_mode: vk::PresentModeKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    /// Per-swapchain-image render-finished semaphores. Present consumes them on the
    /// image's schedule; one shared semaphore can still be held by a previous present.
    render_sems: Vec<vk::Semaphore>,
    acquire_sem: vk::Semaphore,
    /// Timeline each submit signals with its present id: when our GPU work for that
    /// present was done, so the waiter can split our share of latch from the compositor's.
    done_sem: vk::Semaphore,
    fence: vk::Fence,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    staging: Option<Staging>,
    video: Option<VideoImage>,
    /// Submit fence has work pending. Wait before recording; also what makes the single
    /// staging buffer safe to overwrite.
    submitted: bool,
    /// Swapchain image acquired and not yet submitted: the non-blocking probe's, or one an
    /// error left behind. Its `acquire_sem` signal is pending until a batch waits it.
    acquired: Option<u32>,
    /// On-glass timing from present-wait, either generation. `None` without it; the run
    /// loop then keeps its submit-time display stamp (or the vblank waiter's estimate).
    present_timer: Option<present_timing::PresentTimer>,
    /// The waiter runs on `VK_KHR_present_wait2`: presents chain `VkPresentId2KHR` and the
    /// swapchain is created with the present-id2/present-wait2 flags.
    present_id2: bool,
    /// `VK_EXT_present_timing`: the engine stamps each present itself, and the waiter
    /// reads the stamp in place of its own wake time.
    timing: Option<std::sync::Arc<timing_ext::Engine>>,
    /// The live swapchain was created for those stamps and its result queue is sized.
    timing_armed: bool,
    /// The last present asked for a stamp.
    timing_asked: bool,
    /// The output's vblank as the glass clock where present-wait is missing (AMD on
    /// Windows). Estimated stamps, `glass=est`.
    #[cfg(windows)]
    vblank_timer: Option<vblank_timing::VblankTimer>,
    /// Exclusive fullscreen, opt-in: the swapchain owns the monitor's flips.
    #[cfg(windows)]
    fse: Option<FullScreenExclusive>,
    /// Strictly increasing present id (spec: per swapchain). 0 = none presented with an id.
    next_present_id: u64,
    /// Last successful id-carrying present, awaiting [`Presenter::note_presented`].
    last_presented: Option<(vk::SwapchainKHR, u64)>,
    video_fit: punktfunk_core::video_fit::VideoFit,
    /// Extent, frame size and draw path of the last logged placement.
    placement_logged: Option<(vk::Extent2D, u32, u32, &'static str)>,
    /// Wayland lane that hands a dma-buf to the compositor as the window's own buffer.
    #[cfg(target_os = "linux")]
    native: Option<crate::wl_native::NativeLane>,
    /// The compositor's own stamp per swapchain present, beside the driver's.
    #[cfg(target_os = "linux")]
    feedback: Option<crate::wl_feedback::SurfaceFeedback>,
    /// (flipped without a copy, shown) since the last take, from the compositor's stamps.
    #[cfg(target_os = "linux")]
    scanout: (u32, u32),
    /// The compositor has flagged a zero-copy flip at least once. KWin before 6.8 never
    /// sets the bit, so until then a zero means nothing.
    #[cfg(target_os = "linux")]
    scanout_reported: bool,
    /// Compositor stamps by present id, waiting for the driver's sample of the same present.
    #[cfg(target_os = "linux")]
    compositor_stamps: std::collections::HashMap<u64, u64>,
    /// Driver samples without an exact stamp, held one pass for their compositor stamp.
    #[cfg(target_os = "linux")]
    held: Vec<present_timing::PresentedSample>,
    /// Exportable copies of Vulkan Video or PyroWave pictures for the lane; built on the
    /// first one.
    #[cfg(target_os = "linux")]
    export_ring: Option<export_ring::ExportRing>,
    /// The shape a ring could not be built for.
    #[cfg(target_os = "linux")]
    export_refused: Option<RingShape>,
    /// Rings built so far; keeps each ring's lane keys apart.
    #[cfg(target_os = "linux")]
    export_gen: u64,
    /// Exportable copies of the overlay image for the lane's overlay surface.
    #[cfg(target_os = "linux")]
    overlay_ring: Option<export_ring::ExportRing>,
    /// (format, width, height, feedback generation) an overlay ring could not be built for.
    #[cfg(target_os = "linux")]
    overlay_refused: Option<(vk::Format, u32, u32, u64)>,
    /// The overlay image on the lane's overlay surface now.
    #[cfg(target_os = "linux")]
    overlay_shown: Option<vk::Image>,
    /// Explicit-sync timelines per VAAPI pool slot the lane has shown.
    #[cfg(target_os = "linux")]
    vaapi_sync: std::collections::HashMap<u64, sync_timeline::Timelines>,
    /// Pool slots offered to the lane, so a rebuilt pool's old ones can go.
    #[cfg(target_os = "linux")]
    native_keys: Vec<u64>,
    /// The lane's last frame was PQ, shown through the compositor's colour management.
    native_pq: bool,
    /// `flip` diagnostic: when it started; the lane owns even periods, the swapchain odd.
    #[cfg(target_os = "linux")]
    native_flip: Option<std::time::Instant>,
    /// An overlay is up that the lane cannot show: frames go through the swapchain. Only the
    /// Linux lane reads it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    overlay_blocks_native: bool,
    /// The last frame shown went through the native lane, not the swapchain.
    native_last: bool,
    /// The swapchain and its Vulkan surface are torn down while the lane owns the window. A
    /// Vulkan swapchain opts the window's surface into explicit sync, and a buffer committed
    /// without its sync points is a fatal protocol error. The next non-redraw frame through
    /// [`Presenter::present`] builds both again.
    suspended: bool,
}

/// What the native lane did with a Vulkan Video or PyroWave picture.
pub enum NativeVkOutcome<F = NativeVkFrame> {
    /// Copied and committed as the window's buffer.
    Shown,
    /// Copied, but the copy did not finish in time: the frame is gone, nothing shown.
    Dropped,
    /// Not taken; draw it through the swapchain.
    Declined(F),
}

/// (fourcc, width, height, feedback generation, three-plane source) of a picture ring.
#[cfg(target_os = "linux")]
type RingShape = (u32, u32, u32, u64, bool);

/// Where the picture ring left a frame.
#[cfg(target_os = "linux")]
enum RingSlot {
    /// This free, imported slot takes the copy; the lane owns the window.
    Ready(usize),
    /// Every buffer is on screen or importing while the lane owns the window: skip it.
    Drop,
    /// No ring or no free slot yet: the swapchain draws it.
    Decline,
}

impl Presenter {
    /// Whether dmabuf import exists. Callers keep the decoder on software when false.
    #[cfg(target_os = "linux")]
    pub fn supports_dmabuf(&self) -> bool {
        self.hw.is_some()
    }

    /// Whether D3D11 shared-texture import exists. Callers keep software when false.
    #[cfg(windows)]
    pub fn supports_d3d11(&self) -> bool {
        self.hw_win.is_some()
    }

    /// Selected presenter-device facts. `video_decode` inside says whether
    /// Vulkan Video is usable — the rest of the bundle is returned anyway so
    /// vendor and import gates still work; on Linux this is always `Some`.
    /// The frame of present turn `turn` is in hand: the decode lane waits for its submit.
    pub(crate) fn begin_present_turn(&self, turn: u64) {
        self.queue_lock.begin_present_turn(turn);
    }

    /// No frame up to `turn` is about to be submitted: the decode lane may go.
    pub(crate) fn end_present_turn(&self, turn: u64) {
        self.queue_lock.end_present_turn(turn);
    }

    pub fn vulkan_decode(&self) -> Option<pf_client_core::video::VulkanDecodeDevice> {
        self.video_export.clone()
    }

    /// Full device idle. Teardown only, and only after the session pump thread has been
    /// joined (it submits decode work). Mid-session code uses the fence. The queue lock
    /// is held against a straggling submitter.
    pub fn wait_idle(&self) {
        let _q = self.queue_lock.guard();
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe { self.device.device_wait_idle() }.ok();
    }

    /// Let go of the stream's picture before its pump is joined. The decoder frees its
    /// pools once the held frame's token is back or its budget runs out, and a `Redraw`
    /// after that samples freed memory. The console paints opaque over the empty screen.
    pub(crate) fn drop_video(&mut self) {
        // A lost device reads nothing more, so a failed wait still lets go.
        self.quiesce_own().ok();
        if let Some(f) = self.retired_hw.take() {
            f.destroy(&self.device);
        }
        // The ended decoder's pool: cached imports pin its memory, and only a later
        // hardware frame would retire them.
        #[cfg(target_os = "linux")]
        if let Some(hw) = self.hw.as_mut() {
            hw.imports.destroy_all(&self.device);
        }
        #[cfg(windows)]
        if let Some(hw) = self.hw_win.as_mut() {
            hw.imports.destroy_all(&self.device);
        }
        self.direct_last = None;
        if let Some(v) = self.video.take() {
            v.destroy(&self.device);
        }
        #[cfg(windows)]
        {
            self.retained_slot = None;
        }
    }

    /// True when `VK_KHR_present_wait` or the native lane's presentation feedback drives
    /// the display stamp. The run loop then defers e2e/display windows to
    /// [`Presenter::take_presented_samples`].
    pub(crate) fn present_timing_active(&self) -> bool {
        self.glass_active() || self.native_last
    }

    /// A glass clock follows the swapchain's presents: present-wait, or the vblank waiter.
    pub(crate) fn glass_active(&self) -> bool {
        self.present_timer.is_some() || self.vblank_active()
    }

    fn vblank_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.vblank_timer.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The display stamps are the vblank waiter's estimate. Good for the grid and the
    /// ledger; not for counting undisplayed presents, which it confirms a vblank late.
    pub(crate) fn glass_estimated(&self) -> bool {
        self.present_timer.is_none() && self.vblank_active() && !self.native_last
    }

    /// The output's own vblank spacing, where a vblank waiter measures it.
    pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
        #[cfg(windows)]
        {
            let t = self.vblank_timer.as_ref()?;
            Some(t.refresh_ns()).filter(|&p| p > 0)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// The engine's refresh as `(cycle, interval)` in ns, where it stamps presents and has
    /// reported one. The interval is the cycle on a fixed panel, `u64::MAX` under variable
    /// refresh, 0 where the engine cannot tell.
    pub(crate) fn engine_refresh(&self) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering;
        let r = &self.timing.as_ref().filter(|_| self.timing_armed)?.refresh;
        let cycle = r.duration_ns.load(Ordering::Relaxed);
        (cycle > 0).then(|| (cycle, r.interval_ns.load(Ordering::Relaxed)))
    }

    /// Where the display stamp comes from, for the ledger: `timing` (the engine's own
    /// stamps), `wait` (present-wait), `est` (the vblank waiter), `feedback` (the native
    /// lane), `none`.
    pub(crate) fn glass_source(&self) -> &'static str {
        if self.native_last {
            "feedback"
        } else if self.timing_armed {
            "timing"
        } else if self.present_timer.is_some() {
            "wait"
        } else if self.vblank_active() {
            "est"
        } else {
            "none"
        }
    }

    /// The window changed display: the vblank waiter follows it. No-op elsewhere.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub(crate) fn retarget_glass(&self, window: &sdl3::video::Window) {
        #[cfg(windows)]
        if let (Some(t), Some(m)) = (&self.vblank_timer, crate::win32::window_monitor(window)) {
            t.retarget(m);
        }
    }

    /// The native Wayland lane first: `Shown` when the compositor took the dma-buf as the
    /// window's buffer, else the frame comes back for the swapchain path.
    #[cfg(target_os = "linux")]
    pub fn present_native(
        &mut self,
        d: pf_client_core::video::DmabufFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> crate::wl_native::Outcome {
        use crate::wl_native::{Outcome, SlotState};
        self.poll_native_releases();
        let view = (self.extent.width, self.extent.height);
        let suspended = self.suspended;
        if self.flip_holds_swapchain() {
            return Outcome::Declined(d);
        }
        let Some(lane) = self.native.as_mut().filter(|_| !self.overlay_blocks_native) else {
            return Outcome::Declined(d);
        };
        // A rebuilt pool never shows its old slots again: their buffers and timelines go,
        // or each rebuild keeps the old pool pinned in the compositor.
        let generation = d.pool_key >> 32;
        let (sync, device) = (&mut self.vaapi_sync, &self.device);
        self.native_keys.retain(|&k| {
            if k >> 32 == generation {
                return true;
            }
            lane.forget(k);
            if let Some(t) = sync.remove(&k) {
                // SAFETY: host-signalled only; the compositor holds its own syncobj refs.
                unsafe { t.destroy(device) };
            }
            false
        });
        if !lane.takes(&d, view, self.video_fit) {
            return Outcome::Declined(d);
        }
        if !self.native_keys.contains(&d.pool_key) {
            self.native_keys.push(d.pool_key);
        }
        // Owning the window, a new pool slot imports on the spot; before, it imports in the
        // background while the swapchain still draws.
        match lane.prepare(&d, suspended) {
            SlotState::Free => {}
            SlotState::Failed => {
                if !lane.is_dead() {
                    lane.refused(d.fourcc, d.modifier);
                }
                return Outcome::Declined(d);
            }
            // A buffer still on screen or still importing: skip the frame, or let the
            // swapchain draw it while the lane is not in charge yet.
            _ if suspended => return Outcome::Dropped,
            _ => return Outcome::Declined(d),
        }
        if !suspended {
            if let Err(e) = self.suspend_swapchain() {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: swapchain suspend failed");
                return Outcome::Declined(d);
            }
        }
        let (key, pq, meta) = (d.pool_key, d.color.is_pq(), self.hdr_meta);
        let point = match self.vaapi_point(key) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: acquire point");
                return Outcome::Dropped;
            }
        };
        let Some(lane) = self.native.as_mut() else {
            return Outcome::Dropped;
        };
        if lane.commit_vaapi(d, meta, pts_ns, decoded_ns, point) {
            self.native_last = true;
            self.native_pq = pq;
            Outcome::Shown
        } else {
            if let Some(t) = self.vaapi_sync.get_mut(&key) {
                t.uncommit();
            }
            Outcome::Dropped
        }
    }

    /// Under explicit sync, the next acquire and release point for VAAPI slot `key`, its
    /// acquire already signalled from the host: the pump waited the decode. Timelines are made
    /// and imported on the slot's first commit. `None` under implicit sync.
    #[cfg(target_os = "linux")]
    fn vaapi_point(&mut self, key: u64) -> anyhow::Result<Option<u64>> {
        let (Some(lane), Some(maker)) = (
            self.native.as_mut(),
            self.hw.as_ref().and_then(|h| h.timelines.as_ref()),
        ) else {
            return Ok(None);
        };
        if !lane.explicit_sync() {
            return Ok(None);
        }
        let t = match self.vaapi_sync.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                // SAFETY: the maker was made for this presenter's device.
                let t = unsafe { maker.make(&self.device)? };
                let (a, r) = t.fds();
                lane.import_timelines(key, a, r);
                v.insert(t)
            }
        };
        let point = t.next_point();
        // SAFETY: the presenter's device owns the timeline; points only rise.
        if let Err(e) = unsafe { t.signal_acquire(&self.device, point) } {
            t.uncommit();
            return Err(e);
        }
        Ok(Some(point))
    }

    /// Under the `flip` diagnostic, whether this period belongs to the swapchain.
    #[cfg(target_os = "linux")]
    fn flip_holds_swapchain(&self) -> bool {
        self.native_flip.is_some_and(|t| {
            (t.elapsed().as_millis() / crate::wl_native::FLIP_PERIOD.as_millis()) % 2 == 1
        })
    }

    /// Free every lane buffer whose release point passed: ring slots and VAAPI slots.
    #[cfg(target_os = "linux")]
    fn poll_native_releases(&mut self) {
        let Some(lane) = self.native.as_mut() else {
            return;
        };
        for ring in [self.export_ring.as_mut(), self.overlay_ring.as_mut()]
            .into_iter()
            .flatten()
        {
            for key in ring.poll_releases() {
                lane.released(key);
            }
        }
        for (key, t) in &mut self.vaapi_sync {
            // SAFETY: the presenter's device owns the timelines.
            if t.is_committed() && !unsafe { t.poll_held(&self.device) } {
                lane.released(*key);
            }
        }
    }

    /// Hand the window to the native lane: drain our work and tear down the swapchain and its
    /// Vulkan surface, which takes the surface's explicit-sync object with them. Nothing of
    /// the swapchain path's is in flight after the queue drain below.
    #[cfg(target_os = "linux")]
    fn suspend_swapchain(&mut self) -> anyhow::Result<()> {
        use anyhow::Context as _;
        self.quiesce_own()?;
        self.acquired = None;
        {
            let _q = self.queue_lock.guard();
            // SAFETY: `queue` is owned here; `queue_lock` is held so no concurrent submit.
            unsafe { self.device.queue_wait_idle(self.queue) }
                .context("vkQueueWaitIdle (swapchain suspend)")?;
        }
        if let Some(t) = &self.present_timer {
            t.drain();
        }
        self.last_presented = None;
        // The queue wait above is the GPU idle its contract asks for.
        self.overlay_pipe.destroy_targets(&self.device);
        // SAFETY: our fence, the queue and the present waiter are drained above, so nothing
        // still names these objects; destroying a null swapchain or surface is a no-op.
        unsafe {
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            self.swap_d.destroy_swapchain(self.swapchain, None);
            self.surface_i.destroy_surface(self.surface, None);
        }
        self.swapchain = vk::SwapchainKHR::null();
        self.surface = vk::SurfaceKHR::null();
        self.images.clear();
        if let Some(f) = self.retired_hw.take() {
            f.destroy(&self.device); // queue drained above: its reads are done
        }
        self.suspended = true;
        if let Some(lane) = self.native.as_mut() {
            lane.take_window();
        }
        tracing::info!("native scanout: the lane owns the window, swapchain suspended");
        Ok(())
    }

    /// Take the window back from the native lane: a new Vulkan surface on SDL's window and
    /// a swapchain on it.
    #[cfg(target_os = "linux")]
    fn resume_swapchain(&mut self, window: &sdl3::video::Window) -> anyhow::Result<()> {
        // The lane's surface objects go first: the swapchain makes its own. A retired lane
        // never takes the window again, so its exported images go back too.
        if let Some(lane) = self.native.as_mut() {
            lane.release_window();
            if lane.is_dead() {
                self.export_ring = None;
                self.overlay_ring = None;
            }
        }
        // SAFETY: CREATE — `instance` is live; SDL returns a surface we own and destroy.
        let surface = unsafe { window.vulkan_create_surface(self.instance.handle()) }
            .map_err(|e| anyhow::anyhow!("SDL_Vulkan_CreateSurface: {e}"))?;
        self.surface = surface;
        self.suspended = false;
        self.overlay_hide_native();
        tracing::info!("native scanout: the swapchain takes the window back");
        self.recreate_swapchain(window)
    }

    /// The overlay surface comes off when the swapchain draws the overlay itself.
    #[cfg(target_os = "linux")]
    fn overlay_hide_native(&mut self) {
        if let Some(lane) = self.native.as_mut() {
            lane.overlay_hide();
        }
        self.overlay_shown = None;
    }

    /// A Vulkan Video picture through the native lane: copied into an exportable buffer on a
    /// modifier the compositor listed, then committed as the window's buffer. Declined when
    /// the lane is off, the picture is not a copyable NV12/P010 that fills the window in a
    /// colour the compositor takes, or no ring slot is free and imported.
    pub fn present_native_vk(
        &mut self,
        f: NativeVkFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome {
        #[cfg(target_os = "linux")]
        return self.present_native_vk_linux(f, pts_ns, decoded_ns);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pts_ns, decoded_ns);
            NativeVkOutcome::Declined(f)
        }
    }

    #[cfg(target_os = "linux")]
    fn present_native_vk_linux(
        &mut self,
        f: NativeVkFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome {
        self.poll_native_releases();
        let view = (self.extent.width, self.extent.height);
        if self.overlay_blocks_native || self.flip_holds_swapchain() {
            return NativeVkOutcome::Declined(f);
        }
        let Some(lane) = self.native.as_mut() else {
            return NativeVkOutcome::Declined(f);
        };
        lane.pump();
        let Some(target) = export_ring::fourcc_for(f.vk_format) else {
            return NativeVkOutcome::Declined(f);
        };
        let even = |v: u32| v % 2 == 0;
        let takes = f.copyable
            && even(f.width)
            && even(f.height)
            && even(f.crop_x)
            && even(f.crop_y)
            && lane.fits(f.color, (f.width, f.height), view, self.video_fit);
        if !takes {
            return NativeVkOutcome::Declined(f);
        }
        let slot = match self.picture_slot(target, (f.width, f.height), false) {
            RingSlot::Ready(slot) => slot,
            RingSlot::Drop => return NativeVkOutcome::Dropped,
            RingSlot::Decline => return NativeVkOutcome::Declined(f),
        };
        let Some(ring) = self.export_ring.as_mut() else {
            return NativeVkOutcome::Declined(f);
        };
        // SAFETY: the frame's handles live on this device while its guard is held (below);
        // `queue` is this presenter's, externally synchronised by `queue_lock`.
        let copied = unsafe { ring.copy(slot, &f, self.queue, &self.queue_lock) };
        match copied {
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: picture copy failed");
                NativeVkOutcome::Declined(f)
            }
            Ok(copied) => {
                // The submit carries `value + 1`: the decoder waits it before reusing the picture.
                let mut f = f;
                f.guard.mark_presented();
                let color = f.color;
                drop(f);
                if self.commit_picture(slot, copied, color, pts_ns, decoded_ns) {
                    NativeVkOutcome::Shown
                } else {
                    NativeVkOutcome::Dropped
                }
            }
        }
    }

    /// A PyroWave picture through the native lane: Y copied and Cb/Cr interleaved into an
    /// exported NV12 or P010 buffer, then committed as the window's buffer. Declined as
    /// [`Self::present_native_vk`], and at 4:4:4, which display planes do not scan out as YUV.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pub fn present_native_pyro(
        &mut self,
        f: pf_client_core::video_pyrowave::PyroWavePlanarFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome<pf_client_core::video_pyrowave::PyroWavePlanarFrame> {
        #[cfg(target_os = "linux")]
        return self.present_native_pyro_linux(f, pts_ns, decoded_ns);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (pts_ns, decoded_ns);
            NativeVkOutcome::Declined(f)
        }
    }

    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    fn present_native_pyro_linux(
        &mut self,
        f: pf_client_core::video_pyrowave::PyroWavePlanarFrame,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> NativeVkOutcome<pf_client_core::video_pyrowave::PyroWavePlanarFrame> {
        use ash::vk::Handle as _;
        self.poll_native_releases();
        let view = (self.extent.width, self.extent.height);
        if self.overlay_blocks_native || self.flip_holds_swapchain() {
            return NativeVkOutcome::Declined(f);
        }
        let Some(lane) = self.native.as_mut() else {
            return NativeVkOutcome::Declined(f);
        };
        lane.pump();
        let takes = !f.chroma444
            && f.width % 2 == 0
            && f.height % 2 == 0
            && lane.fits(f.color, (f.width, f.height), view, self.video_fit);
        if !takes {
            return NativeVkOutcome::Declined(f);
        }
        let target = if f.ten_bit {
            (
                export_ring::DRM_FORMAT_P010,
                vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            )
        } else {
            (
                export_ring::DRM_FORMAT_NV12,
                vk::Format::G8_B8R8_2PLANE_420_UNORM,
            )
        };
        let slot = match self.picture_slot(target, (f.width, f.height), true) {
            RingSlot::Ready(slot) => slot,
            RingSlot::Drop => return NativeVkOutcome::Dropped,
            RingSlot::Decline => return NativeVkOutcome::Declined(f),
        };
        let Some(ring) = self.export_ring.as_mut() else {
            return NativeVkOutcome::Declined(f);
        };
        let luma = vk::Image::from_raw(f.luma);
        let [_, cb, cr] = f.views.map(vk::ImageView::from_raw);
        // SAFETY: the planes live on this device in GENERAL and their decode was submitted
        // on this queue; the decoder orders its next write of them after this copy's compute
        // and copy stages. `queue` is this presenter's, externally synchronised by
        // `queue_lock`.
        let copied =
            unsafe { ring.copy_planar(slot, luma, [cb, cr], self.queue, &self.queue_lock) };
        match copied {
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: PyroWave copy failed");
                NativeVkOutcome::Declined(f)
            }
            Ok(copied) => {
                if self.commit_picture(slot, copied, f.color, pts_ns, decoded_ns) {
                    NativeVkOutcome::Shown
                } else {
                    NativeVkOutcome::Dropped
                }
            }
        }
    }

    /// A free, imported slot of the picture ring for a `size` picture copied to `target`
    /// (fourcc, format), taking the window from the swapchain before handing it out. The ring
    /// is rebuilt when the shape, the source or the compositor's feedback changes; a
    /// `planar` ring carries the chroma pass.
    #[cfg(target_os = "linux")]
    fn picture_slot(
        &mut self,
        (fourcc, format): (u32, vk::Format),
        size: (u32, u32),
        planar: bool,
    ) -> RingSlot {
        use crate::wl_native::SlotState;
        let (Some(lane), Some(hw)) = (self.native.as_mut(), self.hw.as_ref()) else {
            return RingSlot::Decline;
        };
        // Timelines only where the lane will put points on the surface.
        let timelines = hw.timelines.as_ref().filter(|_| lane.explicit_sync());
        let want: RingShape = (fourcc, size.0, size.1, lane.feedback_generation(), planar);
        let shape =
            |r: &export_ring::ExportRing| (r.fourcc, r.width, r.height, r.feedback_gen, r.planar());
        if self.export_ring.as_ref().is_some_and(|r| shape(r) != want) {
            if let Some(old) = self.export_ring.take() {
                for i in 0..old.len() {
                    lane.forget(old.key(i));
                }
            }
        }
        if self.export_ring.is_none() {
            if self.export_refused == Some(want) {
                return RingSlot::Decline;
            }
            self.export_gen += 1;
            // SAFETY: the presenter's live, paired handles; `hw` exists only when the device
            // enabled the dma-buf extension set the ring needs.
            let built = unsafe {
                export_ring::ExportRing::new(
                    &self.instance,
                    self.pdev,
                    &self.device,
                    &hw.ext_mem_fd,
                    &self.mem_props,
                    self.qfi,
                    (fourcc, format),
                    size,
                    &lane.modifiers_for(fourcc),
                    want.3,
                    export_ring::KEY_PICTURES | (self.export_gen << 8),
                    timelines,
                )
            }
            .and_then(|mut ring| {
                if planar {
                    // SAFETY: the handles the ring was just built with.
                    unsafe { ring.add_interleave(&self.instance, self.pdev, &self.mem_props)? };
                }
                Ok(ring)
            });
            match built {
                Ok(ring) => {
                    tracing::info!(
                        source = if planar { "PyroWave" } else { "Vulkan Video" },
                        fourcc = format!("{fourcc:#010x}"),
                        modifier = format!("{:#x}", ring.modifier),
                        width = size.0,
                        height = size.1,
                        explicit_sync = timelines.is_some(),
                        "native scanout: pictures copy into exported buffers"
                    );
                    for i in 0..ring.len() {
                        lane.import(ring.key(i), size, fourcc, ring.modifier, &ring.planes(i));
                        if let Some((a, r)) = ring.sync_fds(i) {
                            lane.import_timelines(ring.key(i), a, r);
                        }
                    }
                    self.export_ring = Some(ring);
                }
                Err(e) => {
                    tracing::info!(
                        error = %format!("{e:#}"),
                        "native scanout: no exportable copy target — Vulkan presents"
                    );
                    self.export_refused = Some(want);
                    return RingSlot::Decline;
                }
            }
        }
        let Some(ring) = self.export_ring.as_mut() else {
            return RingSlot::Decline;
        };
        if (0..ring.len()).any(|i| lane.slot_state(ring.key(i)) == SlotState::Failed) {
            lane.refused(fourcc, ring.modifier);
            // The lane is retired for the session: its exported images go back.
            self.export_ring = None;
            return RingSlot::Decline;
        }
        let Some(slot) = ring.free_slot(|k| lane.slot_state(k) == SlotState::Free) else {
            // Every buffer on screen or still importing: skip the frame, or let the swapchain
            // draw it while the lane is not in charge yet.
            return if self.suspended {
                RingSlot::Drop
            } else {
                RingSlot::Decline
            };
        };
        if !self.suspended {
            if let Err(e) = self.suspend_swapchain() {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: swapchain suspend failed");
                return RingSlot::Decline;
            }
        }
        RingSlot::Ready(slot)
    }

    /// Commit `slot` of the picture ring after its copy; true when shown. A copy that ran
    /// late, or a commit the lane could not make, shows nothing.
    #[cfg(target_os = "linux")]
    fn commit_picture(
        &mut self,
        slot: usize,
        copied: export_ring::Copied,
        color: pf_client_core::video::ColorDesc,
        pts_ns: u64,
        decoded_ns: u64,
    ) -> bool {
        let export_ring::Copied::Ready(point) = copied else {
            return false;
        };
        let (Some(lane), Some(ring)) = (self.native.as_mut(), self.export_ring.as_mut()) else {
            return false;
        };
        let hold = Box::new(ring.hold(slot));
        let key = ring.key(slot);
        if lane.commit(key, color, self.hdr_meta, hold, pts_ns, decoded_ns, point) {
            self.native_last = true;
            self.native_pq = color.is_pq();
            true
        } else {
            ring.uncommit(slot);
            false
        }
    }

    /// Once per pass, after the overlay renders: while the lane holds the window, the overlay
    /// goes on its own surface above the picture (copied when its image changes) and comes
    /// off when it empties. An overlay the lane cannot show sends frames back through the
    /// swapchain, which composites it. `logical` is the window's size in surface units.
    pub(crate) fn sync_native_overlay(
        &mut self,
        overlay: Option<&crate::overlay::OverlayFrame>,
        logical: (u32, u32),
    ) {
        #[cfg(target_os = "linux")]
        self.sync_native_overlay_linux(overlay, logical);
        #[cfg(not(target_os = "linux"))]
        let _ = (overlay, logical);
    }

    #[cfg(target_os = "linux")]
    fn sync_native_overlay_linux(
        &mut self,
        overlay: Option<&crate::overlay::OverlayFrame>,
        logical: (u32, u32),
    ) {
        use crate::wl_native::SlotState;
        self.poll_native_releases();
        let Some(lane) = self.native.as_mut() else {
            return;
        };
        let Some(o) = overlay else {
            self.overlay_blocks_native = false;
            lane.overlay_hide();
            self.overlay_shown = None;
            return;
        };
        let fourcc = export_ring::overlay_fourcc(o.format);
        let shape = (o.format, o.width, o.height, lane.feedback_generation());
        // Until the lane has shown the compositor scans it out, an overlay goes through the
        // swapchain: drawn into its buffer, the frame can still be scanned out.
        self.overlay_blocks_native = !lane.scans_out()
            || !lane.overlay_supported()
            || fourcc.is_none()
            || self.overlay_refused == Some(shape);
        if self.overlay_blocks_native || !self.native_last {
            lane.overlay_hide();
            self.overlay_shown = None;
            return;
        }
        if self.overlay_shown == Some(o.image) {
            return;
        }
        let (Some(fourcc), Some(hw)) = (fourcc, self.hw.as_ref()) else {
            return;
        };
        let timelines = hw.timelines.as_ref().filter(|_| lane.explicit_sync());
        if self
            .overlay_ring
            .as_ref()
            .is_some_and(|r| (r.format, r.width, r.height, r.feedback_gen) != shape)
        {
            if let Some(old) = self.overlay_ring.take() {
                for i in 0..old.len() {
                    lane.forget(old.key(i));
                }
            }
        }
        if self.overlay_ring.is_none() {
            self.export_gen += 1;
            // SAFETY: the presenter's live, paired handles; `hw` exists only when the device
            // enabled the dma-buf extension set the ring needs.
            let built = unsafe {
                export_ring::ExportRing::new(
                    &self.instance,
                    self.pdev,
                    &self.device,
                    &hw.ext_mem_fd,
                    &self.mem_props,
                    self.qfi,
                    (fourcc, o.format),
                    (o.width, o.height),
                    &lane.modifiers_for(fourcc),
                    shape.3,
                    export_ring::KEY_OVERLAY | (self.export_gen << 8),
                    timelines,
                )
            };
            match built {
                Ok(ring) => {
                    for i in 0..ring.len() {
                        lane.import(
                            ring.key(i),
                            (o.width, o.height),
                            fourcc,
                            ring.modifier,
                            &ring.planes(i),
                        );
                        if let Some((a, r)) = ring.sync_fds(i) {
                            lane.import_timelines(ring.key(i), a, r);
                        }
                    }
                    self.overlay_ring = Some(ring);
                }
                Err(e) => {
                    tracing::info!(
                        error = %format!("{e:#}"),
                        "native scanout: the overlay has no exportable copy target — the \
                         swapchain draws while it is up"
                    );
                    self.overlay_refused = Some(shape);
                    self.overlay_blocks_native = true;
                    return;
                }
            }
        }
        let Some(ring) = self.overlay_ring.as_mut() else {
            return;
        };
        if (0..ring.len()).any(|i| lane.slot_state(ring.key(i)) == SlotState::Failed) {
            self.overlay_refused = Some(shape);
            self.overlay_blocks_native = true;
            lane.overlay_hide();
            return;
        }
        // Imports still pending, or every buffer on screen: the next pass tries again.
        let Some(slot) = ring.free_slot(|k| lane.slot_state(k) == SlotState::Free) else {
            return;
        };
        // SAFETY: `o.image` is the overlay's live image in the ring's format and size, last
        // written on this queue; `queue` is this presenter's, synchronised by `queue_lock`.
        let copied = unsafe { ring.copy_overlay(slot, o.image, self.queue, &self.queue_lock) };
        match copied {
            Ok(export_ring::Copied::Ready(point)) => {
                let hold = Box::new(ring.hold(slot));
                if lane.overlay_show(ring.key(slot), logical, hold, point) {
                    self.overlay_shown = Some(o.image);
                } else {
                    ring.uncommit(slot);
                }
            }
            Ok(export_ring::Copied::Late) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "native scanout: overlay copy failed");
                self.overlay_refused = Some(shape);
                self.overlay_blocks_native = true;
                lane.overlay_hide();
            }
        }
    }

    /// Presents the engine reported as never shown since the last call: a newer one
    /// replaced them before a refresh took them.
    pub(crate) fn take_unshown(&self) -> u32 {
        self.present_timer.as_ref().map_or(0, |t| t.take_unshown())
    }

    /// (flipped without a copy, shown) since the last call, by the compositor's own word.
    /// `None` until the compositor has ever flagged a flip: a zero from one that never
    /// says so is not "composited".
    #[cfg(target_os = "linux")]
    pub(crate) fn take_scanout(&mut self) -> Option<(u32, u32)> {
        let out = std::mem::take(&mut self.scanout);
        self.scanout_reported.then_some(out)
    }

    /// (zero-copy, presented) the native lane counted since the last call.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_native_zero_copy(&mut self) -> (u32, u32) {
        self.native.as_mut().map_or((0, 0), |l| l.take_zero_copy())
    }

    /// Claim the just-submitted present for on-glass timing. Call right after a
    /// `present()` that returned `true`, with that frame's capture + decode stamps.
    /// No-op when timing is inactive.
    pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
        let Some((sc, id)) = self.last_presented.take() else {
            return;
        };
        // Submit stamp: `present()` has returned, so "now" is the present-call tail.
        // The submit signalled `done_sem` with this id when its GPU work finished.
        let done = (self.done_sem != vk::Semaphore::null()).then_some((self.done_sem, id));
        let now_ns = pf_client_core::session::now_ns();
        if let Some(t) = &self.present_timer {
            t.enqueue(present_timing::Job {
                swapchain: sc,
                present_id: id,
                done,
                stamped: std::mem::take(&mut self.timing_asked),
                pts_ns,
                decoded_ns,
                submitted_ns: now_ns,
            });
        }
        #[cfg(windows)]
        if let Some(t) = &self.vblank_timer {
            t.enqueue(done, pts_ns, decoded_ns, now_ns, !self.vblank_locked());
        }
    }

    /// Undisplayed id-carrying presents in flight (0 when timing is inactive) — the
    /// FIFO glass gate's budget count.
    pub(crate) fn presents_outstanding(&self) -> usize {
        let waited = self.present_timer.as_ref().map_or(0, |t| t.outstanding());
        #[cfg(windows)]
        let waited = waited + self.vblank_timer.as_ref().map_or(0, |t| t.outstanding());
        waited
    }

    /// Run-loop wake for present completions (SDL event push). No-op without timing.
    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
        #[cfg(windows)]
        if self.present_timer.is_none() {
            if let Some(t) = &self.vblank_timer {
                t.set_wake(cb);
            }
            return;
        }
        if let Some(t) = &self.present_timer {
            t.set_wake(cb);
        }
    }

    /// `(import_us, submit_us)` of the last present: D3D11 import lookup and `vkQueueSubmit`.
    pub(crate) fn last_timings(&self) -> (u32, u32) {
        (self.last_import_us, self.last_submit_us)
    }

    /// `(fence_us, acquire_us, present_us)` of the last present: the in-flight fence
    /// wait, `vkAcquireNextImageKHR`, and `vkQueuePresentKHR`.
    pub(crate) fn last_waits(&self) -> (u32, u32, u32) {
        (
            self.last_fence_us,
            self.last_acquire_us,
            self.last_present_us,
        )
    }

    /// Active present path for the stats overlay: `native` while the compositor holds the
    /// window's buffer, else the swapchain mode, which can differ from the request when the
    /// surface does not offer it.
    pub(crate) fn present_mode_name(&self) -> &'static str {
        if self.native_last {
            return "native";
        }
        match self.present_mode {
            vk::PresentModeKHR::MAILBOX => "mailbox",
            vk::PresentModeKHR::FIFO => "fifo",
            vk::PresentModeKHR::FIFO_RELAXED => "fifo-relaxed",
            vk::PresentModeKHR::IMMEDIATE => "immediate",
            setup::fifo_latest_ready::MODE => "fifo-latest-ready",
            _ => "other",
        }
    }

    /// True when the swapchain itself can queue presents — the only modes the glass gate
    /// governs. MAILBOX and IMMEDIATE replace or drop stale images in the driver, and
    /// so does `FIFO_LATEST_READY` — except on Windows, where DXGI keeps up to three
    /// composed presents queued ahead of DWM and LATEST_READY drains none of them. Under
    /// Wayland the gate on LATEST_READY measured no gain at 60 fps and held a 120 fps
    /// stream 20 ms: a completion there is not the flip.
    pub(crate) fn needs_glass_gate(&self) -> bool {
        let fifo = matches!(
            self.present_mode,
            vk::PresentModeKHR::FIFO | vk::PresentModeKHR::FIFO_RELAXED
        );
        fifo || (cfg!(windows) && self.present_mode == setup::fifo_latest_ready::MODE)
    }

    /// True when presents land on the vblank grid — the VRR cadence probe's premise.
    /// The whole FIFO family qualifies (`FIFO_LATEST_READY` drops stale images but still
    /// presents on the refresh boundary). MAILBOX/IMMEDIATE do not.
    pub(crate) fn vblank_locked(&self) -> bool {
        matches!(
            self.present_mode,
            vk::PresentModeKHR::FIFO
                | vk::PresentModeKHR::FIFO_RELAXED
                | setup::fifo_latest_ready::MODE
        )
    }

    pub(crate) fn take_presented_samples(&mut self) -> Vec<present_timing::PresentedSample> {
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut out = self
            .present_timer
            .as_ref()
            .map(|t| t.take_samples())
            .unwrap_or_default();
        #[cfg(windows)]
        if let Some(t) = &self.vblank_timer {
            out.extend(t.take_samples());
        }
        // Both stamps per present: the compositor's joins the driver's below, by id.
        #[cfg(target_os = "linux")]
        if let Some(fb) = self.feedback.as_mut() {
            for s in fb.take() {
                if let Some(t) = s.displayed_ns {
                    self.scanout.1 += 1;
                    if s.zero_copy {
                        self.scanout.0 += 1;
                        self.scanout_reported = true;
                    }
                    self.compositor_stamps.insert(s.present_id, t);
                }
                tracing::trace!(
                    target: "pf_glass",
                    id = s.present_id,
                    displayed_ns = s.displayed_ns.unwrap_or(0),
                    zero_copy = s.zero_copy,
                    refresh_ns = s.refresh_ns,
                    "compositor stamp"
                );
            }
        }
        // The driver's half of the join, on every OS: a Windows build without it has no
        // reader of the id at all.
        for s in &out {
            tracing::trace!(
                target: "pf_glass",
                id = s.present_id,
                displayed_ns = s.displayed_ns,
                submitted_ns = s.submitted_ns,
                exact = s.exact,
                "driver stamp"
            );
        }
        #[cfg(target_os = "linux")]
        if let Some(lane) = self.native.as_mut() {
            out.extend(lane.take_samples().into_iter().map(|s| {
                present_timing::PresentedSample {
                    present_id: 0,
                    pts_ns: s.pts_ns,
                    decoded_ns: s.decoded_ns,
                    submitted_ns: s.submitted_ns,
                    // No GPU work of ours on this lane: the whole latch is the compositor's.
                    gpu_done_ns: s.submitted_ns,
                    displayed_ns: s.displayed_ns,
                    // The compositor's own presentation time.
                    exact: true,
                }
            }));
        }
        #[cfg(target_os = "linux")]
        {
            out = self.join_compositor_stamps(out);
        }
        out
    }

    /// A driver sample without the engine's stamp takes the compositor's for the same
    /// present. One with neither yet waits a pass, and everything behind it waits too so
    /// stamps leave in order; after that it goes out on its wake time. The map keeps only
    /// ids newer than the last sample out.
    #[cfg(target_os = "linux")]
    fn join_compositor_stamps(
        &mut self,
        fresh: Vec<present_timing::PresentedSample>,
    ) -> Vec<present_timing::PresentedSample> {
        let mut ready = Vec::with_capacity(self.held.len() + fresh.len());
        for mut s in self.held.drain(..) {
            Self::take_stamp(&mut self.compositor_stamps, &mut s);
            ready.push(s);
        }
        let mut holding = false;
        for mut s in fresh {
            let joined = Self::take_stamp(&mut self.compositor_stamps, &mut s);
            holding |= !joined && s.present_id != 0;
            if holding {
                self.held.push(s);
            } else {
                ready.push(s);
            }
        }
        if let Some(last) = ready.last() {
            let floor = last.present_id.saturating_sub(64);
            self.compositor_stamps.retain(|&id, _| id >= floor);
        }
        ready
    }

    /// `true` once the sample carries an exact display time, its own or the compositor's.
    #[cfg(target_os = "linux")]
    fn take_stamp(
        stamps: &mut std::collections::HashMap<u64, u64>,
        s: &mut present_timing::PresentedSample,
    ) -> bool {
        let stamp = stamps.remove(&s.present_id);
        if s.exact {
            return true;
        }
        match stamp {
            Some(t) => {
                s.displayed_ns = t;
                s.exact = true;
                true
            }
            None => false,
        }
    }

    /// Device handles the overlay renders on. Valid for the presenter's lifetime; the
    /// run loop drops the overlay first.
    pub fn shared_device(&self) -> SharedDevice {
        SharedDevice {
            entry: self.entry.clone(),
            instance: self.instance.clone(),
            physical_device: self.pdev,
            device: self.device.clone(),
            queue: self.queue,
            queue_family_index: self.qfi,
            queue_lock: self.queue_lock.clone(),
            api_version: self.overlay_api_version(),
            av1_decode: pf_client_core::video::av1_hardware_decodable(self.video_export.as_ref()),
        }
    }

    /// Vulkan version an overlay renderer may size its function table to: the lower of
    /// [`INSTANCE_API_VERSION`] and what the loader actually provides.
    ///
    /// Both halves are load-bearing. The loader can be newer than we declared — entry
    /// points in between resolve to null. A 1.1+ loader can also accept our 1.3 instance
    /// as intent without delivering 1.3. The minimum is the only number true on both sides.
    fn overlay_api_version(&self) -> u32 {
        // SAFETY: per the Vulkan contract above - `vkEnumerateInstanceVersion` is a global
        // command taking no handles, resolved through the loaded entry that owns it; it writes
        // one `u32` local. Absent (a 1.0 loader) it reports `None` rather than failing.
        let loader = unsafe { self.entry.try_enumerate_instance_version() }
            .ok()
            .flatten();
        overlay_api_version_of(INSTANCE_API_VERSION, loader)
    }
}

/// `VK_EXT_full_screen_exclusive`, application-controlled: the swapchain takes the
/// monitor's flips, so a variable-refresh panel follows our presents where the desktop
/// compositor would hold the window. Opt-in (`PUNKTFUNK_FULLSCREEN_EXCLUSIVE=1`); the
/// PresentMon leg of the pacing plan decides the default. Every query the exclusive
/// swapchain is built from carries the same pNext chain, as the spec requires.
#[cfg(windows)]
pub(crate) struct FullScreenExclusive {
    pub(crate) device: ash::ext::full_screen_exclusive::Device,
    pub(crate) instance: ash::ext::full_screen_exclusive::Instance,
    pub(crate) caps2: ash::khr::get_surface_capabilities2::Instance,
    /// The window's `HMONITOR`; the one the swapchain claims.
    pub(crate) monitor: isize,
}

#[cfg(windows)]
impl FullScreenExclusive {
    fn chain(
        &self,
    ) -> (
        vk::SurfaceFullScreenExclusiveInfoEXT<'static>,
        vk::SurfaceFullScreenExclusiveWin32InfoEXT<'static>,
    ) {
        (
            vk::SurfaceFullScreenExclusiveInfoEXT::default()
                .full_screen_exclusive(vk::FullScreenExclusiveEXT::APPLICATION_CONTROLLED),
            vk::SurfaceFullScreenExclusiveWin32InfoEXT::default()
                .hmonitor(self.monitor as vk::HMONITOR),
        )
    }

    /// Surface capabilities as the exclusive swapchain will see them.
    pub(crate) fn capabilities(
        &self,
        pdev: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
    ) -> anyhow::Result<vk::SurfaceCapabilitiesKHR> {
        use anyhow::Context as _;
        let (mut info, mut win32) = self.chain();
        let surface_info = vk::PhysicalDeviceSurfaceInfo2KHR::default()
            .surface(surface)
            .push_next(&mut info)
            .push_next(&mut win32);
        let mut caps2 = vk::SurfaceCapabilities2KHR::default();
        // SAFETY: live handles; the chained locals outlive the call.
        unsafe {
            self.caps2
                .get_physical_device_surface_capabilities2(pdev, &surface_info, &mut caps2)
        }
        .context("vkGetPhysicalDeviceSurfaceCapabilities2KHR")?;
        Ok(caps2.surface_capabilities)
    }

    /// `want` if the exclusive surface offers it, else FIFO (guaranteed).
    pub(crate) fn present_mode(
        &self,
        pdev: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        want: vk::PresentModeKHR,
    ) -> vk::PresentModeKHR {
        let (mut info, mut win32) = self.chain();
        let surface_info = vk::PhysicalDeviceSurfaceInfo2KHR::default()
            .surface(surface)
            .push_next(&mut info)
            .push_next(&mut win32);
        // SAFETY: live handles; the chained locals outlive the call.
        let modes = unsafe {
            self.instance
                .get_physical_device_surface_present_modes2(pdev, &surface_info)
        }
        .unwrap_or_default();
        if modes.contains(&want) {
            want
        } else {
            vk::PresentModeKHR::FIFO
        }
    }

    /// Chain the exclusive request onto a swapchain create. The pair must outlive it.
    pub(crate) fn extend<'a>(
        &self,
        info: vk::SwapchainCreateInfoKHR<'a>,
        pair: &'a mut (
            vk::SurfaceFullScreenExclusiveInfoEXT<'static>,
            vk::SurfaceFullScreenExclusiveWin32InfoEXT<'static>,
        ),
    ) -> vk::SwapchainCreateInfoKHR<'a> {
        *pair = self.chain();
        info.push_next(&mut pair.0).push_next(&mut pair.1)
    }

    /// Take the monitor for `swapchain`. A refusal leaves a windowed swapchain.
    pub(crate) fn acquire(&self, swapchain: vk::SwapchainKHR) {
        // SAFETY: `swapchain` was just created on this device.
        match unsafe { self.device.acquire_full_screen_exclusive_mode(swapchain) } {
            Ok(()) => tracing::info!("exclusive fullscreen acquired"),
            Err(e) => tracing::warn!(
                error = ?e,
                "exclusive fullscreen refused — presenting windowed"
            ),
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // The present-wait waiter holds the swapchain. Drop it (joins in-flight waits,
        // 250 ms cap in `present_timing`) before swapchain teardown below. The vblank
        // waiter reads `done_sem`: it goes before the semaphore does.
        self.present_timer.take();
        #[cfg(windows)]
        self.vblank_timer.take();
        // The ring waits its own copies; the lane's buffers go before the images behind them.
        #[cfg(target_os = "linux")]
        {
            self.native.take();
            self.export_ring.take();
            self.overlay_ring.take();
            for (_, t) in self.vaapi_sync.drain() {
                // SAFETY: host-signalled only; the compositor holds its own syncobj refs.
                unsafe { t.destroy(&self.device) };
            }
        }
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe {
            {
                // Against a straggling submitter. The run loop joins the pump first, so
                // this is normally uncontended.
                let _q = self.queue_lock.guard();
                // A held image's semaphore signal must not outlive the semaphore. The
                // `queue_lock` held here is `retire_acquire_sem`'s contract.
                if self.acquired.take().is_some() {
                    self.retire_acquire_sem().ok();
                }
                self.device.device_wait_idle().ok();
            }
            if let Some(f) = self.retired_hw.take() {
                f.destroy(&self.device); // GPU idle above — reads are done
            }
            #[cfg(windows)]
            if let Some(hw) = self.hw_win.as_mut() {
                hw.imports.destroy_all(&self.device); // GPU idle above
            }
            #[cfg(target_os = "linux")]
            if let Some(hw) = self.hw.as_mut() {
                hw.imports.destroy_all(&self.device); // GPU idle above
                if let Some(s) = hw.sync.take() {
                    s.destroy(&self.device);
                }
            }
            if let Some(s) = self.staging.take() {
                s.destroy(&self.device);
            }
            if let Some(v) = self.video.take() {
                v.destroy(&self.device);
            }
            #[cfg(target_os = "linux")]
            self.hw.take();
            self.csc.destroy(&self.device);
            self.csc_planar.destroy(&self.device);
            if let Some(p) = self.cpu_planes.take() {
                p.destroy(&self.device);
            }
            self.overlay_pipe.destroy(&self.device);
            self.scale.destroy(&self.device);
            self.direct.destroy(&self.device);
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_semaphore(self.acquire_sem, None);
            self.device.destroy_semaphore(self.done_sem, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(self.swapchain, None);
            }
            self.device.destroy_device(None);
            self.surface_i.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
        // `entry` (libvulkan) must outlive every vk call.
        let _ = &self.entry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_loader_never_raises_the_cap() {
        let loader = vk::make_api_version(0, 1, 4, 321);
        assert_eq!(
            overlay_api_version_of(INSTANCE_API_VERSION, Some(loader)),
            INSTANCE_API_VERSION
        );
    }

    /// A 1.1+ loader accepts our 1.3 `apiVersion` as intent even when it cannot deliver
    /// 1.3, so the overlay must not be promised 1.3 functions the loader lacks.
    #[test]
    fn an_older_loader_lowers_the_cap() {
        let loader = vk::make_api_version(0, 1, 2, 198);
        assert_eq!(
            overlay_api_version_of(INSTANCE_API_VERSION, Some(loader)),
            loader
        );
    }

    #[test]
    fn a_loader_that_cannot_answer_is_1_0() {
        assert_eq!(
            overlay_api_version_of(INSTANCE_API_VERSION, None),
            vk::API_VERSION_1_0
        );
    }
}
