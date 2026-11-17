//! D3D11 shared-texture → Vulkan import (Windows): presenter half of
//! D3D11VA (`pf_client_core::video_d3d11`). Each frame names the NT handle of
//! a shareable ring slot: single-plane RGB the video processor filled (BGRA8
//! sRGB or RGB10A2 PQ), composited straight into the swapchain; or a two-plane
//! NV12/P010 copy of the decoded picture, sampled per plane by the CSC pass.
//! Imported as one `VkImage` (`VK_KHR_external_memory_win32`, dedicated allocation).
//!
//! Slots stay imported across frames ([`ImportCache`], keyed by ring
//! generation and handle); a superseded generation is destroyed after the
//! fence that could still read it. A fenced ring's `written`/`read` pair imports
//! once per generation as timeline semaphores the submit waits and signals;
//! otherwise both sides acquire/release the DXGI keyed mutex
//! (`VK_KHR_win32_keyed_mutex`) on key 0. The decoder ring owns the NT handles;
//! a driver reject is a clean error and the caller demotes.

use crate::knobs::HwFault;
use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{
    D3d11Frame, SharedSlot, SliceGuard, SlotFence, SlotFormat, SlotHandle,
};
use std::sync::Arc;

/// Required at device creation. Missing either, `supports_d3d11()` is false.
pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 2] = [
    ash::khr::external_memory_win32::NAME,
    ash::khr::win32_keyed_mutex::NAME,
];

/// Enabled beside [`DEVICE_EXTENSIONS`] when [`ImportSupport::fence`] holds; never required.
pub const FENCE_EXTENSION: &std::ffi::CStr = ash::khr::external_semaphore_win32::NAME;

/// Spec-required probe for an image `import` could create. An unsupported
/// external image is undefined.
fn format_importable(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
) -> bool {
    let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
    let fmt_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .flags(flags)
        .push_next(&mut ext_info);
    let mut ext_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
    // SAFETY: `instance` is live; `fmt_info` and `props` are locals that outlive the call.
    unsafe { instance.get_physical_device_image_format_properties2(pdev, &fmt_info, &mut props) }
        .is_ok()
        && ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}

/// What this device imports from D3D11. BGRA8 gates D3D11VA; RGB10A2 gates PQ
/// pass-through on the RGB ring; NV12/P010 gate the planar ring; `fence` gates the
/// ring's shared fences (`d3d11_fence`).
#[derive(Clone, Copy)]
pub struct ImportSupport {
    pub bgra8: bool,
    pub rgb10: bool,
    pub nv12: bool,
    pub p010: bool,
    pub fence: bool,
}

/// Planar slots on this vendor? A planar D3D11 import loses the Vulkan device on NVIDIA
/// however it is consumed, and on Intel at the first frame, so both stay on the RGB ring.
/// `knob`, when set, overrides it both ways.
pub fn planar_allowed(vendor_id: u32, knob: Option<bool>) -> bool {
    knob.unwrap_or(!matches!(vendor_id, 0x10DE | 0x8086))
}

