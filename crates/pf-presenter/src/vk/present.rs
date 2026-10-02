//! Per-frame present: `FrameInput` → video image → CSC → placed scale or blit → present.
//! A D3D11 RGB slot arrives converted: it is the blit source and the `Redraw` picture,
//! with no video image. A D3D11 planar slot goes through CSC like the native lane.
//!
//! [`Presenter::present`] returns `false` when the swapchain is out of date; the
//! caller recreates with current window state and may retry. One frame in flight:
//! the submit fence covers the command buffer, staging buffer, and the parked
//! hardware frame. Hardware lanes import or bind before acquire so a failed
//! import does not consume the acquire semaphore.
//!
//! HDR follows the frame's PQ flag. No HDR10 surface → CSC shader mode 1
//! tonemaps onto SDR. Pin peak with `PUNKTFUNK_TONEMAP_PEAK` (default 4.9 ≈
//! 1000 nits / 203). Windows: `PUNKTFUNK_D3D11_NO_MUTEX=1` skips the keyed mutex.
//!
//! Evidence: `csc_depth_packing` table tests; `design/pyrowave-444-hdr.md`.

use super::gpu::*;
use super::present_timing::SLICE_NS;
use super::{BusyOn, DirectLast, DirectSrc, FrameInput, Presented, Presenter, Retired};
use crate::csc::csc_rows;
#[cfg(target_os = "linux")]
use crate::dmabuf::{self, HwFrame};
use crate::overlay::OverlayFrame;
use anyhow::{Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
#[cfg(windows)]
use pf_client_core::video::SlotFormat;
use pf_client_core::video::{CpuPlanarFrame, NativeVkFrame, NativeVkLayout, RawVkFormat};

/// `PUNKTFUNK_TONEMAP_PEAK`, read once: the environment lock and a string per frame is not
/// a price the CSC record pays. Default 4.9 ≈ 1000 nits / 203-nit reference.
fn tonemap_peak() -> f32 {
    static PEAK: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *PEAK.get_or_init(|| {
        std::env::var("PUNKTFUNK_TONEMAP_PEAK")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(4.9)
    })
}

/// `PUNKTFUNK_DIRECT_PRESENT=0` keeps every lane on the video image: an A/B on one build.
fn direct_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_DIRECT_PRESENT").ok().as_deref() != Some("0"))
}

/// Where a CSC pass draws.
#[derive(Clone, Copy)]
enum CscTarget {
    /// The video image, blitted or filtered into the swapchain afterwards.
    Video {
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
    },
    /// A swapchain image through the overlay's framebuffer: `rect` is the picture's place,
    /// `surface` the whole image, which `clear` paints black first (the letterbox).
    Direct {
        framebuffer: vk::Framebuffer,
        surface: vk::Extent2D,
        rect: vk::Rect2D,
        clear: bool,
    },
}

/// This present's frame after import: what every later phase matches on. At most one
/// lane runs per present.
enum Lane<'a> {
    /// No new frame: show the retained picture again.
    Redraw,
    Cpu(&'a CpuPlanarFrame),
    #[cfg(target_os = "linux")]
    Dmabuf(HwFrame),
    #[cfg(windows)]
    D3d11(pf_client_core::video::D3d11Frame, crate::d3d11::Imported),
    Native(NativeVkFrame),
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    Pyro(pf_client_core::video_pyrowave::PyroWavePlanarFrame),
}

impl Lane<'_> {
    /// A real frame that samples on the GPU, not from the software plane images.
    fn is_hw(&self) -> bool {
        !matches!(self, Lane::Redraw | Lane::Cpu(_))
    }

    /// The size the video image must have before the CSC pass writes it. `None`: no
    /// new picture, or a D3D11 RGB slot the composite reads itself.
    fn video_size(&self) -> Option<(u32, u32)> {
        match self {
            Lane::Redraw => None,
            Lane::Cpu(f) => Some((f.width, f.height)),
            #[cfg(target_os = "linux")]
            Lane::Dmabuf(f) => Some((f.width, f.height)),
            #[cfg(windows)]
            Lane::D3d11(f, imported) => imported.planes.map(|_| (f.width, f.height)),
            Lane::Native(f) => Some((f.width, f.height)),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Lane::Pyro(f) => Some((f.width, f.height)),
        }
    }
}

/// The picture's place on the swapchain image for the direct pass.
#[derive(Clone, Copy)]
struct DirectPlan {
    rect: vk::Rect2D,
    clear: bool,
}

/// `PUNKTFUNK_D3D11_NO_MUTEX=1`, read once (debugging only: torn frames).
#[cfg(windows)]
fn keyed_mutex_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PUNKTFUNK_D3D11_NO_MUTEX").is_none())
}

impl Presenter {
    /// How a frame of another aspect fills the swapchain. Takes effect on the next present.
    pub fn set_video_fit(&mut self, fit: punktfunk_core::video_fit::VideoFit) {
        self.video_fit = fit;
        self.placement_logged = None;
    }

    /// Log where a `width`×`height` frame lands and how it is drawn (`path`). A new frame
    /// size, fit or path logs at info; a window resize alone logs at debug, so a drag does
    /// not flood the log.
    fn log_placement(
        &mut self,
        width: u32,
        height: u32,
        p: &punktfunk_core::video_fit::Placement,
        path: &'static str,
    ) {
        let key = (self.extent, width, height, path);
        if self.placement_logged == Some(key) {
            return;
        }
        let new_frame = self
            .placement_logged
            .is_none_or(|(_, w, h, was)| (w, h, was) != (width, height, path));
        self.placement_logged = Some(key);
        macro_rules! log_placement {
            ($level:ident) => {
                tracing::$level!(
                    fit = self.video_fit.name(),
                    path,
                    view = ?(self.extent.width, self.extent.height),
                    frame = ?(width, height),
                    dst = ?(p.dst_x, p.dst_y, p.dst_w, p.dst_h),
                    src = ?(p.src_x, p.src_y, p.src_w, p.src_h),
                    scale = ?(p.scale_x, p.scale_y),
                    kernel = ?(p.kernel_x().name(), p.kernel_y().name()),
                    "video placement"
                )
            };
        }
        if new_frame {
            log_placement!(info);
        } else {
            log_placement!(debug);
        }
    }

