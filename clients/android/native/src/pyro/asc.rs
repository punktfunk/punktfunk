//! Present through `ASurfaceControl`: the CSC draws into an `AHardwareBuffer` the compositor
//! takes as a layer, not into a swapchain image. Nothing waits on the CPU for a free buffer,
//! SurfaceFlinger scales the picture to the view, and each buffer comes back with a release
//! fence the next draw into it waits on, on the GPU.
//!
//! [`BUFFERS`] buffers at the picture's size: one on screen, one or two the compositor still
//! holds, one to draw. With none free the picture is dropped, as a swapchain acquire timeout is.

use super::device::PyroDevice;
use super::present::FRAMES;
use crate::decode::surface_control::{self, Layer, PresentComplete};
use crate::decode::DecodeEvent;
use crate::pyro_ring::Ring;
use anyhow::{anyhow, Context as _, Result};
use ash::vk;
use ndk::hardware_buffer::{
    HardwareBuffer, HardwareBufferDesc, HardwareBufferRef, HardwareBufferUsage,
};
use ndk::hardware_buffer_format::HardwareBufferFormat;
use ndk::native_window::NativeWindow;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc};

/// Buffers in the ring.
const BUFFERS: usize = 4;

/// The format the CSC draws: non-linear sRGB values in 8-bit UNORM, as the swapchain path.
pub(super) const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// `ADATASPACE_SRGB`: BT.709 primaries, sRGB transfer, full range. The CSC tone-maps PQ to it.
const DATASPACE_SRGB: i32 = 142_671_872;

/// A ring slot's buffer and the Vulkan objects over it; its state is in [`Ring`].
struct Slot {
    buffer: HardwareBufferRef,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    fb: vk::Framebuffer,
    /// SurfaceFlinger's release fence, imported once it is known; the next draw waits on it.
    release: vk::Semaphore,
}

pub(super) struct AscTarget {
    device: ash::Device,
    ahb_d: ash::android::external_memory_android_hardware_buffer::Device,
    sem_fd: ash::khr::external_semaphore_fd::Device,
    layer: Layer,
    /// Indexed as `ring`'s slots.
    slots: Vec<Slot>,
    ring: Ring,
    /// Per frame in flight: signalled by the draw, exported as the transaction's acquire fence.
    done: [vk::Semaphore; FRAMES],
    ev_tx: mpsc::Sender<DecodeEvent>,
    ev_rx: mpsc::Receiver<DecodeEvent>,
    frame_rate: f32,
}

/// The slot a draw goes to: its framebuffer, and the release fence to wait on first.
pub(super) struct Draw {
    pub(super) slot: usize,
    pub(super) fb: vk::Framebuffer,
    pub(super) image: vk::Image,
    pub(super) extent: vk::Extent2D,
    pub(super) wait: Option<vk::Semaphore>,
}