/// Ask the driver, once at setup, which D3D11 slot formats import here.
pub fn import_supported(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> ImportSupport {
    let copy = vk::ImageUsageFlags::TRANSFER_SRC;
    let none = vk::ImageCreateFlags::empty();
    let bgra8 = format_importable(instance, pdev, vk::Format::B8G8R8A8_UNORM, copy, none);
    let rgb10 = format_importable(
        instance,
        pdev,
        vk::Format::A2B10G10R10_UNORM_PACK32,
        copy,
        none,
    );
    // The CSC pass samples per-plane views, which a two-plane image allows only with
    // MUTABLE_FORMAT.
    let planar = |format| {
        format_importable(
            instance,
            pdev,
            format,
            vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
        )
    };
    let nv12 = planar(vk::Format::G8_B8R8_2PLANE_420_UNORM);
    let p010 = planar(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16);
    let fence = fence_importable(instance, pdev);
    tracing::info!(
        bgra8,
        rgb10,
        nv12,
        p010,
        fence,
        "D3D11 texture → Vulkan import support"
    );
    ImportSupport {
        bgra8,
        rgb10,
        nv12,
        p010,
        fence,
    }
}

/// Whether a shared D3D11 fence imports here as a timeline semaphore: the device offers
/// [`FENCE_EXTENSION`] and the driver reports the handle type importable.
fn fence_importable(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> bool {
    // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
    let offered = unsafe { instance.enumerate_device_extension_properties(pdev) }.is_ok_and(|e| {
        e.iter()
            .any(|e| e.extension_name_as_c_str() == Ok(FENCE_EXTENSION))
    });
    if !offered {
        return false;
    }
    let mut timeline =
        vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
        .push_next(&mut timeline);
    let mut props = vk::ExternalSemaphoreProperties::default();
    // SAFETY: `instance` is live and exposes the 1.1 core query; `info` and `props` are
    // locals that outlive the call.
    unsafe { instance.get_physical_device_external_semaphore_properties(pdev, &info, &mut props) };
    props
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
}

/// A fenced slot's `written` value and the `read` value of its last read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotReads {
    pub written: u64,
    pub read: u64,
}

/// One more read of a fenced slot: the submit waits `written` and signals the next value of
/// `reads`, which becomes the slot's last `read`. A `Redraw` takes one too, so the slot goes
/// back to the decoder only after its last read.
pub fn next_read(reads: &mut u64, slot: &mut SlotReads) -> (u64, u64) {
    *reads += 1;
    slot.read = *reads;
    (slot.written, *reads)
}

/// The fenced slot a `Redraw` may read again, held until a later frame's submit replaces it.
pub struct Parked {
    pub guard: SliceGuard,
    pub reads: SlotReads,
}

impl Parked {
    /// Hand the slot back at its last read. Only once no pending submit reads it.
    pub fn release(self) {
        self.guard.release(self.reads.read);
    }
}

/// One imported slot, what a submit names. The cache owns the objects.
#[derive(Clone, Copy)]
pub struct Imported {
    pub image: vk::Image,
    /// The submit's keyed-mutex acquire/release info names this allocation.
    pub memory: vk::DeviceMemory,
    /// Luma and chroma views of a planar slot for the CSC pass; `None` on the RGB ring.
    pub planes: Option<[vk::ImageView; 2]>,
    /// The slot's ring generation's fences; `None` on the keyed-mutex path.
    pub fences: Option<FencePair>,
}

/// A fenced ring generation's `written` and `read` fences as timeline semaphores.
#[derive(Clone, Copy)]
pub struct FencePair {
    pub written: vk::Semaphore,
    pub read: vk::Semaphore,
}

struct Entry {
    generation: u32,
    handle: isize,
    imported: Imported,
    /// Keeps the NT handle open while the import lives, so `handle` cannot be reused.
    _keep: Arc<SlotHandle>,
}

struct FenceEntry {
    generation: u32,
    pair: FencePair,
    /// The fences' NT handles, open while their semaphores live.
    _keep: [Arc<SlotHandle>; 2],
}

/// Imported ring slots: six per generation, imported on first sight, and a fenced
/// generation's fence pair. Nothing here is destroyed while a submit may use it —
/// [`ImportCache::retire_stale`] runs after the in-flight fence and
/// [`ImportCache::destroy_all`] after a device wait-idle.
#[derive(Default)]
pub struct ImportCache {
    entries: Vec<Entry>,
    fences: Vec<FenceEntry>,
}

impl ImportCache {
    /// The slot's import, made on the first frame from `(generation, handle)`, with its
    /// generation's fences when the frame is fenced (`ext_sem` is then required). An
    /// injected `fault` fails every import.
    pub fn get_or_import(
        &mut self,
        device: &ash::Device,
        ext_mem_win32: &ash::khr::external_memory_win32::Device,
        ext_sem: Option<&ash::khr::external_semaphore_win32::Device>,
        frame: &SharedSlot,
        fault: Option<HwFault>,
    ) -> Result<Imported> {
        let handle = frame.handle.raw();
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| e.generation == frame.generation && e.handle == handle)
        {
            return Ok(e.imported);
        }
        let fences = match &frame.fence {
            Some(fence) => Some(self.fences_of(device, ext_sem, frame.generation, fence)?),
            None => None,
        };
        let imported = Imported {
            fences,
            ..import(device, ext_mem_win32, frame, fault)?
        };
        self.entries.push(Entry {
            generation: frame.generation,
            handle,
            imported,
            _keep: frame.handle.clone(),
        });
        Ok(imported)
    }

    /// Generation `generation`'s fence pair, imported on its first fenced frame.
    fn fences_of(
        &mut self,
        device: &ash::Device,
        ext_sem: Option<&ash::khr::external_semaphore_win32::Device>,
        generation: u32,
        fence: &SlotFence,
    ) -> Result<FencePair> {
        if let Some(e) = self.fences.iter().find(|e| e.generation == generation) {
            return Ok(e.pair);
        }
        let ext_sem = ext_sem.context("fenced D3D11 frame on a device without fence import")?;
        let written = import_fence(device, ext_sem, &fence.written_fence)
            .context("import the ring's written fence")?;
        let read = match import_fence(device, ext_sem, &fence.read_fence) {
            Ok(read) => read,
            Err(e) => {
                // SAFETY: `written` was created above and no submit has named it.
                unsafe { device.destroy_semaphore(written, None) };
                return Err(e).context("import the ring's read fence");
            }
        };
        let pair = FencePair { written, read };
        self.fences.push(FenceEntry {
            generation,
            pair,
            _keep: [fence.written_fence.clone(), fence.read_fence.clone()],
        });
        Ok(pair)
    }

    /// Destroy every import of a generation other than `generation`; `true` if any slot went.
    /// Call only after the in-flight fence: a ring rebuild retires its slots and fences while
    /// the last frame of the old generation may still be on the GPU.
    pub fn retire_stale(&mut self, device: &ash::Device, generation: u32) -> bool {
        let (keep, stale): (Vec<_>, Vec<_>) = self
            .entries
            .drain(..)
            .partition(|e| e.generation == generation);
        self.entries = keep;
        let retired = !stale.is_empty();
        for e in stale {
            // SAFETY: the caller's fence wait; no submit references these objects.
            unsafe { destroy(device, e.imported) };
        }
        let (keep, stale): (Vec<_>, Vec<_>) = self
            .fences
            .drain(..)
            .partition(|e| e.generation == generation);
        self.fences = keep;
        // SAFETY: the caller's fence wait; no pending submit waits or signals these.
        unsafe { destroy_fences(device, stale) };
        retired
    }

    /// Call only after a device wait-idle (`Presenter::drop`).
    pub fn destroy_all(&mut self, device: &ash::Device) {
        for e in self.entries.drain(..) {
            // SAFETY: the caller's wait-idle; the GPU is done with every entry.
            unsafe { destroy(device, e.imported) };
        }
        // SAFETY: the caller's wait-idle; no submit is pending on these.
        unsafe { destroy_fences(device, self.fences.drain(..)) };
    }
}