    /// Present one frame. `Stale` means the swapchain is out of date — the caller
    /// recreates it (current window state) and may retry. `Busy` hands the frame back:
    /// the swapchain has no image yet (FIFO with no glass stamps to gate on), so the
    /// caller keeps the frame and tries again shortly instead of blocking on the queue.
    pub fn present<'a>(
        &mut self,
        window: &sdl3::video::Window,
        input: FrameInput<'a>,
        overlay: Option<&OverlayFrame>,
    ) -> Result<Presented<'a>> {
        if self.extent.width == 0 || self.extent.height == 0 {
            return Ok(Presented::Shown); // minimized: not Stale (Stale recreates)
        }
        // While the native lane owns the window a swapchain redraw would paint over the
        // picture (and has no swapchain to paint into); a real frame takes the window back.
        if self.native_last || self.suspended {
            if matches!(input, FrameInput::Redraw) {
                return Ok(Presented::Shown);
            }
            self.native_last = false;
            #[cfg(target_os = "linux")]
            if self.suspended {
                self.resume_swapchain(window)?;
            }
        }
        // FIFO without present-wait: the queue is policed here rather than by blocking.
        // Give the previous submit's fence up to 1 ms (it is the frame's real gate, and a
        // sleep-and-retry either spins or wakes late), then take the image ahead of time;
        // either one not ready means a refresh has not passed yet. Probed before `input`
        // is consumed so the frame can go back to the store whole.
        let nonblocking = self.needs_glass_gate()
            && self.present_timer.is_none()
            && !matches!(input, FrameInput::Redraw);
        if nonblocking {
            if self.submitted {
                // SAFETY: `fence` is owned here; a bounded wait is always legal.
                match unsafe { self.device.wait_for_fences(&[self.fence], true, 1_000_000) } {
                    Ok(()) => {}
                    Err(vk::Result::TIMEOUT) => {
                        return Ok(Presented::Busy(input, BusyOn::Fence));
                    }
                    Err(e) => return Err(e).context("vkWaitForFences"),
                }
            }
            if self.acquired.is_none() {
                // The non-blocking probe runs only without a present waiter: no swapchain lock.
                // SAFETY: `swapchain`/`acquire_sem` are owned. No image is held, so every wait
                // on `acquire_sem` is complete: a present's by its fence (checked above), a
                // discarded image's by `recreate_swapchain`'s queue drain.
                match unsafe {
                    self.swap_d.acquire_next_image(
                        self.swapchain,
                        0,
                        self.acquire_sem,
                        vk::Fence::null(),
                    )
                } {
                    Ok((index, _)) => self.acquired = Some(index),
                    Err(vk::Result::NOT_READY) | Err(vk::Result::TIMEOUT) => {
                        return Ok(Presented::Busy(input, BusyOn::Acquire));
                    }
                    Err(vk::Result::ERROR_OUT_OF_DATE_KHR)
                    | Err(vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT) => {
                        self.recreate_swapchain(window)?;
                        return Ok(Presented::Stale);
                    }
                    Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
                }
            }
        }
        // HDR follows this frame's PQ flag before any work. No HDR10 surface →
        // PQ stays on the SDR swapchain; CSC shader mode 1 tonemaps.
        let frame_pq = match &input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some(f.color.is_pq()),
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => Some(d.color.is_pq()),
            #[cfg(windows)]
            FrameInput::D3d11(d) => Some(d.color.is_pq()),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => Some(f.color.is_pq()),
            FrameInput::NativeVk(f) => Some(f.color.is_pq()),
        };
        if let Some(pq) = frame_pq {
            // Once: missing HDR is the surface/compositor, not a host that omitted PQ.
            if pq && self.hdr10_format.is_none() && !self.hdr_downgrade_warned {
                self.hdr_downgrade_warned = true;
                tracing::warn!(
                    "PQ (HDR10) stream tone-mapped to SDR — the surface offers no HDR10 \
                     colorspace, so no HDR is committed to the compositor. Under gamescope this \
                     usually means the gamescope Vulkan WSI layer is not visible in the sandbox."
                );
            }
            let want = pq && self.hdr10_format.is_some();
            if want != self.hdr_active {
                self.set_hdr_mode(window, want)?;
            }
        }
        let redraw = matches!(input, FrameInput::Redraw);
        // Import/view before acquire: a reject must fail before this present
        // consumes the acquire semaphore.
        let lane = self.import(input)?;

        // One frame in flight: the fence covers the command buffer, the staging
        // buffer, and the previously submitted hw frame.

        let fence_started = std::time::Instant::now();
        // SAFETY: `fence` is owned here. `submitted` means the last `queue_submit`
        // named it; wait idles that submit, then reset is legal.
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
            self.device.reset_fences(&[self.fence])?;
        }
        self.last_fence_us = fence_started.elapsed().as_micros() as u32;

        // Acquire before anything below retires the picture on screen. An out-of-date
        // swapchain drops this frame, and the next `Redraw` must still replay the last one.
        let acquire_started = std::time::Instant::now();
        // An image taken by the non-blocking probe above is used as is.
        let acquired = match self.acquired {
            Some(index) => Ok((index, false)),
            None => {
                // With a present waiter, each call holds the swapchain for one bounded slice.
                let timeout = match self.present_timer {
                    Some(_) => SLICE_NS,
                    None => u64::MAX,
                };
                loop {
                    let _swapchain = self.present_timer.as_ref().map(|t| t.swapchain_guard());
                    // SAFETY: `swapchain` and `acquire_sem` are owned here; the guard above is
                    // the swapchain's host sync. No image is held, so every wait on
                    // `acquire_sem` is complete: a present's by the fence wait above, a
                    // discarded image's by `recreate_swapchain`'s queue drain.
                    let r = unsafe {
                        self.swap_d.acquire_next_image(
                            self.swapchain,
                            timeout,
                            self.acquire_sem,
                            vk::Fence::null(),
                        )
                    };
                    if r != Err(vk::Result::TIMEOUT) {
                        break r;
                    }
                }
            }
        };
        let (index, _suboptimal) = match acquired {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR)
            | Err(vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT) => {
                // Acquire failed: GPU never saw the import; destroy it here.
                #[cfg(target_os = "linux")]
                if let Lane::Dmabuf(f) = lane {
                    f.destroy(&self.device);
                }
                self.recreate_swapchain(window)?;
                return Ok(Presented::Stale);
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        };
        // Held until the submit waits `acquire_sem`: an error before then leaves the image
        // for the next present, or for `recreate_swapchain` to retire.
        self.acquired = Some(index);
        self.last_acquire_us = acquire_started.elapsed().as_micros() as u32;
        // A `Redraw` samples the retired frame again through the direct pass, so it goes
        // with the next real frame, whose fence covers the redraw's reads too.
        if !redraw {
            if let Some(old) = self.retired_hw.take() {
                old.destroy(&self.device);
            }
        }
        // Nothing is in flight past the wait above: imports of a superseded ring
        // generation can go now.
        #[cfg(windows)]
        if let (Lane::D3d11(d, _), Some(hw)) = (&lane, self.hw_win.as_mut()) {
            // The retained slot may name a retired import; a `Redraw` must not sample it.
            if hw.imports.retire_stale(&self.device, d.generation) {
                self.retained_slot = None;
            }
        }
        // Same for a rebuilt VAAPI pool; another lane's frame means that decoder is
        // gone, and its cached imports pin the pool's memory until they go too.
        #[cfg(target_os = "linux")]
        if let Some(hw) = self.hw.as_mut() {
            match &lane {
                Lane::Dmabuf(f) => hw.imports.retire_stale(&self.device, f.generation()),
                l if l.is_hw() && !hw.imports.is_empty() => hw.imports.destroy_all(&self.device),
                _ => {}
            }
        }
        // First fence wait is the first moment the software plane images are
        // unreferenced. Hardware lane will not sample them again.
        if lane.is_hw() {
            if let Some(p) = self.cpu_planes.take() {
                tracing::debug!("freeing the software rung's plane images (hardware lane)");
                p.destroy(&self.device);
            }
        }

        let cpu_offsets = self.bind(&lane)?;
        if let Some(o) = overlay {
            // Descriptor set idle: fence wait above.
            let infos = [vk::DescriptorImageInfo::default()
                .image_view(o.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let writes = [vk::WriteDescriptorSet::default()
                .dst_set(self.overlay_pipe.desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos)];
            // SAFETY: overlay `desc_set` is owned here; fence wait above means no
            // in-flight cmd buf samples it. `writes`/`infos` outlive the call.
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        }

        // Where the picture lands and which path draws it. A fractional scale goes through the
        // filter ladder (`scale.rs`); whole-number scales blit exactly. The shader samples the
        // video image and draws through the overlay's framebuffers, so both must exist.
        #[cfg(windows)]
        let slot = match &lane {
            Lane::D3d11(d, f) if f.planes.is_none() => Some((*f, d.width, d.height)),
            _ => self.retained_slot.filter(|_| redraw),
        };
        #[cfg(windows)]
        let from_slot = slot.is_some();
        #[cfg(not(windows))]
        let from_slot = false;
        let source = self.video.as_ref().map(|v| (v.image, v.width, v.height));
        #[cfg(windows)]
        let source = slot.map(|(f, w, h)| (f.image, w, h)).or(source);
        let view = (self.extent.width, self.extent.height);
        let placement = source
            .map(|(_, w, h)| punktfunk_core::video_fit::place(self.video_fit, view, (w, h)))
            .filter(|p| !p.is_empty());
        let targets_ready = self.overlay_pipe.framebuffers.len() == self.images.len();
        let filtered = match (placement, &self.video) {
            (Some(p), Some(v)) if !from_slot && targets_ready && crate::scale::needs_filter(&p) => {
                self.scale
                    .prepare(&self.device, &self.mem_props, v.height, &p, v.view)?;
                Some(p)
            }
            _ => None,
        };
        // Direct CSC into the swapchain image: the whole picture at an exact scale, from a
        // lane that samples on this device. A `Redraw` replays the last real frame's planes
        // while the frame behind them is still held. Everything else takes the video image.
        let last: Option<DirectLast> = match &lane {
            Lane::Redraw => self.direct_last.filter(|l| match l.src {
                DirectSrc::Native => matches!(self.retired_hw, Some(Retired::NativeVk(_))),
                #[cfg(target_os = "linux")]
                DirectSrc::Dmabuf => matches!(self.retired_hw, Some(Retired::Dmabuf(_))),
                DirectSrc::Cpu => self.cpu_planes.is_some(),
            }),
            Lane::Native(f) => {
                let (depth, msb_packed) = csc_depth_packing_or_8bit(f.vk_format);
                Some(DirectLast {
                    src: DirectSrc::Native,
                    uv_scale: [
                        f.width as f32 / f.coded_width as f32,
                        f.height as f32 / f.coded_height as f32,
                    ],
                    color: f.color,
                    depth,
                    msb_packed,
                })
            }
            Lane::Cpu(f) => Some(DirectLast {
                src: DirectSrc::Cpu,
                uv_scale: [1.0, 1.0],
                color: f.color,
                depth: 8,
                msb_packed: false,
            }),
            #[cfg(target_os = "linux")]
            Lane::Dmabuf(f) => Some(DirectLast {
                src: DirectSrc::Dmabuf,
                uv_scale: f.uv_scale(),
                color: f.color,
                depth: if f.is_p010() { 10 } else { 8 },
                msb_packed: f.is_p010(),
            }),
            #[cfg(windows)]
            Lane::D3d11(..) => None,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Lane::Pyro(_) => None,
        };
        let direct = match (last, source, placement) {
            (Some(l), Some((_, w, h)), Some(p))
                if direct_enabled()
                    && targets_ready
                    && !from_slot
                    && filtered.is_none()
                    && p.src_x == 0.0
                    && p.src_y == 0.0
                    && p.src_w == f64::from(w)
                    && p.src_h == f64::from(h) =>
            {
                let covered = p.dst_x == 0
                    && p.dst_y == 0
                    && p.dst_w == self.extent.width
                    && p.dst_h == self.extent.height;
                let rect = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: p.dst_x as i32,
                        y: p.dst_y as i32,
                    },
                    extent: vk::Extent2D {
                        width: p.dst_w,
                        height: p.dst_h,
                    },
                };
                Some((
                    l,
                    DirectPlan {
                        rect,
                        clear: !covered,
                    },
                ))
            }
            _ => None,
        };
        if !redraw {
            self.direct_last = direct.map(|(l, _)| l);
        }
        if let (Some((_, w, h)), Some(p)) = (source, placement) {
            // A D3D11 RGB ring slot is imported TRANSFER_SRC only, so a fractional scale of it
            // stays a bilinear blit until the import also asks for SAMPLED.
            let path = if direct.is_some() {
                "direct"
            } else if filtered.is_some() {
                "filtered"
            } else if crate::scale::needs_filter(&p) {
                "bilinear blit"
            } else {
                "exact blit"
            };
            self.log_placement(w, h, &p, path);
        }

        let swap_image = self.images[index as usize];
        let direct_target = direct.map(|(_, plan)| CscTarget::Direct {
            framebuffer: self.overlay_pipe.framebuffers[index as usize],
            surface: self.extent,
            rect: plan.rect,
            clear: plan.clear,
        });

        // SAFETY: `cmd_buf` is owned and idle (fence wait above). Recording names
        // images/views/sets this presenter owns (or a live overlay/native frame
        // parked until the next fence). Submit and present take `queue` under
        // `queue_lock`. Builders are locals that outlive each call.
        unsafe {
            self.device.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // The VideoProcessor already delivered RGB matching the HDR mode: the
            // composite reads an RGB slot itself (this frame's, or the retained one on
            // `Redraw`). Cross-API sync is the keyed mutex on submit, not these barriers.
            #[cfg(windows)]
            if let Some((f, _, _)) = slot {
                external_acquire_barrier(
                    &self.device,
                    self.cmd_buf,
                    f.image,
                    self.qfi,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::AccessFlags::TRANSFER_READ,
                );
            }

            let mut native_wait: Option<(vk::Semaphore, u64)> = None;
            match &lane {
                // CSC render pass leaves the video image in TRANSFER_SRC for the blit.
                #[cfg(target_os = "linux")]
                Lane::Dmabuf(f) => {
                    if let Some(v) = &self.video {
                        for view_image in [f.luma_image(), f.chroma_image()] {
                            foreign_acquire_barrier(
                                &self.device,
                                self.cmd_buf,
                                view_image,
                                self.qfi,
                            );
                        }
                        let extent = vk::Extent2D {
                            width: v.width,
                            height: v.height,
                        };
                        let ten_bit = f.is_p010();
                        // Imported images span the full exported (coded) extent; the
                        // CSC pass crops them to the visible picture.
                        let target = direct_target.unwrap_or(CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent,
                        });
                        self.record_csc(
                            false,
                            target,
                            f.uv_scale(),
                            f.color,
                            if ten_bit { 10 } else { 8 },
                            ten_bit,
                        );
                    }
                }

                // A planar slot is sampled by the CSC pass into the video image; the composite
                // then reads that, as on the native lane.
                #[cfg(windows)]
                Lane::D3d11(d, f) => {
                    if let (Some(v), true) = (&self.video, f.planes.is_some()) {
                        external_acquire_barrier(
                            &self.device,
                            self.cmd_buf,
                            f.image,
                            self.qfi,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags::FRAGMENT_SHADER,
                            vk::AccessFlags::SHADER_READ,
                        );
                        let (depth, msb_packed) = match d.format {
                            SlotFormat::P010 => (10, true),
                            _ => (8, false),
                        };
                        let extent = vk::Extent2D {
                            width: v.width,
                            height: v.height,
                        };
                        let target = CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent,
                        };
                        self.record_csc(false, target, [1.0, 1.0], d.color, depth, msb_packed);
                    }
                }

                // Image already on this device; layout and semaphore ride the frame.
                // Pool images are CONCURRENT across graphics+decode, so these are
                // layout transitions, not queue-family ownership transfers.
                Lane::Native(f) => {
                    if let Some(v) = &self.video {
                        let image = vk::Image::from_raw(f.image);
                        let decode_layout = match f.layout {
                            NativeVkLayout::DecodeDst => vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                            NativeVkLayout::DecodeDpb => vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
                        };
                        native_layer_barrier(
                            &self.device,
                            self.cmd_buf,
                            image,
                            f.layer,
                            decode_layout,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        );
                        let extent = vk::Extent2D {
                            width: v.width,
                            height: v.height,
                        };
                        // Depth/packing from the picture format (can change mid-stream).
                        // 8-bit math over P010 decodes and displays the wrong range.
                        // `uv_scale` is picture/coded so a taller decode pool does not show.
                        let (depth, msb_packed) = csc_depth_packing_or_8bit(f.vk_format);
                        let target = direct_target.unwrap_or(CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent,
                        });
                        self.record_csc(
                            false,
                            target,
                            [
                                f.width as f32 / f.coded_width as f32,
                                f.height as f32 / f.coded_height as f32,
                            ],
                            f.color,
                            depth,
                            msb_packed,
                        );
                        native_layer_barrier(
                            &self.device,
                            self.cmd_buf,
                            image,
                            f.layer,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            decode_layout,
                        );
                        native_wait =
                            Some((vk::Semaphore::from_raw(f.semaphore), f.semaphore_value));
                    }
                }

                // Planes already on this device and in GENERAL for fragment sampling.
                #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                Lane::Pyro(f) => {
                    if let Some(v) = &self.video {
                        let extent = vk::Extent2D {
                            width: v.width,
                            height: v.height,
                        };
                        // 10-bit planes hold MSB-packed codes, PQ or SDR.
                        let (depth, msb_packed) = if f.ten_bit { (10, true) } else { (8, false) };
                        let target = CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent,
                        };
                        self.record_csc(true, target, [1.0, 1.0], f.color, depth, msb_packed);
                    }
                }

                // Tightly packed (`CpuPlanarFrame`): leave `buffer_row_length` zero —
                // a stride here would be a second place for the layout to be wrong.
                Lane::Cpu(f) => {
                    if let (Some(offsets), Some(v), Some(s), Some(p)) =
                        (cpu_offsets, &self.video, &self.staging, &self.cpu_planes)
                    {
                        // Fresh images start UNDEFINED; later uploads start where the
                        // previous CSC pass left them (SHADER_READ_ONLY_OPTIMAL).
                        let from = if p.initialized {
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                        } else {
                            vk::ImageLayout::UNDEFINED
                        };
                        for (i, offset) in offsets.iter().enumerate() {
                            let (w, h) = f.plane_dims(i);
                            barrier(
                                &self.device,
                                self.cmd_buf,
                                p.images[i],
                                from,
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            );
                            let region = vk::BufferImageCopy::default()
                                .buffer_offset(*offset as u64)
                                .image_subresource(subresource_layers())
                                .image_extent(vk::Extent3D {
                                    width: w,
                                    height: h,
                                    depth: 1,
                                });
                            self.device.cmd_copy_buffer_to_image(
                                self.cmd_buf,
                                s.buffer,
                                p.images[i],
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                &[region],
                            );
                            barrier(
                                &self.device,
                                self.cmd_buf,
                                p.images[i],
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            );
                        }
                        let extent = vk::Extent2D {
                            width: v.width,
                            height: v.height,
                        };
                        // Always 8-bit, no MSB packing — R8 planes, whatever the stream
                        // signals. PQ tone-maps through shader mode 1, not 10-bit.
                        let target = direct_target.unwrap_or(CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent,
                        });
                        self.record_csc(true, target, [1.0, 1.0], f.color, 8, false);
                    }
                }

                // `Redraw` of a frame drawn direct: the same planes and push constants, no new
                // decode wait (the last real submit waited it) and no timeline signal (that value
                // is spent). The native frame returns to its decode layout as on a real frame.
                // Off the direct path (a new size or fit) the planes go to the video image first:
                // a direct frame never wrote it.
                Lane::Redraw => {
                    let target = direct_target.or_else(|| {
                        self.video.as_ref().map(|v| CscTarget::Video {
                            framebuffer: v.framebuffer,
                            extent: vk::Extent2D {
                                width: v.width,
                                height: v.height,
                            },
                        })
                    });
                    if let (Some(l), Some(target)) = (last, target) {
                        match l.src {
                            DirectSrc::Native => {
                                if let Some(Retired::NativeVk(f)) = &self.retired_hw {
                                    let image = vk::Image::from_raw(f.image);
                                    let decode_layout = match f.layout {
                                        NativeVkLayout::DecodeDst => {
                                            vk::ImageLayout::VIDEO_DECODE_DST_KHR
                                        }
                                        NativeVkLayout::DecodeDpb => {
                                            vk::ImageLayout::VIDEO_DECODE_DPB_KHR
                                        }
                                    };
                                    native_layer_barrier(
                                        &self.device,
                                        self.cmd_buf,
                                        image,
                                        f.layer,
                                        decode_layout,
                                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                                    );
                                    self.record_csc(
                                        false,
                                        target,
                                        l.uv_scale,
                                        l.color,
                                        l.depth,
                                        l.msb_packed,
                                    );
                                    native_layer_barrier(
                                        &self.device,
                                        self.cmd_buf,
                                        image,
                                        f.layer,
                                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                                        decode_layout,
                                    );
                                }
                            }
                            // Planes stay where the first pass left them, owned by this queue.
                            #[cfg(target_os = "linux")]
                            DirectSrc::Dmabuf => {
                                self.record_csc(
                                    false,
                                    target,
                                    l.uv_scale,
                                    l.color,
                                    l.depth,
                                    l.msb_packed,
                                );
                            }
                            DirectSrc::Cpu => {
                                self.record_csc(
                                    true,
                                    target,
                                    l.uv_scale,
                                    l.color,
                                    l.depth,
                                    l.msb_packed,
                                );
                            }
                        }
                    }
                }
            }

            let swap_layout = if direct.is_some() {
                // The direct pass drew the picture, and the letterbox when there is one.
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
            } else if let (Some(p), Some(v)) = (filtered, &self.video) {
                // The CSC pass leaves the video image in TRANSFER_SRC; the blit path and the
                // next `Redraw` expect it back there.
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                self.scale.record(
                    &self.device,
                    self.cmd_buf,
                    (v.width, v.height),
                    &p,
                    self.overlay_pipe.framebuffers[index as usize],
                    self.extent,
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                );
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
            } else {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                );
                // A picture that covers the swapchain needs no clear under it: skipping the
                // full-screen fill and its barrier is a whole pass saved on an iGPU.
                let covered = matches!(
                    (source, &placement),
                    (Some(_), Some(p))
                        if p.dst_x == 0
                            && p.dst_y == 0
                            && p.dst_w == self.extent.width
                            && p.dst_h == self.extent.height
                );
                if !covered {
                    self.device.cmd_clear_color_image(
                        self.cmd_buf,
                        swap_image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue {
                            float32: [0.0, 0.0, 0.0, 1.0],
                        },
                        &[subresource_range()],
                    );
                    // Clear and blit both write the swapchain image; transfer commands carry no
                    // implicit order. RDNA fast-clears DCC metadata beside the blit, and where
                    // the clear lands second the tile shows black (the AMD "equaliser" report).
                    barrier(
                        &self.device,
                        self.cmd_buf,
                        swap_image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    );
                }
                if let (Some((image, _, _)), Some(p)) = (source, placement) {
                    let corner = |x: f64, y: f64, z: i32| vk::Offset3D {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        z,
                    };
                    let blit = vk::ImageBlit::default()
                        .src_subresource(subresource_layers())
                        .src_offsets([
                            corner(p.src_x, p.src_y, 0),
                            corner(p.src_x + p.src_w, p.src_y + p.src_h, 1),
                        ])
                        .dst_subresource(subresource_layers())
                        .dst_offsets([
                            corner(f64::from(p.dst_x), f64::from(p.dst_y), 0),
                            corner(
                                f64::from(p.dst_x + p.dst_w),
                                f64::from(p.dst_y + p.dst_h),
                                1,
                            ),
                        ]);
                    // NEAREST is exact at 1:1 and whole-number scales; LINEAR is the fallback
                    // for a fractional scale the shader cannot take.
                    let filter = if crate::scale::needs_filter(&p) {
                        vk::Filter::LINEAR
                    } else {
                        vk::Filter::NEAREST
                    };
                    self.device.cmd_blit_image(
                        self.cmd_buf,
                        image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        swap_image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[blit],
                        filter,
                    );
                }
                vk::ImageLayout::TRANSFER_DST_OPTIMAL
            };
            // An HDR switch swaps in a fresh overlay pipe and leaves the swapchain to
            // `recreate_swapchain`, which keeps the old one while the window has no
            // extent (a display-topology flip). Until that recreate lands the pipe has
            // no framebuffers: present the video alone rather than index past them.
            let overlay =
                overlay.filter(|_| (index as usize) < self.overlay_pipe.framebuffers.len());
            if let Some(o) = overlay {
                // Skia flushed on this queue: same-layout barrier is execution
                // + memory only (cross-submit visibility).
                barrier(
                    &self.device,
                    self.cmd_buf,
                    o.image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    swap_layout,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
                self.device.cmd_begin_render_pass(
                    self.cmd_buf,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(self.overlay_pipe.render_pass)
                        .framebuffer(self.overlay_pipe.framebuffers[index as usize])
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: self.extent,
                        }),
                    vk::SubpassContents::INLINE,
                );
                self.device.cmd_bind_pipeline(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline,
                );
                self.device.cmd_set_viewport(
                    self.cmd_buf,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: self.extent.width as f32,
                        height: self.extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                self.device.cmd_set_scissor(
                    self.cmd_buf,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: self.extent,
                    }],
                );
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline_layout,
                    0,
                    &[self.overlay_pipe.desc_set],
                    &[],
                );
                self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(self.cmd_buf);
            } else {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    swap_layout,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                );
            }
            self.device.end_command_buffer(self.cmd_buf)?;
            // Next CPU upload must transition from SHADER_READ_ONLY_OPTIMAL.
            // Set here after record, before submit: a submit failure tears the
            // presenter down rather than re-recording.
            if let Some(p) = self.cpu_planes.as_mut() {
                p.initialized = true;
            }

            let render_sem = self.render_sems[index as usize];
            let cmd_bufs = [self.cmd_buf];
            let mut wait_sems = vec![self.acquire_sem];
            // The swapchain image is written by the blit (transfer) or by the direct,
            // scale and overlay passes (colour attachment): both wait the acquire.
            let mut wait_stages = vec![
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            ];
            let mut signal_sems = vec![render_sem];
            let mut wait_values = vec![0u64];
            let mut signal_values = vec![0u64];
            // Wait decode-complete at FRAGMENT_SHADER (`native_layer_barrier`
            // chain). Signal `value + 1` when reads and layout restore finish
            // (`mark_presented`). Per-image timelines keep value spaces private.
            if let Some((sem, value)) = &native_wait {
                wait_sems.push(*sem);
                wait_stages.push(vk::PipelineStageFlags::FRAGMENT_SHADER);
                wait_values.push(*value);
                signal_sems.push(*sem);
                signal_values.push(*value + 1);
            }
            // The VAAPI decode's fence, sampled at FRAGMENT_SHADER like the native lane.
            #[cfg(target_os = "linux")]
            if let Lane::Dmabuf(f) = &lane {
                for sem in &f.sync_sems {
                    wait_sems.push(*sem);
                    wait_stages.push(vk::PipelineStageFlags::FRAGMENT_SHADER);
                    wait_values.push(0);
                }
            }
            // With present timing the submit also signals `done_sem` with the id the
            // present below will carry: the waiter splits our GPU time from the compositor's.
            let timed = self.glass_active() && self.done_sem != vk::Semaphore::null();
            if timed {
                signal_sems.push(self.done_sem);
                signal_values.push(self.next_present_id + 1);
            }
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_values)
                .signal_semaphore_values(&signal_values);
            let mut submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&signal_sems);
            if native_wait.is_some() || timed {
                submit = submit.push_next(&mut timeline);
            }
            // Keyed mutex, key 0 both ways (decode writes under acquire(0)/release(0)
            // too), on every submit that reads a slot, `Redraw` included. Acquire orders
            // the read after the decoder's Blt or copy; release unblocks the ring slot.
            #[cfg(windows)]
            let keyed_mem;
            #[cfg(windows)]
            let keyed_keys = [0u64];
            #[cfg(windows)]
            let keyed_timeouts = [2000u32];
            #[cfg(windows)]
            let mut keyed_info;
            #[cfg(windows)]
            let lane_memory = match &lane {
                Lane::D3d11(_, f) => Some(f.memory),
                _ => None,
            };
            #[cfg(windows)]
            if let Some(memory) = lane_memory.or(slot.map(|(f, _, _)| f.memory)) {
                if keyed_mutex_on() {
                    keyed_mem = [memory];
                    keyed_info = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
                        .acquire_syncs(&keyed_mem)
                        .acquire_keys(&keyed_keys)
                        .acquire_timeouts(&keyed_timeouts)
                        .release_syncs(&keyed_mem)
                        .release_keys(&keyed_keys);
                    submit = submit.push_next(&mut keyed_info);
                }
            }
            let submit_started = std::time::Instant::now();
            let submitted = {
                // Queue external sync vs the pump's decode submits (`queue_lock`).
                let _q = self.queue_lock.guard();
                self.device.queue_submit(self.queue, &[submit], self.fence)
            };
            self.last_submit_us = submit_started.elapsed().as_micros() as u32;
            submitted?;
            self.submitted = true;
            self.acquired = None;
            // A real frame from any other lane ends the D3D11 picture; `Redraw` keeps it.
            #[cfg(windows)]
            {
                self.retained_slot = slot;
            }
            // Park until the fence proves the reads done (next present's wait, or
            // Drop). A D3D11 slot stays in the import cache. A `Redraw` keeps the parked
            // frame: it read it again.
            if !redraw {
                self.retired_hw = match lane {
                    #[cfg(target_os = "linux")]
                    Lane::Dmabuf(f) => Some(Retired::Dmabuf(f)),
                    // Submit enqueued `value + 1` — `mark_presented` so the decoder waits
                    // that write-back. Failed submit never reaches here (no phantom signal).
                    // Park until the fence; Drop sends the release token.
                    Lane::Native(mut f) => {
                        f.guard.mark_presented();
                        Some(Retired::NativeVk(f))
                    }
                    _ => None,
                };
            }

            let swapchains = [self.swapchain];
            let indices = [index];
            let present_sems = [render_sem];
            // Monotonic present id for `PresentTimer`'s `vkWaitForPresentKHR`.
            let ids = [self.next_present_id + 1];
            let mut pid_info = vk::PresentIdKHR::default().present_ids(&ids);
            let mut pid2_info = super::setup::present_wait2::PresentId2::new(&ids);
            // A stamp only while the swapchain's result queue has room: asking into a
            // full one fails the present.
            let ask = self
                .timing
                .as_ref()
                .filter(|t| self.timing_armed && t.may_ask());
            let timing_info = ask.map_or_else(
                || super::timing_ext::TimingInfo::stamps(0, 0),
                |t| t.request(),
            );
            let timings_info = super::timing_ext::TimingsInfo::new(&timing_info);
            let mut present_info = vk::PresentInfoKHR::default()
                .wait_semaphores(&present_sems)
                .swapchains(&swapchains)
                .image_indices(&indices);
            // The id names the `done_sem` value either way; only present-wait carries it
            // to the driver, in the struct of the generation the waiter runs on.
            let glass = self.glass_active();
            if glass {
                self.next_present_id += 1;
            }
            // The compositor stamps the commit this present makes. Only a waiter's sample
            // can take it; without one the answers would pile up unread.
            #[cfg(target_os = "linux")]
            if let Some(fb) = self.feedback.as_mut().filter(|_| glass && !redraw) {
                fb.request(self.next_present_id);
            }
            if self.present_id2 {
                // Hand-rolled structs: the chain is empty here, so they are the whole chain.
                if ask.is_some() {
                    pid2_info.p_next = (&timings_info) as *const _ as *const std::ffi::c_void;
                }
                present_info.p_next = (&pid2_info) as *const _ as *const std::ffi::c_void;
            } else if self.present_timer.is_some() {
                present_info = present_info.push_next(&mut pid_info);
            }
            let present_started = std::time::Instant::now();
            // Same queue external-sync as the submit. Scoped tightly: OUT_OF_DATE
            // re-enters the lock via `recreate_swapchain`'s queue drain. The swapchain
            // guard comes first, so a waiter slice never holds off decode submits.
            let present_res = {
                let _swapchain = self.present_timer.as_ref().map(|t| t.swapchain_guard());
                let _q = self.queue_lock.guard();
                self.swap_d.queue_present(self.queue, &present_info)
            };
            self.last_present_us = present_started.elapsed().as_micros() as u32;
            let asked = ask.is_some();
            match present_res {
                Ok(_) => {
                    // A failed present's id may never signal — claim it only on Ok.
                    if self.glass_active() {
                        self.last_presented = Some((self.swapchain, self.next_present_id));
                    }
                    self.timing_asked = asked;
                    if let Some(t) = self.timing.as_ref().filter(|_| asked) {
                        t.note_asked();
                    }
                    Ok(Presented::Shown)
                }
                // The driver counted its result queue differently: stop asking, and take
                // a swapchain whose queue is empty.
                Err(super::timing_ext::QUEUE_FULL) => {
                    tracing::warn!("present stamp queue full; presenting without stamps");
                    self.timing = None;
                    self.timing_armed = false;
                    self.recreate_swapchain(window)?;
                    Ok(Presented::Stale)
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR)
                | Err(vk::Result::ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT) => {
                    self.recreate_swapchain(window)?;
                    Ok(Presented::Stale)
                }
                Err(e) => Err(e).context("vkQueuePresentKHR"),
            }
        }
    }

    /// Import or view the frame's planes. Runs before acquire, so a rejected import
    /// fails before this present consumes the acquire semaphore.
    fn import<'a>(&mut self, input: FrameInput<'a>) -> Result<Lane<'a>> {
        Ok(match input {
            FrameInput::Redraw => Lane::Redraw,
            FrameInput::Cpu(f) => Lane::Cpu(f),
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => {
                let hw = self
                    .hw
                    .as_mut()
                    .context("hardware frame without dmabuf support")?;
                Lane::Dmabuf(dmabuf::get_or_import(
                    &self.instance,
                    self.pdev,
                    &self.device,
                    &hw.ext_mem_fd,
                    &mut hw.modifier_cache,
                    &mut hw.imports,
                    hw.sync.as_mut(),
                    d,
                )?)
            }
            #[cfg(windows)]
            FrameInput::D3d11(d) => {
                let hw = self
                    .hw_win
                    .as_mut()
                    .context("D3D11 frame without win32 import support")?;
                let started = std::time::Instant::now();
                let imported = hw
                    .imports
                    .get_or_import(&self.device, &hw.ext_mem_win32, &d)?;
                self.last_import_us = started.elapsed().as_micros() as u32;
                Lane::D3d11(d, imported)
            }
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => Lane::Pyro(f),
            // Same device; decoder already made the per-plane views — no import,
            // no view create, nothing that can fail here.
            FrameInput::NativeVk(f) => Lane::Native(f),
        })
    }

    /// Size the video image for this frame and point the CSC pass at its planes. Only
    /// after the fence wait, which leaves the descriptor sets idle. Returns the staged
    /// software planes' buffer offsets.
    fn bind(&mut self, lane: &Lane) -> Result<Option<[usize; 3]>> {
        if let Some((width, height)) = lane.video_size() {
            self.ensure_video_image(width, height)?;
        }
        match lane {
            Lane::Redraw => {}
            Lane::Cpu(f) => {
                let offsets = self.stage_frame(f)?;
                let views = self
                    .cpu_planes
                    .as_ref()
                    .context("software frame without plane images")?
                    .views;
                self.csc_planar.bind_planes_planar(
                    &self.device,
                    views,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                return Ok(Some(offsets));
            }
            #[cfg(target_os = "linux")]
            Lane::Dmabuf(f) => self
                .csc
                .bind_planes(&self.device, f.luma_view, f.chroma_view),
            // A planar D3D11 slot goes through the CSC pass into the video image, like the
            // native lane; an RGB slot needs no video image.
            #[cfg(windows)]
            Lane::D3d11(_, imported) => {
                if let Some(planes) = imported.planes {
                    self.csc.bind_planes(&self.device, planes[0], planes[1]);
                }
            }
            Lane::Native(f) => {
                // UV-scale crop is origin-only; a nonzero origin would show the wrong window.
                if f.crop_x != 0 || f.crop_y != 0 {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            crop_x = f.crop_x,
                            crop_y = f.crop_y,
                            "native frame carries a non-origin conformance crop — the UV \
                             scale only handles origin crops; picture offset expected"
                        );
                    }
                }
                // Decoder-owned plane views.
                self.csc.bind_planes(
                    &self.device,
                    vk::ImageView::from_raw(f.plane_views[0]),
                    vk::ImageView::from_raw(f.plane_views[1]),
                );
            }
            // Decode leaves planes in GENERAL; CPU uploads arrive in SHADER_READ_ONLY_OPTIMAL.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Lane::Pyro(f) => self.csc_planar.bind_planes_planar(
                &self.device,
                f.views.map(vk::ImageView::from_raw),
                vk::ImageLayout::GENERAL,
            ),
        }
        Ok(None)
    }

    /// YCbCr→RGBA CSC: a fullscreen triangle with the CICP push-constant rows, drawn where
    /// `target` says. `planar` picks the 3-plane pipe (PyroWave, software I420) over the
    /// NV12 one (dmabuf, Vulkan Video, D3D11 slots). `uv_scale` is picture/surface per
    /// axis: `[1.0, 1.0]` unless the bound planes are a decode pool larger than the
    /// picture (see the shader's `params.zw`).
    ///
    /// # Safety
    /// `self.cmd_buf` must be recording; the pass's descriptor set must point at live
    /// plane views.
    unsafe fn record_csc(
        &self,
        planar: bool,
        target: CscTarget,
        uv_scale: [f32; 2],
        color: pf_client_core::video::ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) {
        let pass = if planar { &self.csc_planar } else { &self.csc };
        let full = |extent: vk::Extent2D| vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let (render_pass, pipeline, framebuffer, area, rect) = match target {
            CscTarget::Video {
                framebuffer,
                extent,
            } => (
                pass.render_pass,
                pass.pipeline,
                framebuffer,
                full(extent),
                full(extent),
            ),
            CscTarget::Direct {
                framebuffer,
                surface,
                rect,
                clear,
            } => (
                if clear {
                    self.direct.clear
                } else {
                    self.direct.keep
                },
                if planar {
                    self.direct.planar
                } else {
                    self.direct.nv12
                },
                framebuffer,
                if clear { full(surface) } else { rect },
                rect,
            ),
        };
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        // SAFETY: `cmd_buf` is recording (`# Safety` on this fn). Pipelines, layouts, and
        // desc_sets are owned here; plane views were bound this present or the last real one.
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd_buf,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(framebuffer)
                    .render_area(area)
                    .clear_values(&clear_values),
                vk::SubpassContents::INLINE,
            );
            self.device
                .cmd_bind_pipeline(self.cmd_buf, vk::PipelineBindPoint::GRAPHICS, pipeline);
            self.device.cmd_set_viewport(
                self.cmd_buf,
                0,
                &[vk::Viewport {
                    x: rect.offset.x as f32,
                    y: rect.offset.y as f32,
                    width: rect.extent.width as f32,
                    height: rect.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(self.cmd_buf, 0, &[rect]);
            self.device.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                pass.pipeline_layout,
                0,
                &[pass.desc_set],
                &[],
            );
            let rows = csc_rows(color, depth, msb_packed);
            // Mode 1 = PQ→SDR tonemap (PQ stream, no HDR10 surface); mode 0
            // passes the transfer through (SDR, or PQ onto the HDR10 swapchain).
            let mode = if color.is_pq() && !self.hdr_active {
                1.0f32
            } else {
                0.0
            };
            let mut pc = [0f32; 16];
            pc[..12].copy_from_slice(rows.as_flattened());
            pc[12] = mode;
            pc[13] = tonemap_peak();
            pc[14] = uv_scale[0];
            pc[15] = uv_scale[1];
            let words = pc.map(f32::to_ne_bytes);
            let bytes = words.as_flattened();
            self.device.cmd_push_constants(
                self.cmd_buf,
                pass.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes,
            );
            self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.cmd_buf);
        }
    }
}