impl AscTarget {
    /// A layer over `window` voting `frame_rate`, or `None` where `ASurfaceControl` is missing.
    /// `force_switch` lets the vote switch the panel non-seamlessly.
    pub(super) fn new(
        dev: &PyroDevice,
        window: &NativeWindow,
        surface_size: Arc<AtomicU64>,
        src_crop: Arc<AtomicU64>,
        frame_rate: f32,
        force_switch: bool,
    ) -> Result<Option<AscTarget>> {
        let Some(layer) = Layer::create(window, surface_size, src_crop, force_switch) else {
            return Ok(None);
        };
        let mut done = [vk::Semaphore::null(); FRAMES];
        for sem in &mut done {
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            // SAFETY: device live; the create-info is a local; `Drop` destroys the semaphore.
            *sem = unsafe {
                dev.device.create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut export),
                    None,
                )
            }?;
        }
        let (ev_tx, ev_rx) = mpsc::channel();
        Ok(Some(AscTarget {
            device: dev.device.clone(),
            ahb_d: ash::android::external_memory_android_hardware_buffer::Device::new(
                &dev.base.instance,
                &dev.device,
            ),
            sem_fd: ash::khr::external_semaphore_fd::Device::new(&dev.base.instance, &dev.device),
            layer,
            slots: Vec::new(),
            ring: Ring::default(),
            done,
            ev_tx,
            ev_rx,
            frame_rate,
        }))
    }

    /// Whether the ring must be rebuilt for a `width`×`height` picture.
    pub(super) fn outgrown(&self, width: u32, height: u32) -> bool {
        self.ring.outgrown(width, height)
    }

    /// Rebuild the ring at `width`×`height`. On an error the ring holds no slot and is still
    /// outgrown, so the next picture rebuilds it again.
    ///
    /// # Safety
    /// The device is idle, and `render_pass` draws [`FORMAT`].
    pub(super) unsafe fn rebuild(
        &mut self,
        render_pass: vk::RenderPass,
        width: u32,
        height: u32,
    ) -> Result<()> {
        // SAFETY: fn contract: nothing in flight uses the old slots.
        unsafe { self.destroy_slots() };
        self.ring.clear();
        for _ in 0..BUFFERS {
            // SAFETY: fn contract.
            let slot = unsafe { self.make_slot(render_pass, width, height) }?;
            self.slots.push(slot);
        }
        self.ring.fill(BUFFERS, width, height);
        log::info!(
            "pyro: presenting {BUFFERS} buffers of {width}x{height} through ASurfaceControl"
        );
        Ok(())
    }

    /// One ring slot: an `AHardwareBuffer` imported as the framebuffer's image.
    ///
    /// # Safety
    /// As [`Self::rebuild`].
    unsafe fn make_slot(&self, render_pass: vk::RenderPass, w: u32, h: u32) -> Result<Slot> {
        let buffer = HardwareBuffer::allocate(HardwareBufferDesc {
            width: w,
            height: h,
            layers: 1,
            format: HardwareBufferFormat::R8G8B8A8_UNORM,
            usage: HardwareBufferUsage::GPU_COLOR_OUTPUT
                | HardwareBufferUsage::GPU_SAMPLED_IMAGE
                | HardwareBufferUsage::COMPOSER_OVERLAY,
            stride: 0,
        })
        .map_err(|e| anyhow!("AHardwareBuffer_allocate: {e:?}"))?;
        let ahb = buffer.as_ptr().cast::<vk::AHardwareBuffer>();
        let mut slot = Slot {
            buffer,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            fb: vk::Framebuffer::null(),
            release: vk::Semaphore::null(),
        };
        // SAFETY: fn contract; every create-info is a local; on error `drop_slot` frees what was
        // made, null handles being no-ops.
        let made = unsafe {
            (|| -> Result<()> {
                let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
                let mut props =
                    vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
                self.ahb_d
                    .get_android_hardware_buffer_properties(ahb, &mut props)?;
                let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(
                    vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID,
                );
                slot.image = self.device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(FORMAT)
                        .extent(vk::Extent3D {
                            width: w,
                            height: h,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED)
                        .push_next(&mut external),
                    None,
                )?;
                let type_index = (0..32)
                    .find(|i| props.memory_type_bits & (1 << i) != 0)
                    .context("no memory type imports this AHardwareBuffer")?;
                let mut import = vk::ImportAndroidHardwareBufferInfoANDROID::default().buffer(ahb);
                let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(slot.image);
                slot.memory = self.device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(props.allocation_size)
                        .memory_type_index(type_index)
                        .push_next(&mut import)
                        .push_next(&mut dedicated),
                    None,
                )?;
                self.device.bind_image_memory(slot.image, slot.memory, 0)?;
                slot.view = self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(slot.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(FORMAT)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            level_count: 1,
                            layer_count: 1,
                            ..Default::default()
                        }),
                    None,
                )?;
                let attachments = [slot.view];
                slot.fb = self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&attachments)
                        .width(w)
                        .height(h)
                        .layers(1),
                    None,
                )?;
                slot.release = self
                    .device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
                Ok(())
            })()
        };
        if let Err(e) = made {
            // SAFETY: the slot's handles were made above and never used.
            unsafe { self.drop_slot(&slot) };
            return Err(e);
        }
        Ok(slot)
    }

    /// Apply the completions that arrived: each frees the slot its transaction replaced, behind
    /// the release fence SurfaceFlinger handed back for it.
    pub(super) fn drain(&mut self) {
        while let Ok(ev) = self.ev_rx.try_recv() {
            let DecodeEvent::PresentComplete(PresentComplete {
                seq,
                prev_release_fence,
                ..
            }) = ev
            else {
                continue;
            };
            let Some(i) = self.ring.complete(seq) else {
                continue;
            };
            if let Some(fence) = prev_release_fence {
                // SAFETY: `release` is this slot's idle semaphore and `fence` a live sync fd.
                let imported = unsafe {
                    self.sem_fd.import_semaphore_fd(
                        &vk::ImportSemaphoreFdInfoKHR::default()
                            .semaphore(self.slots[i].release)
                            .flags(vk::SemaphoreImportFlags::TEMPORARY)
                            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                            .fd(fence.as_raw_fd()),
                    )
                };
                match imported {
                    // The semaphore owns the fd now; a failed import leaves it ours to close.
                    Ok(()) => {
                        std::mem::forget(fence);
                        self.ring.arm(i);
                    }
                    Err(e) => {
                        log::warn!(
                            "pyro: release fence not imported ({e}); buffer reused unfenced"
                        );
                    }
                }
            }
        }
    }

    /// The slot the next picture draws into, or `None` while the compositor holds every one.
    pub(super) fn next_draw(&mut self) -> Option<Draw> {
        let (slot, armed) = self.ring.next_draw()?;
        let s = &self.slots[slot];
        let (width, height) = self.ring.extent();
        Some(Draw {
            slot,
            fb: s.fb,
            image: s.image,
            extent: vk::Extent2D { width, height },
            wait: armed.then_some(s.release),
        })
    }

    /// The semaphore the draw for frame-in-flight `frame` signals.
    pub(super) fn done(&self, frame: usize) -> vk::Semaphore {
        self.done[frame]
    }

    /// Hand the drawn `slot` to the layer, fenced by the draw's `done(frame)` signal. `false`:
    /// the transaction was not made and the slot stays free.
    pub(super) fn present(&mut self, slot: usize, frame: usize) -> Result<bool> {
        // SAFETY: the draw that signals `done[frame]` is submitted, so the export has a pending
        // signal to copy; the returned fd is ours.
        let fd = unsafe {
            self.sem_fd.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(self.done[frame])
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            )
        }
        .map_err(|e| anyhow!("vkGetSemaphoreFdKHR: {e}"))?;
        // A sync fd of -1 means already signalled.
        // SAFETY: a non-negative fd from the export is a fresh descriptor we own.
        let mut fence = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
        let seq = self.ring.next_seq();
        let (width, height) = self.ring.extent();
        let shown = surface_control::present(
            &mut [&mut self.layer],
            &self.slots[slot].buffer,
            width as i32,
            height as i32,
            &mut fence,
            0,
            DATASPACE_SRGB,
            None,
            self.frame_rate,
            seq,
            &self.ev_tx,
        );
        if shown {
            self.ring.shown(slot, seq);
        }
        Ok(shown)
    }

    /// # Safety
    /// Nothing in flight uses the slots.
    unsafe fn destroy_slots(&mut self) {
        for slot in std::mem::take(&mut self.slots) {
            // SAFETY: fn contract.
            unsafe { self.drop_slot(&slot) };
        }
    }

    /// # Safety
    /// Nothing in flight uses the slot.
    unsafe fn drop_slot(&self, slot: &Slot) {
        // SAFETY: fn contract; null handles are no-ops. The `AHardwareBuffer` reference drops
        // with the slot after its memory, which held its own.
        unsafe {
            self.device.destroy_semaphore(slot.release, None);
            self.device.destroy_framebuffer(slot.fb, None);
            self.device.destroy_image_view(slot.view, None);
            self.device.destroy_image(slot.image, None);
            self.device.free_memory(slot.memory, None);
        }
    }
}

impl Drop for AscTarget {
    /// The owner idles the device first ([`super::present::Present`]'s `Drop`).
    fn drop(&mut self) {
        self.layer.hide();
        // SAFETY: the device is idle (see above) and every handle was made here.
        unsafe {
            self.destroy_slots();
            for sem in self.done {
                self.device.destroy_semaphore(sem, None);
            }
        }
    }
}