/// # Safety
/// No pending submit waits or signals any semaphore of `entries`.
unsafe fn destroy_fences(device: &ash::Device, entries: impl IntoIterator<Item = FenceEntry>) {
    for sem in entries
        .into_iter()
        .flat_map(|e| [e.pair.written, e.pair.read])
    {
        // SAFETY: per this fn's contract; the cache created each and drops it once.
        unsafe { device.destroy_semaphore(sem, None) };
    }
}

/// One shared D3D11 fence as a timeline semaphore. A Win32 handle import does not take
/// the NT handle: the ring's [`SlotHandle`] still closes it.
fn import_fence(
    device: &ash::Device,
    ext_sem: &ash::khr::external_semaphore_win32::Device,
    handle: &SlotHandle,
) -> Result<vk::Semaphore> {
    let mut timeline =
        vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
    // SAFETY: `timeline` is a local that outlives the call; the semaphore is owned here.
    let sem = unsafe {
        device.create_semaphore(
            &vk::SemaphoreCreateInfo::default().push_next(&mut timeline),
            None,
        )
    }
    .context("create a timeline semaphore")?;
    let info = vk::ImportSemaphoreWin32HandleInfoKHR::default()
        .semaphore(sem)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
        .handle(handle.raw() as vk::HANDLE);
    // SAFETY: `sem` was created above and no submit has named it; `info.handle` is the
    // ring's live NT handle, which the frame holds open.
    if let Err(e) = unsafe { ext_sem.import_semaphore_win32_handle(&info) } {
        // SAFETY: `sem` was created in this call and no submit has named it.
        unsafe { device.destroy_semaphore(sem, None) };
        return Err(e).context("vkImportSemaphoreWin32HandleKHR");
    }
    Ok(sem)
}

/// # Safety
/// The GPU must be idle on `imported`.
unsafe fn destroy(device: &ash::Device, imported: Imported) {
    // SAFETY: per this fn's contract; the cache created every handle and drops each once.
    unsafe {
        for view in imported.planes.into_iter().flatten() {
            device.destroy_image_view(view, None);
        }
        device.destroy_image(imported.image, None);
        device.free_memory(imported.memory, None);
    }
}

/// Luma and chroma views of a two-plane image, in the per-plane `formats`.
fn plane_views(
    device: &ash::Device,
    image: vk::Image,
    formats: [vk::Format; 2],
) -> Result<[vk::ImageView; 2]> {
    let view = |format, aspect| {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // SAFETY: `image` is live and bound, created MUTABLE_FORMAT with a two-plane format
        // whose plane `aspect` matches `format`; `info` is a local that outlives the call.
        unsafe { device.create_image_view(&info, None) }
    };
    let luma = view(formats[0], vk::ImageAspectFlags::PLANE_0).context("create luma view")?;
    match view(formats[1], vk::ImageAspectFlags::PLANE_1) {
        Ok(chroma) => Ok([luma, chroma]),
        Err(e) => {
            // SAFETY: `luma` was created above and nothing has recorded it yet.
            unsafe { device.destroy_image_view(luma, None) };
            Err(e).context("create chroma view")
        }
    }
}

