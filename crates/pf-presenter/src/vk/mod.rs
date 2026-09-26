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

mod gpu;
mod overlay_pipe;
mod present;
mod present_timing;
mod reconfig;
mod resources;
mod setup;

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
    Busy(FrameInput<'a>),
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
    /// In-flight hardware frame; released after the next fence wait.
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
    fence: vk::Fence,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    staging: Option<Staging>,
    video: Option<VideoImage>,
    /// Submit fence has work pending. Wait before recording; also what makes the single
    /// staging buffer safe to overwrite.
    submitted: bool,
    /// Swapchain image taken by the non-blocking probe, waiting for its present.
    acquired: Option<u32>,
    /// `VK_KHR_present_wait` on-glass timing. `None` without present-id/present-wait;
    /// the run loop then keeps its submit-time display stamp.
    present_timer: Option<present_timing::PresentTimer>,
    /// Strictly increasing present id (spec: per swapchain). 0 = none presented with an id.
    next_present_id: u64,
    /// Last successful id-carrying present, awaiting [`Presenter::note_presented`].
    last_presented: Option<(vk::SwapchainKHR, u64)>,
    video_fit: punktfunk_core::video_fit::VideoFit,
    /// Extent, frame size and draw path of the last logged placement.
    placement_logged: Option<(vk::Extent2D, u32, u32, &'static str)>,
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

    /// True when `VK_KHR_present_wait` drives the display stamp. The run loop then
    /// defers e2e/display windows to [`Presenter::take_presented_samples`].
    pub(crate) fn present_timing_active(&self) -> bool {
        self.present_timer.is_some()
    }

    /// Claim the just-submitted present for on-glass timing. Call right after a
    /// `present()` that returned `true`, with that frame's capture + decode stamps.
    /// No-op when timing is inactive.
    pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
        if let (Some(t), Some((sc, id))) = (&self.present_timer, self.last_presented.take()) {
            // Submit stamp: `present()` has returned, so "now" is the present-call tail.
            t.enqueue(
                sc,
                id,
                pts_ns,
                decoded_ns,
                pf_client_core::session::now_ns(),
            );
        }
    }

    /// Undisplayed id-carrying presents in flight (0 when timing is inactive) — the
    /// FIFO glass gate's budget count.
    pub(crate) fn presents_outstanding(&self) -> usize {
        self.present_timer.as_ref().map_or(0, |t| t.outstanding())
    }

    /// Run-loop wake for present completions (SDL event push). No-op without timing.
    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
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

    /// Active swapchain present mode for the stats overlay. Can differ from the request
    /// when the surface does not offer it.
    pub(crate) fn present_mode_name(&self) -> &'static str {
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
    /// composed presents queued ahead of DWM and LATEST_READY drains none of them.
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

    pub(crate) fn take_presented_samples(&self) -> Vec<present_timing::PresentedSample> {
        self.present_timer
            .as_ref()
            .map(|t| t.take_samples())
            .unwrap_or_default()
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

impl Drop for Presenter {
    fn drop(&mut self) {
        // The present-wait waiter holds the swapchain. Drop it (joins in-flight waits,
        // 250 ms cap in `present_timing`) before swapchain teardown below.
        self.present_timer.take();
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe {
            {
                // Against a straggling submitter. The run loop joins the pump first, so
                // this is normally uncontended.
                let _q = self.queue_lock.guard();
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
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            if let Some(v) = self.video.take() {
                if v.framebuffer != vk::Framebuffer::null() {
                    self.device.destroy_framebuffer(v.framebuffer, None);
                }
                if v.view != vk::ImageView::null() {
                    self.device.destroy_image_view(v.view, None);
                }
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
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
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_semaphore(self.acquire_sem, None);
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
