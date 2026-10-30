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
//! FIFO otherwise; `knobs.present_mode` (`fifo|mailbox|immediate`) pins the mode
//! (`pick_present_mode` — FIFO's present queue must not block an arrival-paced caller).
//! `FrameInput::Redraw` re-blits the retained image on expose/resize.

use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf::HwFrame;
use crate::overlay::SharedDevice;
#[cfg(target_os = "linux")]
use crate::reconfig_model::Plan;
use ash::vk;
#[cfg(target_os = "linux")]
use pf_client_core::video::DmabufFrame;
use pf_client_core::video::{CpuPlanarFrame, DecodedImage, NativeVkFrame};
#[cfg(target_os = "linux")]
use reconfig::Input;

#[cfg(target_os = "linux")]
mod export_ring;
pub(crate) mod gpu;
mod overlay_pipe;
mod present;
pub(crate) mod present_timing;
#[cfg(windows)]
pub(crate) mod vblank_timing;
pub(crate) use present::PresentError;
pub(crate) use present_timing::PresentedSample;
mod reconfig;
mod resources;
mod setup;
#[cfg(target_os = "linux")]
mod sync_timeline;
mod targets;
pub(crate) mod timing_ext;

#[cfg(windows)]
pub(crate) use setup::probe_import;
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
    /// Loaded when the device imports the ring's shared fences.
    ext_sem_win32: Option<ash::khr::external_semaphore_win32::Device>,
    /// Ring slots imported once per ring generation, not per frame.
    imports: crate::d3d11::ImportCache,
    /// Last `read` value a fenced submit signalled. It only goes up, across ring
    /// generations, so every fresh `read` fence sees increasing values.
    reads: u64,
    /// The last real fenced frame. Kept past a suspend, which drops `retired_hw` while
    /// `retained_slot` still names this slot.
    parked: Option<crate::d3d11::Parked>,
}

/// Hardware frame held until the in-flight fence proves GPU reads are done. Dropping one
/// releases its decoder slot; none owns a Vulkan object.
enum Retired {
    #[cfg(target_os = "linux")]
    Dmabuf(#[allow(dead_code, reason = "held for its drop")] HwFrame),
    NativeVk(NativeVkFrame),
    /// The frame holds its plane ring: a `Redraw` samples planes that are still there.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    Pyro(
        #[allow(dead_code, reason = "held for its drop")]
        pf_client_core::video_pyrowave::PyroWavePlanarFrame,
    ),
}

/// The retired frame and what the direct pass drew from it, which a `Redraw` replays
/// while the frame is held.
struct Held {
    frame: Retired,
    direct: Option<DirectLast>,
}

/// Which planes the direct pass sampled, and how. A `Redraw` replays this: the descriptor
/// set still points at those planes, and the frame or the CPU planes behind them are held.
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
    Native,
    /// A Vulkan Video picture through the conversion pipe, which never draws direct.
    NativeYcbcr,
    #[cfg(target_os = "linux")]
    Dmabuf,
    /// The software rung's plane images, which persist.
    Cpu,
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    Pyro,
}

