//! Swapchain recreate, resize, and SDR↔HDR10 flip.

use super::setup::pick_formats;
use super::{OverlayPipe, Presenter};
use anyhow::{anyhow, Context as _, Result};
use ash::vk;

/// NVIDIA proprietary still fails `vkCreateSwapchainKHR` after a successful
/// enumerate — it wants `vkAcquireDrmDisplayEXT`, which SDL's KMSDRM surface
/// does not. Empty on other backends.
fn kmsdrm_swapchain_hint() -> String {
    let kmsdrm = std::env::var("SDL_VIDEODRIVER").is_ok_and(|v| v.eq_ignore_ascii_case("kmsdrm"));
    if !kmsdrm {
        return String::new();
    }
    let card = std::env::var("PUNKTFUNK_DRM_CARD").unwrap_or_else(|_| "unset".into());
    format!(
        " — under SDL_VIDEODRIVER=kmsdrm (PUNKTFUNK_DRM_CARD={card}). Check, in order: the card \
         has a CONNECTED connector (`cat /sys/class/drm/card*-*/status`); nothing else holds DRM \
         master on it (a running compositor does — pin another card with PUNKTFUNK_DRM_CARD=<n>); \
         and the driver is Mesa. NVIDIA's proprietary direct-display path is known to fail here \
         even as root with a display Vulkan can enumerate."
    )
}

impl Presenter {
    pub fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        self.quiesce_own()?;
        // An image acquired ahead of a present belongs to the swapchain that goes now.
        self.acquired = None;
        // Presentation-engine semaphore waits finish here. A fence wait proves
        // only OUR submit (VUID-vkDestroySemaphore-05149 /
        // VUID-vkDestroySwapchainKHR-01282). Decode submits share `queue_lock`.
        {
            let _q = self.queue_lock.guard();
            // SAFETY: `queue` is owned here; `queue_lock` is held so no concurrent submit.
            unsafe { self.device.queue_wait_idle(self.queue) }
                .context("vkQueueWaitIdle (swapchain recreate)")?;
        }