/// The ring slot this backend imports. A pool slice is the D3D11 backend's; the error
/// demotes like a failed import.
pub fn shared_slot(frame: D3d11Frame) -> Result<SharedSlot> {
    match frame {
        D3d11Frame::Shared(slot) => Ok(slot),
        D3d11Frame::Slice(_) => bail!("D3D11 pool slices need the D3D11 presenter"),
    }
}

/// Import one ring slot. A driver reject is a clean error; the caller demotes.
fn import(
    device: &ash::Device,
    ext_mem_win32: &ash::khr::external_memory_win32::Device,
    frame: &SharedSlot,
    fault: Option<HwFault>,
) -> Result<Imported> {
    // Test hook: fault every import so demotion is exercisable without a broken driver.
    if fault == Some(HwFault::Import) {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    // DXGI R10G10B10A2 matches Vulkan A2B10G10R10_PACK32 (R in the low bits). NV12/P010
    // map to the two-plane formats; the CSC pass samples them through per-plane views.
    let (mp_format, plane_formats) = match frame.format {
        SlotFormat::Bgra8 => (vk::Format::B8G8R8A8_UNORM, None),
        SlotFormat::Rgb10a2 => (vk::Format::A2B10G10R10_UNORM_PACK32, None),
        SlotFormat::Nv12 => (
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            Some([vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]),
        ),
        SlotFormat::P010 => (
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            Some([
                vk::Format::R10X6_UNORM_PACK16,
                vk::Format::R10X6G10X6_UNORM_2PACK16,
            ]),
        ),
    };
    // RGB slots are only blitted; planar slots are sampled per plane. Exactly what
    // `import_supported` asked the driver about.
    let (usage, flags) = if plane_formats.is_some() {
        (
            vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
        )
    } else {
        (
            vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageCreateFlags::empty(),
        )
    };
    let handle_type = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

    let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
    // SAFETY: `external_info` is a local that outlives the call; the returned handle is owned here.
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .push_next(&mut external_info)
                .image_type(vk::ImageType::TYPE_2D)
                .format(mp_format)
                .extent(vk::Extent3D {
                    width: frame.width,
                    height: frame.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .flags(flags)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .with_context(|| {
        format!(
            "create {}x{} {mp_format:?} external image",
            frame.width, frame.height
        )
    })?;

    let result = (|| {
        let handle = frame.handle.raw() as vk::HANDLE;
        let mut handle_props = vk::MemoryWin32HandlePropertiesKHR::default();
        // SAFETY: `handle` is the decoder ring's live NT handle (the frame holds it open);
        // `handle_props` is a local that outlives the call.
        unsafe {
            ext_mem_win32.get_memory_win32_handle_properties(handle_type, handle, &mut handle_props)
        }
        .context("vkGetMemoryWin32HandlePropertiesKHR")?;
        // SAFETY: `image` was created above and has not been destroyed.
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & handle_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for the D3D11 texture")?;

        // Import does not take NT-handle ownership; the ring's last `SlotHandle` closes it.
        let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(handle_type)
            .handle(handle);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        // SAFETY: `import_info` and `dedicated` are locals that outlive the call.
        // `import_info.handle` is the decoder ring's live NT handle, not owned here.
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .push_next(&mut import_info)
                    .push_next(&mut dedicated)
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("import D3D11 texture memory")?;
        // SAFETY: `image` and `memory` were created above and are still owned here.
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: `memory` was allocated in this call and never bound, so the GPU is idle on it.
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();
    let memory = match result {
        Ok(memory) => memory,
        Err(e) => {
            // SAFETY: `image` was created in this call and never bound, so the GPU is idle on it.
            unsafe { device.destroy_image(image, None) };
            return Err(e);
        }
    };
    let planes = match plane_formats
        .map(|f| plane_views(device, image, f))
        .transpose()
    {
        Ok(planes) => planes,
        Err(e) => {
            // SAFETY: `image` and `memory` were created in this call; nothing recorded them.
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
            return Err(e);
        }
    };
    Ok(Imported {
        image,
        memory,
        planes,
        fences: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redraw_signals_the_next_read() {
        let mut reads = 5;
        let mut slot = SlotReads {
            written: 9,
            read: 5,
        };
        assert_eq!(next_read(&mut reads, &mut slot), (9, 6));
        assert_eq!(next_read(&mut reads, &mut slot), (9, 7), "a second redraw");
        assert_eq!((reads, slot.read), (7, 7));
    }
}
