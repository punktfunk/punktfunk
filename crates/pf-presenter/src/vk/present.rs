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
use super::{FrameInput, Presented, Presenter, Retired};
use crate::csc::csc_rows;
#[cfg(target_os = "linux")]
use crate::dmabuf::{self, HwFrame};
use crate::overlay::OverlayFrame;
use anyhow::{Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
#[cfg(windows)]
use pf_client_core::video::SlotFormat;
use pf_client_core::video::{NativeVkFrame, NativeVkLayout, RawVkFormat};

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
        // FIFO without present-wait: the queue is policed here rather than by blocking.
        // Probe the previous submit's fence and take the image ahead of time; either
        // one not ready means a refresh has not passed yet. Probed before `input` is
        // consumed so the frame can go back to the store whole.
        let nonblocking = self.needs_glass_gate()
            && self.present_timer.is_none()
            && !matches!(input, FrameInput::Redraw);
        if nonblocking {
            // SAFETY: `fence` is owned here; a status query is always legal.
            if self.submitted && !unsafe { self.device.get_fence_status(self.fence) }? {
                return Ok(Presented::Busy(input));
            }
            if self.acquired.is_none() {
                // SAFETY: `swapchain`/`acquire_sem` are owned; the last submit that waited
                // `acquire_sem` is fence-complete (checked above), so it is not pending.
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
                        return Ok(Presented::Busy(input));
                    }
                    Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
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
        #[cfg(windows)]
        let redraw = matches!(input, FrameInput::Redraw);
        // Import/view before acquire: a reject must fail before this present
        // consumes the acquire semaphore.
        #[cfg(target_os = "linux")]
        let mut hw_frame: Option<HwFrame> = None;
        #[cfg(windows)]
        let mut win_frame: Option<(
            pf_client_core::video::D3d11Frame,
            crate::d3d11::Imported,
        )> = None;
        let mut native_frame: Option<NativeVkFrame> = None;
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        let mut pyro_frame: Option<pf_client_core::video_pyrowave::PyroWavePlanarFrame> = None;
        // Non-CPU real frame: software plane images are dead (freed after the
        // fence). `Redraw` is not one — it re-blits retained video.
        let mut hw_lane = false;
        let cpu_frame = match input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some(f),
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => {
                let hw = self
                    .hw
                    .as_mut()
                    .context("hardware frame without dmabuf support")?;
                hw_frame = Some(dmabuf::get_or_import(
                    &self.instance,
                    self.pdev,
                    &self.device,
                    &hw.ext_mem_fd,
                    &mut hw.modifier_cache,
                    &mut hw.imports,
                    hw.sync.as_mut(),
                    d,
                )?);
                hw_lane = true;
                None
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
                win_frame = Some((d, imported));
                hw_lane = true;
                None
            }
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => {
                pyro_frame = Some(f);
                hw_lane = true;
                None
            }
            // Same device; decoder already made the per-plane views — no import,
            // no view create, nothing that can fail here.
            FrameInput::NativeVk(f) => {
                native_frame = Some(f);
                hw_lane = true;
                None
            }
        };

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
        if let Some(old) = self.retired_hw.take() {
            old.destroy(&self.device);
        }
        // Nothing is in flight past the wait above: imports of a superseded ring
        // generation can go now.
        #[cfg(windows)]
        if let (Some((d, _)), Some(hw)) = (&win_frame, self.hw_win.as_mut()) {
            hw.imports.retire_stale(&self.device, d.generation);
        }
        // Same for a rebuilt VAAPI pool; another lane's frame means that decoder is
        // gone, and its cached imports pin the pool's memory until they go too.
        #[cfg(target_os = "linux")]
        if let Some(hw) = self.hw.as_mut() {
            match &hw_frame {
                Some(f) => hw.imports.retire_stale(&self.device, f.generation()),
                None if hw_lane && !hw.imports.is_empty() => hw.imports.destroy_all(&self.device),
                None => {}
            }
        }
        // First fence wait is the first moment the software plane images are
        // unreferenced. Hardware lane will not sample them again.
        if hw_lane {
            if let Some(p) = self.cpu_planes.take() {
                tracing::debug!("freeing the software rung's plane images (hardware lane)");
                p.destroy(&self.device);
            }
        }

        let cpu_offsets = match cpu_frame {
            Some(f) => Some(self.stage_frame(f)?),
            None => None,
        };
        #[cfg(target_os = "linux")]
        if let Some(f) = &hw_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            // Descriptor set idle: fence wait above.
            self.csc
                .bind_planes(&self.device, f.luma_view, f.chroma_view);
        }
        // A planar D3D11 slot goes through the CSC pass into the video image, like the
        // native lane; an RGB slot needs no video image. Descriptor set idle: fence wait above.
        #[cfg(windows)]
        if let Some((f, imported)) = &win_frame {
            if let Some(planes) = imported.planes {
                if self
                    .video
                    .as_ref()
                    .is_none_or(|v| v.width != f.width || v.height != f.height)
                {
                    self.rebuild_video_image(f.width, f.height)?;
                    tracing::info!(width = f.width, height = f.height, "video image (re)built");
                }
                self.csc.bind_planes(&self.device, planes[0], planes[1]);
            }
        }
        if let Some(f) = &native_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
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
            // Decoder-owned plane views; fence wait above makes the set rebindable.
            self.csc.bind_planes(
                &self.device,
                vk::ImageView::from_raw(f.plane_views[0]),
                vk::ImageView::from_raw(f.plane_views[1]),
            );
        }
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        if let Some(f) = &pyro_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            // Decode leaves planes in GENERAL; CPU uploads arrive in SHADER_READ_ONLY_OPTIMAL.
            self.csc_planar.bind_planes_planar(
                &self.device,
                f.views.map(vk::ImageView::from_raw),
                vk::ImageLayout::GENERAL,
            );
        }
        if cpu_offsets.is_some() {
            // Descriptor set idle: fence wait above.
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
        }
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
        let slot = win_frame
            .as_ref()
            .filter(|(_, f)| f.planes.is_none())
            .map(|(d, f)| (*f, d.width, d.height))
            .or(self.retained_slot.filter(|_| redraw));
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
                let (device, mem_props) = (&self.device, &self.mem_props);
                self.scale.prepare(device, v.height, &p, v.view, |reqs| {
                    allocate(
                        device,
                        mem_props,
                        reqs,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )
                })?;
                Some(p)
            }
            _ => None,
        };
        if let (Some((_, w, h)), Some(p)) = (source, placement) {
            // A D3D11 RGB ring slot is imported TRANSFER_SRC only, so a fractional scale of it
            // stays a bilinear blit until the import also asks for SAMPLED.
            let path = if filtered.is_some() {
                "filtered"
            } else if crate::scale::needs_filter(&p) {
                "bilinear blit"
            } else {
                "exact blit"
            };
            self.log_placement(w, h, &p, path);
        }

        let acquire_started = std::time::Instant::now();
        // SAFETY: `swapchain` and `acquire_sem` are owned here. Fence wait above
        // completed the last submit that waited `acquire_sem`, so it is not pending.
        // An image taken by the non-blocking probe above is used as is.
        let acquired = match self.acquired.take() {
            Some(index) => Ok((index, false)),
            None => unsafe {
                self.swap_d.acquire_next_image(
                    self.swapchain,
                    u64::MAX,
                    self.acquire_sem,
                    vk::Fence::null(),
                )
            },
        };
        let (index, _suboptimal) = match acquired {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // Acquire failed: GPU never saw the import; destroy it here.
                #[cfg(target_os = "linux")]
                if let Some(f) = hw_frame {
                    f.destroy(&self.device);
                }
                self.recreate_swapchain(window)?;
                return Ok(Presented::Stale);
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        };
        self.last_acquire_us = acquire_started.elapsed().as_micros() as u32;
        let swap_image = self.images[index as usize];

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

            // CSC render pass leaves the video image in TRANSFER_SRC for the blit.
            #[cfg(target_os = "linux")]
            if let (Some(f), Some(v)) = (&hw_frame, &self.video) {
                for view_image in [f.luma_image(), f.chroma_image()] {
                    foreign_acquire_barrier(&self.device, self.cmd_buf, view_image, self.qfi);
                }
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                let ten_bit = f.is_p010();
                // Imported images span the full exported (coded) extent; the
                // CSC pass crops them to the visible picture.
                self.record_csc(
                    v.framebuffer,
                    extent,
                    f.uv_scale(),
                    f.color,
                    if ten_bit { 10 } else { 8 },
                    ten_bit,
                );
            }

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
            // A planar slot is sampled by the CSC pass into the video image; the composite
            // then reads that, as on the native lane.
            #[cfg(windows)]
            if let (Some((d, f)), Some(v)) = (&win_frame, &self.video) {
                if f.planes.is_some() {
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
                    self.record_csc(
                        v.framebuffer,
                        extent,
                        [1.0, 1.0],
                        d.color,
                        depth,
                        msb_packed,
                    );
                }
            }

            // Image already on this device; layout and semaphore ride the frame.
            // Pool images are CONCURRENT across graphics+decode, so these are
            // layout transitions, not queue-family ownership transfers.
            let mut native_wait: Option<(vk::Semaphore, u64)> = None;
            if let (Some(f), Some(v)) = (&native_frame, &self.video) {
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
                self.record_csc(
                    v.framebuffer,
                    extent,
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
                native_wait = Some((vk::Semaphore::from_raw(f.semaphore), f.semaphore_value));
            }

            // Planes already on this device and in GENERAL for fragment sampling.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            if let (Some(f), Some(v)) = (&pyro_frame, &self.video) {
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                // 10-bit planes hold MSB-packed codes, PQ or SDR.
                let (depth, msb_packed) = if f.ten_bit { (10, true) } else { (8, false) };
                self.record_csc_planar(v.framebuffer, extent, f.color, depth, msb_packed);
            }

            // Tightly packed (`CpuPlanarFrame`): leave `buffer_row_length` zero —
            // a stride here would be a second place for the layout to be wrong.
            if let (Some(f), Some(offsets), Some(v), Some(s), Some(p)) = (
                cpu_frame,
                cpu_offsets,
                &self.video,
                &self.staging,
                &self.cpu_planes,
            ) {
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
                self.record_csc_planar(v.framebuffer, extent, f.color, 8, false);
            }

            let swap_layout = if let (Some(p), Some(v)) = (filtered, &self.video) {
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
            let mut wait_stages = vec![vk::PipelineStageFlags::TRANSFER];
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
            if let Some(f) = &hw_frame {
                for sem in &f.sync_sems {
                    wait_sems.push(*sem);
                    wait_stages.push(vk::PipelineStageFlags::FRAGMENT_SHADER);
                    wait_values.push(0);
                }
            }
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_values)
                .signal_semaphore_values(&signal_values);
            let mut submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&signal_sems);
            if native_wait.is_some() {
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
            if let Some(memory) = win_frame
                .as_ref()
                .map(|(_, f)| f.memory)
                .or(slot.map(|(f, _, _)| f.memory))
            {
                // `PUNKTFUNK_D3D11_NO_MUTEX=1` skips acquire/release (torn frames;
                // debugging only).
                if std::env::var_os("PUNKTFUNK_D3D11_NO_MUTEX").is_none() {
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
            // A real frame from any other lane ends the D3D11 picture; `Redraw` keeps it.
            #[cfg(windows)]
            {
                self.retained_slot = slot;
            }
            // Park until the fence proves the reads done (next present's wait, or
            // Drop). At most one of hw_frame / native_frame is set; a D3D11 slot stays
            // in the import cache.
            self.retired_hw = None;
            #[cfg(target_os = "linux")]
            if let Some(f) = hw_frame.take() {
                self.retired_hw = Some(Retired::Dmabuf(f));
            }
            // Submit enqueued `value + 1` — `mark_presented` so the decoder waits
            // that write-back. Failed submit never reaches here (no phantom signal).
            // Park until the fence; Drop sends the release token.
            if let Some(mut f) = native_frame.take() {
                f.guard.mark_presented();
                self.retired_hw = Some(Retired::NativeVk(f));
            }

            let swapchains = [self.swapchain];
            let indices = [index];
            let present_sems = [render_sem];
            // Monotonic present id for `PresentTimer`'s `vkWaitForPresentKHR`.
            let ids = [self.next_present_id + 1];
            let mut pid_info = vk::PresentIdKHR::default().present_ids(&ids);
            let mut present_info = vk::PresentInfoKHR::default()
                .wait_semaphores(&present_sems)
                .swapchains(&swapchains)
                .image_indices(&indices);
            if self.present_timer.is_some() {
                self.next_present_id += 1;
                present_info = present_info.push_next(&mut pid_info);
            }
            let present_started = std::time::Instant::now();
            // Same queue external-sync as the submit. Scoped tightly: OUT_OF_DATE
            // re-enters the lock via `recreate_swapchain`'s queue drain.
            let present_res = {
                let _q = self.queue_lock.guard();
                self.swap_d.queue_present(self.queue, &present_info)
            };
            self.last_present_us = present_started.elapsed().as_micros() as u32;
            match present_res {
                Ok(_) => {
                    // A failed present's id may never signal — claim it only on Ok.
                    if self.present_timer.is_some() {
                        self.last_presented = Some((self.swapchain, self.next_present_id));
                    }
                    Ok(Presented::Shown)
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate_swapchain(window)?;
                    Ok(Presented::Stale)
                }
                Err(e) => Err(e).context("vkQueuePresentKHR"),
            }
        }
    }

    /// NV12→RGBA CSC into the video image: fullscreen triangle, CICP push-constant
    /// rows. Shared by the dmabuf and Vulkan-Video paths — only the bound plane
    /// views and `uv_scale` differ.
    ///
    /// `extent` is the picture (framebuffer size). `uv_scale` is picture/surface
    /// per axis: `[1.0, 1.0]` unless the bound planes are a decode pool larger
    /// than the picture. See the shader's `params.zw`.
    ///
    /// # Safety
    /// `self.cmd_buf` must be recording; the CSC descriptor set must point at
    /// live plane views.
    unsafe fn record_csc(
        &self,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        uv_scale: [f32; 2],
        color: pf_client_core::video::ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) {
        // SAFETY: `cmd_buf` is recording (`# Safety` on this fn). CSC pipeline,
        // layout, and desc_set are owned here; plane views were bound this present.
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd_buf,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.csc.render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                self.csc.pipeline,
            );
            self.device.cmd_set_viewport(
                self.cmd_buf,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                self.cmd_buf,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                self.csc.pipeline_layout,
                0,
                &[self.csc.desc_set],
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
            let peak = std::env::var("PUNKTFUNK_TONEMAP_PEAK")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(4.9); // ≈1000 nits / 203-nit reference
            let mut pc = [0f32; 16];
            pc[..12].copy_from_slice(rows.as_flattened());
            pc[12] = mode;
            pc[13] = peak;
            pc[14] = uv_scale[0];
            pc[15] = uv_scale[1];
            let words = pc.map(f32::to_ne_bytes);
            let bytes = words.as_flattened();
            self.device.cmd_push_constants(
                self.cmd_buf,
                self.csc.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes,
            );
            self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.cmd_buf);
        }
    }

    /// [`record_csc`] on the planar (3-plane) pass — PyroWave decode output and
    /// the software rung's uploaded I420.
    ///
    /// `depth`/`msb_packed` are the producer's, never inferred from colour.
    /// Pyrowave couples 10-bit to PQ by negotiation; the software rung is 8-bit
    /// regardless. Treating PQ as 10-bit MSB-packed over an 8-bit plane samples
    /// at quarter scale.
    unsafe fn record_csc_planar(
        &self,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        color: pf_client_core::video::ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) {
        let planar = &self.csc_planar;
        // SAFETY: caller holds `cmd_buf` recording. Planar pipeline, layout, and
        // desc_set are owned here; plane views were bound this present.
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd_buf,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(planar.render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                planar.pipeline,
            );
            self.device.cmd_set_viewport(
                self.cmd_buf,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                self.cmd_buf,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                planar.pipeline_layout,
                0,
                &[planar.desc_set],
                &[],
            );
            let rows = csc_rows(color, depth, msb_packed);
            // Mode 1 = PQ→SDR tonemap; mode 0 passes the transfer through.
            let mode = if color.is_pq() && !self.hdr_active {
                1.0f32
            } else {
                0.0
            };
            let peak = std::env::var("PUNKTFUNK_TONEMAP_PEAK")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(4.9); // ≈1000 nits / 203-nit reference
            let mut pc = [0f32; 16];
            pc[..12].copy_from_slice(rows.as_flattened());
            pc[12] = mode;
            pc[13] = peak;
            let words = pc.map(f32::to_ne_bytes);
            let bytes = words.as_flattened();
            self.device.cmd_push_constants(
                self.cmd_buf,
                planar.pipeline_layout,
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