/// Premultiplied-alpha quad blended over the swapchain image after the video blit.
/// Recorded only when an overlay frame arrives.
#[derive(Default)]
struct OverlayPipe {
    pass: gpu::Pass,
    desc_set: vk::DescriptorSet,
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
type VideoImage = gpu::RenderImage;

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
    /// CSC through a sampler Y'CbCr conversion. Built by the first picture that needs it:
    /// only a device with no plane views has any.
    csc_ycbcr: CscPass,
    /// The picture format [`Self::csc_ycbcr`] converts; `UNDEFINED` until it exists.
    csc_ycbcr_format: vk::Format,
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
    /// What the last real frame drew direct from the CPU planes, so a `Redraw` can sample
    /// them again. `None`: it was another lane's, or went through the video image.
    cpu_direct: Option<DirectLast>,
    /// In-flight hardware frame; released after the next fence wait. A `Redraw` keeps it:
    /// the direct path samples it again, so it lives until the next real frame's fence.
    retired_hw: Option<Held>,
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
    hdr10_format: Option<vk::SurfaceFormatKHR>,
    hdr_active: bool,
    /// One-shot: a PQ frame arrived and the surface has no HDR10 colorspace, so CSC
    /// tone-maps to SDR. Distinguishes "surface cannot advertise HDR" from "host sent SDR".
    hdr_downgrade_warned: bool,
    hdr_metadata_d: Option<ash::ext::hdr_metadata::Device>,
    /// Latest ST.2086/CLL metadata (0xCE plane). Pushed while HDR10 is live; until the
    /// first datagram, a generic HDR10 baseline is pushed instead.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// The intent `present_mode` was picked from, device facts resolved.
    pref: PresentPref,
    present_mode: vk::PresentModeKHR,
    /// The swapchain and what it brings; replaced whole by a rebuild.
    targets: targets::Targets,
    acquire_sem: vk::Semaphore,
    /// Timeline each submit signals with its present id: when our GPU work for that
    /// present was done, so the waiter can split our share of latch from the compositor's.
    done_sem: vk::Semaphore,
    fence: vk::Fence,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    staging: Option<Staging>,
    video: Option<VideoImage>,
    /// Who presents into the window. Only `Presenter::decide` writes it.
    owner: crate::reconfig_model::Owner,
    /// The submit in flight, the held image.
    state: crate::swapchain_state::SwapchainState,
    /// The glass instruments, their lifecycle with the swapchain, the present ids.
    glass: crate::glass::GlassClock,
    /// Exclusive fullscreen, opt-in: the swapchain owns the monitor's flips.
    #[cfg(windows)]
    fse: Option<FullScreenExclusive>,
    video_fit: punktfunk_core::video_fit::VideoFit,
    /// Extent, frame size and draw path of the last logged placement.
    placement_logged: Option<(vk::Extent2D, u32, u32, &'static str)>,
    /// Wayland lane that hands a dma-buf to the compositor as the window's own buffer.
    #[cfg(target_os = "linux")]
    native: Option<crate::wl_native::NativeLane>,
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
    /// `flip` diagnostic: when it started; the lane owns even periods, the swapchain odd.
    #[cfg(target_os = "linux")]
    native_flip: Option<std::time::Instant>,
    /// An overlay is up that the lane cannot show: frames go through the swapchain. Only the
    /// Linux lane reads it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    overlay_blocks_native: bool,
    /// The shell's knobs: the per-frame and rebuild paths read theirs here.
    knobs: crate::knobs::Knobs,
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
        self.retired_hw = None;
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
        self.cpu_direct = None;
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
        self.clocks().active()
    }

    fn clocks(&self) -> crate::glass::Clocks {
        self.glass.clocks(self.owner.native_last())
    }

    /// The display stamps are the vblank waiter's estimate. Good for the grid and the
    /// ledger; not for counting undisplayed presents, which it confirms a vblank late.
    pub(crate) fn glass_estimated(&self) -> bool {
        self.clocks().estimated()
    }