        // SAFETY: `pdev` and `surface` are live handles owned by this presenter.
        let caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        let (pw, ph) = window.size_in_pixels();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: pw.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: ph.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            // Minimized: keep the old swapchain. Presents return OUT_OF_DATE
            // and land back here once the window has a size.
            return Ok(());
        }
        let mut min_images = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            min_images = min_images.min(caps.max_image_count);
        }

        // Drain before create: `oldSwapchain` is externally synchronised, so
        // the driver may retire it under a parked `vkWaitForPresentKHR`.
        // Bounded by the waiter's 250 ms cap.
        if let Some(t) = &self.present_timer {
            t.drain();
        }
        // Last present belonged to the dying swapchain. `note_presented` is
        // the only producer and shares this thread, so nothing enqueues
        // between this drain and the destroy below.
        self.last_presented = None;
        let old = self.swapchain;
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(min_images)
            .image_format(self.format.format)
            .image_color_space(self.format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // TRANSFER_DST is clear + blit; COLOR_ATTACHMENT keeps the overlay
            // pass from forcing a swapchain-rebuild contract change.
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            .clipped(true)
            .old_swapchain(old);
        // SAFETY: `device`/`surface` are live; `info` is a local that outlives the call.
        let swapchain = unsafe { self.swap_d.create_swapchain(&info, None) }.map_err(|e| {
            anyhow!(
                "vkCreateSwapchainKHR: {e} ({}x{}, {:?} / {:?}, {:?}, {min_images} images){}",
                extent.width,
                extent.height,
                self.format.format,
                self.format.color_space,
                self.present_mode,
                kmsdrm_swapchain_hint()
            )
        })?;
        // Quiesce covered our cmd bufs, queue drain the presentation-engine
        // semaphore waits, present-timer drain the last waiter — nothing
        // still names these objects.
        let (overlay_views, overlay_framebuffers) = self.overlay_pipe.take_targets();
        // SAFETY: quiesce, `queue_wait_idle`, and present-timer drain above;
        // GPU idle on these views, framebuffers, semaphores, and `old`.
        unsafe {
            for fb in overlay_framebuffers {
                self.device.destroy_framebuffer(fb, None);
            }
            for v in overlay_views {
                self.device.destroy_image_view(v, None);
            }
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            if old != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(old, None);
            }
        }
        self.swapchain = swapchain;
        // SAFETY: `swapchain` was created above and is owned here.
        self.images = unsafe { self.swap_d.get_swapchain_images(swapchain) }?;
        self.extent = extent;
        self.overlay_pipe.rebuild_targets(
            &self.device,
            &self.images,
            self.format.format,
            extent,
        )?;

        for _ in 0..self.images.len() {
            // SAFETY: `device` is live; create-info is a local that outlives the call.
            self.render_sems.push(unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }
        tracing::debug!(
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            "swapchain (re)created"
        );
        // HDR metadata is per-swapchain: a rebuilt HDR10 swapchain needs it
        // pushed again (`set_hdr_mode` into HDR10 also lands here).
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
        Ok(())
    }

    /// Swapchain is HDR10/PQ, not a PQ stream tone-mapped onto SDR.
    /// User-facing "HDR" indicators should report this, not stream signalling.
    pub fn hdr_active(&self) -> bool {
        self.hdr_active
    }

    /// The swapchain holds 10 bits a channel (SDR or HDR10): an overlay drawn in 8 would band.
    pub fn ten_bit(&self) -> bool {
        matches!(
            self.format.format,
            vk::Format::A2B10G10R10_UNORM_PACK32 | vk::Format::A2R10G10B10_UNORM_PACK32
        )
    }

    /// Drop back to the SDR swapchain. No-op unless HDR10 is live.
    ///
    /// Console UI is SDR and composites into whatever swapchain the last
    /// stream left. [`Presenter::present`] only flips on a frame with colour
    /// signalling; a UI-only present is `FrameInput::Redraw` (none). After a
    /// PQ session the UI would otherwise write into the HDR10 swapchain and
    /// sRGB mid-tones would emit as PQ code points.
    pub fn leave_hdr(&mut self, window: &sdl3::video::Window) -> Result<()> {
        if !self.hdr_active {
            return Ok(());
        }
        // Do not flip while minimized: `recreate_swapchain` keeps the old
        // swapchain at zero extent, but `set_hdr_mode` would already have
        // rebuilt CSC/overlay against SDR, mismatched to live HDR10 images.
        if self.extent.width == 0 || self.extent.height == 0 {
            return Ok(());
        }
        tracing::info!("stream over — leaving HDR10 so the console UI composites as SDR");
        self.set_hdr_mode(window, false)
    }

    /// Host ST.2086 + content-light from the 0xCE plane.
    pub fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        if self.hdr_meta == Some(meta) {
            return;
        }
        self.hdr_meta = Some(meta);
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
    }

    /// `vkSetHdrMetadataEXT` with the host 0xCE values, or a generic HDR10
    /// baseline until that plane arrives. Compositors gate HDR-app signalling
    /// on this call — HDR10 colorspace alone leaves gamescope treating the
    /// app as SDR. No-op without the extension.
    fn apply_hdr_metadata(&self) {
        let Some(ext) = &self.hdr_metadata_d else {
            return;
        };
        // Same generic baseline as the Windows presenter: BT.2020 + D65,
        // 1000-nit mastering, MaxCLL 1000 / MaxFALL 400.
        let m = self.hdr_meta.unwrap_or(punktfunk_core::quic::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 1,
            max_cll: 1000,
            max_fall: 400,
        });
        // HDR10 SEI fixed-point: chromaticity 1/50000, luminance 0.0001
        // cd/m², primaries in ST.2086 G,B,R order. Vulkan wants 0..1 xy,
        // whole nits, primaries named R/G/B.
        let xy = |p: [u16; 2]| vk::XYColorEXT {
            x: p[0] as f32 / 50_000.0,
            y: p[1] as f32 / 50_000.0,
        };
        let [g, b, r] = m.display_primaries;
        let md = vk::HdrMetadataEXT::default()
            .display_primary_red(xy(r))
            .display_primary_green(xy(g))
            .display_primary_blue(xy(b))
            .white_point(xy(m.white_point))
            .max_luminance(m.max_display_mastering_luminance as f32 / 10_000.0)
            .min_luminance(m.min_display_mastering_luminance as f32 / 10_000.0)
            .max_content_light_level(m.max_cll as f32)
            .max_frame_average_light_level(m.max_fall as f32);
        // SAFETY: `swapchain` is live and owned here; `md` is a local.
        unsafe { ext.set_hdr_metadata(&[self.swapchain], &[md]) };
        tracing::debug!(from_host = self.hdr_meta.is_some(), "HDR metadata pushed");
    }
    /// SDR↔HDR10 flip. The video intermediate ([`super::VIDEO_FORMAT`]) keeps its format; only
    /// its content, mapped for the old mode, is dropped. A driver that refuses the HDR10
    /// swapchain it advertised loses `hdr10_format` for good: back to SDR, where PQ frames
    /// tone-map.
    pub(super) fn set_hdr_mode(&mut self, window: &sdl3::video::Window, on: bool) -> Result<()> {
        let target = if on {
            self.hdr10_format.expect("caller checked availability")
        } else {
            // `self.format` currently holds the HDR pairing; the SDR pick never changed.
            pick_formats(&self.surface_i, self.pdev, self.surface, false)?.0
        };
        tracing::info!(hdr = on, format = ?target, "switching presentation mode");
        self.quiesce_own()?;
        if let Some(v) = self.video.take() {
            // SAFETY: `quiesce_own` above; GPU idle on this video image.
            unsafe {
                self.device.destroy_framebuffer(v.framebuffer, None);
                self.device.destroy_image_view(v.view, None);
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
        }
        // New overlay pipe for the new format. Old views/framebuffers are
        // only in our cmd bufs — fence quiesce makes destroy safe here;
        // the swapchain rides `recreate_swapchain` below.
        let mut old_pipe = std::mem::replace(
            &mut self.overlay_pipe,
            OverlayPipe::new(&self.device, target.format, on)?,
        );
        let (overlay_views, overlay_framebuffers) = old_pipe.take_targets();
        // SAFETY: fence quiesce above; these views/framebuffers are only in our cmd bufs.
        unsafe {
            for fb in overlay_framebuffers {
                self.device.destroy_framebuffer(fb, None);
            }
            for v in overlay_views {
                self.device.destroy_image_view(v, None);
            }
        }
        old_pipe.destroy(&self.device);
        // The scale pass renders into the swapchain format too; fence quiesce above.
        self.scale.destroy(&self.device);
        self.scale = crate::scale::ScalePass::new(&self.device, target.format)?;
        self.format = target;
        self.hdr_active = on;
        match self.recreate_swapchain(window) {
            Err(e) if on => {
                tracing::warn!(error = %format!("{e:#}"),
                    "HDR10 swapchain refused — staying SDR and tone-mapping PQ");
                self.hdr10_format = None;
                self.set_hdr_mode(window, false)
            }
            r => r,
        }
    }
}