/// CSC `(bit depth, MSB-packed)` for a decoded picture's `VkFormat`, or `None`.
///
/// Stream property, not codec: the frame carries [`NativeVkFrame::vk_format`].
/// 8-bit two-plane → depth 8, unpacked. 10-bit two-plane `3PACK16` → depth 10,
/// MSB-packed (10 bits in the MSBs of 16): a UNORM16 sample reads
/// `code·64/65535`; `csc_rows` applies `65535/65472`. 8-bit math on those
/// expands range and the PQ curve wrong.
///
/// Chroma subsampling is not here. The shader samples both planes in
/// normalized coordinates and disables quarter-texel 4:2:0 siting when chroma
/// is full width. Pinned by the table test below.
fn csc_depth_packing(raw: RawVkFormat) -> Option<(u8, bool)> {
    [
        (vk::Format::G8_B8R8_2PLANE_420_UNORM, (8, false)),
        (vk::Format::G8_B8R8_2PLANE_444_UNORM, (8, false)),
        (
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            (10, true),
        ),
        (
            vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16,
            (10, true),
        ),
    ]
    .into_iter()
    .find_map(|(f, dp)| (f.as_raw() == raw.0).then_some(dp))
}

/// [`csc_depth_packing`] plus 8-bit fallback. pf-vkdecode refuses an unmapped
/// picture format before a session exists; unreachable is not impossible, so
/// warn once per format.
///
/// Per format, not once per process: a session can renegotiate mid-stream, and
/// a single latch would silence a later different unmapped format.
fn csc_depth_packing_or_8bit(raw: RawVkFormat) -> (u8, bool) {
    csc_depth_packing(raw).unwrap_or_else(|| {
        use std::sync::Mutex;
        static WARNED: Mutex<Vec<RawVkFormat>> = Mutex::new(Vec::new());
        let mut seen = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&raw) {
            seen.push(raw);
            tracing::warn!(
                vk_format = raw.0,
                "decoded picture in a format the CSC pass has no depth mapping for — \
                 rendering it as 8-bit, which is wrong if it is not"
            );
        }
        (8, false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8-bit math on a 10-bit `3PACK16` picture expands range and the PQ curve wrong.
    #[test]
    fn csc_depth_and_packing_follow_the_pictures_format() {
        let d = |fmt: vk::Format| csc_depth_packing(RawVkFormat(fmt.as_raw()));
        assert_eq!(d(vk::Format::G8_B8R8_2PLANE_420_UNORM), Some((8, false)));
        assert_eq!(d(vk::Format::G8_B8R8_2PLANE_444_UNORM), Some((8, false)));
        // 10-bit, MSB-packed into 16. The packing flag recovers `code/1023` from
        // a UNORM16 sample.
        assert_eq!(
            d(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16),
            Some((10, true))
        );
        assert_eq!(
            d(vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16),
            Some((10, true))
        );
        // Formats the two-binding CSC pass cannot sample (3-plane 4:4:4, 16-bit)
        // have no mapping, not a plausible default.
        assert_eq!(d(vk::Format::G8_B8_R8_3PLANE_444_UNORM), None);
        assert_eq!(d(vk::Format::G16_B16R16_2PLANE_444_UNORM), None);
        assert_eq!(csc_depth_packing(RawVkFormat(0)), None);
        assert_eq!(csc_depth_packing(RawVkFormat(-1)), None);
        // Fallback is 8-bit, not a panic: a wrong picture beats a dead session.
        assert_eq!(csc_depth_packing_or_8bit(RawVkFormat(0)), (8, false));
    }

    /// Pins this table against [`pf_client_core::video::native_picture_formats`]
    /// (forwards `pf_vkdecode::OUTPUT_FORMATS`). A new decoder format would
    /// otherwise hit the 8-bit fallback while the table test above stayed green.
    /// No converse: pf-vkdecode need not produce every format CSC can sample.
    #[test]
    fn every_format_the_native_decoder_can_deliver_has_colour_math_here() {
        let produced = pf_client_core::video::native_picture_formats();
        assert!(!produced.is_empty(), "the vocabulary must not be empty");
        for raw in produced {
            assert!(
                csc_depth_packing(raw).is_some(),
                "pf-vkdecode delivers vk_format {} and the CSC pass has no depth \
                 mapping for it — it would render as 8-bit",
                raw.0
            );
        }
    }
}