    /// The output's own vblank spacing, where a vblank waiter measures it.
    pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
        self.glass.measured_refresh_ns()
    }

    /// The engine's refresh as `(cycle, interval)` in ns, where it stamps presents and has
    /// reported one. The interval is the cycle on a fixed panel, `u64::MAX` under variable
    /// refresh, 0 where the engine cannot tell.
    pub(crate) fn engine_refresh(&self) -> Option<(u64, u64)> {
        self.glass.engine_refresh()
    }

    /// Where the display stamp comes from, for the ledger.
    pub(crate) fn glass_source(&self) -> &'static str {
        self.clocks().rank()
    }

    /// The window changed display: the vblank waiter follows it. No-op elsewhere.
    #[cfg_attr(not(windows), allow(unused_variables))]
    pub(crate) fn retarget_glass(&self, window: &sdl3::video::Window) {
        #[cfg(windows)]
        if let Some(m) = crate::win32::window_monitor(window) {
            self.glass.retarget(m);
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
        let view = (self.targets.extent.width, self.targets.extent.height);
        let suspended = self.owner.suspended();
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
            if let Err(e) = self.hand_to_lane() {
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
            self.decide(Input::Shown { pq });
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

    /// Hand the window to the native lane: drain our work and tear down the targets and the
    /// Vulkan surface, which takes the surface's explicit-sync object with them. The drain is
    /// the rebuild's, so the acquire after resume finds `acquire_sem` unsignalled. The extent
    /// and format stay for the resume.
    #[cfg(target_os = "linux")]
    fn suspend_swapchain(&mut self) -> anyhow::Result<()> {
        self.quiesce_own()?;
        self.drain_targets("swapchain suspend")?;
        let old = self.targets.take();
        // SAFETY: our fence, the queue and the present waiter are drained above, so nothing
        // still names the targets, the swapchain or the surface.
        unsafe {
            old.destroy(&self.device, &self.swap_d);
            self.surface_i.destroy_surface(self.surface, None);
        }
        self.surface = vk::SurfaceKHR::null();
        self.retired_hw = None; // queue drained above: its reads are done
        self.decide(Input::Suspended);
        if let Some(lane) = self.native.as_mut() {
            lane.take_window();
        }
        tracing::info!("native scanout: the lane owns the window, swapchain suspended");
        Ok(())
    }

    /// The lane takes the window. `LaneTakes` always plans `Suspend`, which needs no window.
    #[cfg(target_os = "linux")]
    fn hand_to_lane(&mut self) -> anyhow::Result<()> {
        let plan = self.decide(Input::LaneTakes);
        debug_assert_eq!(plan, Plan::Suspend);
        self.suspend_swapchain()
    }

    /// `Resume`: take the window back from the native lane, a new Vulkan surface on SDL's
    /// window and a swapchain on it. `Dropped` leaves the lane with the window: a minimized
    /// window has nothing to size a swapchain by, and the next real frame tries again.
    #[cfg(target_os = "linux")]
    fn resume_swapchain(
        &mut self,
        window: &sdl3::video::Window,
    ) -> anyhow::Result<reconfig::Applied> {
        let (pw, ph) = window.size_in_pixels();
        if pw == 0 || ph == 0 {
            return Ok(reconfig::Applied::Dropped); // the owner stays on the lane
        }
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
        self.overlay_hide_native();
        // The pref may have moved while the lane held the window (`set_present_pref`).
        let pinned = self.knobs.present_mode.as_deref();
        let built =
            setup::pick_present_mode(&self.surface_i, self.pdev, surface, self.pref, pinned)
                .and_then(|mode| self.reconfigure(Input::Resumed { mode }, window));
        if let Err(e) = built {
            // No swapchain stands: the lane keeps the window, or the next frame acquires
            // from a null swapchain.
            self.hand_to_lane()?;
            return Err(e);
        }
        tracing::info!("native scanout: the swapchain takes the window back");
        Ok(reconfig::Applied::Done)
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
        let view = (self.targets.extent.width, self.targets.extent.height);
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
        let view = (self.targets.extent.width, self.targets.extent.height);
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
            return if self.owner.suspended() {
                RingSlot::Drop
            } else {
                RingSlot::Decline
            };
        };
        if !self.owner.suspended() {
            if let Err(e) = self.hand_to_lane() {
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
            self.decide(Input::Shown { pq: color.is_pq() });
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
        if self.overlay_blocks_native || !self.owner.native_last() {
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
        self.glass.take_unshown()
    }

    /// (flipped without a copy, shown) since the last call, by the compositor's own word.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_scanout(&mut self) -> Option<(u32, u32)> {
        self.glass.take_scanout()
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
        let off_grid = !self.vblank_locked();
        self.glass
            .note_presented(self.done_sem, pts_ns, decoded_ns, off_grid);
    }

    /// Undisplayed id-carrying presents in flight (0 when timing is inactive) — the
    /// FIFO glass gate's budget count.
    pub(crate) fn presents_outstanding(&self) -> usize {
        self.glass.outstanding()
    }

    /// Run-loop wake for present completions (SDL event push). No-op without timing.
    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
        self.glass.set_wake(cb);
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
        if self.owner.native_last() {
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

    /// The glass clock's samples, then the native lane's, which the compositor stamped.
    pub(crate) fn take_presented_samples(&mut self) -> Vec<present_timing::PresentedSample> {
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut lane = Vec::new();
        #[cfg(target_os = "linux")]
        if let Some(native) = self.native.as_mut() {
            lane.extend(native.take_samples().into_iter().map(|s| {
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
        self.glass.take_samples(lane)
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
            av1_decode: pf_client_core::video::av1_advertised(self.video_export.as_ref()),
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
/// compositor would hold the window. Opt-in (`knobs.fullscreen_exclusive`); the
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
        // The present-wait waiter holds the swapchain and the vblank waiter reads `done_sem`:
        // both join before the teardown below.
        self.glass.stop();
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
                if self.state.acquired.take().is_some() {
                    self.retire_acquire_sem().ok();
                }
                self.device.device_wait_idle().ok();
            }
            self.retired_hw = None; // GPU idle above — reads are done
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
            self.csc_ycbcr.destroy(&self.device);
            if let Some(p) = self.cpu_planes.take() {
                p.destroy(&self.device);
            }
            self.overlay_pipe.destroy(&self.device);
            self.scale.destroy(&self.device);
            self.direct.destroy(&self.device);
            self.targets.take().destroy(&self.device, &self.swap_d);
            self.device.destroy_semaphore(self.acquire_sem, None);
            self.device.destroy_semaphore(self.done_sem, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
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
