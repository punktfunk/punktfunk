//! Decode image pools: picture slots are decoupled from DPB slots.
//!
//! Pool size is `required_slots + HOLD_HEADROOM`. A DPB slot binds a free
//! picture at activation; a re-activated slot may bind a different one.
//! A consumer-held picture stays off the free list until its release token
//! returns, so it is never a decode target.
//!
//! - **coincide, separate refs**: one `DPB|DST|SAMPLED` image per picture.
//! - **coincide, layered refs**: one array image, one picture per layer.
//! - **distinct**: a reference-only DPB (layered or per-slot; never
//!   delivered, slot↔layer mapping fixed) plus `DST|SAMPLED` pictures.
//!
//! Each picture owns a timeline semaphore (AVVkFrame): the decoder
//! signals `value+1` on write; the presenter waits, samples, restores
//! layout, and signals `value+1` in the same submission. `release_frame`
//! waits that write-back before the picture's next use.

use ash::vk;

use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps::COINCIDE_USAGE;
use crate::caps::DPB_USAGE;
use crate::caps::OUTPUT_USAGE;
use crate::device::find_memory_type_preferring;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::device::Unwind;

pub use pf_bitstream::slots::HOLD_HEADROOM;

/// Pool layout for one `(caps, required_slots)` pair. No GPU allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPlan {
    /// Distinct-mode reference-only DPB images; 0 in coincide (the picture
    /// pool is the DPB there).
    pub dpb_image_count: u32,
    pub dpb_layers_per_image: u32,
    pub dpb_usage: vk::ImageUsageFlags,
    /// Layers in each backing picture image. Layered coincide uses
    /// `picture_count` so every DPB candidate is a layer of the one image.
    pub picture_layers_per_image: u32,
    /// Decode outputs; also DPB bindings in coincide mode.
    pub picture_count: u32,
    pub picture_usage: vk::ImageUsageFlags,
    pub picture_flags: vk::ImageCreateFlags,
}

/// `required_slots` is the stream's `max_dpb_frames + 1`. Picture count is
/// that plus [`HOLD_HEADROOM`] so a consumer-held picture is never a decode
/// target. Layered coincide puts all of them in one array image.
pub fn plan_pools(caps: &DecodeCaps, required_slots: u32) -> PoolPlan {
    let picture_count = required_slots + HOLD_HEADROOM;
    let picture_flags = vk::ImageCreateFlags::MUTABLE_FORMAT;
    if caps.coincide {
        PoolPlan {
            dpb_image_count: 0,
            dpb_layers_per_image: 0,
            dpb_usage: vk::ImageUsageFlags::empty(),
            picture_layers_per_image: if caps.layered_dpb { picture_count } else { 1 },
            picture_count,
            picture_usage: COINCIDE_USAGE,
            picture_flags,
        }
    } else {
        let (dpb_image_count, dpb_layers_per_image) = if caps.layered_dpb {
            (1, required_slots)
        } else {
            (required_slots, 1)
        };
        PoolPlan {
            dpb_image_count,
            dpb_layers_per_image,
            dpb_usage: DPB_USAGE,
            picture_layers_per_image: 1,
            picture_count,
            picture_usage: OUTPUT_USAGE,
            picture_flags,
        }
    }
}

/// Give the pool's pictures TRANSFER_SRC when the driver answers a query that asks for it
/// on `format`. Some drivers report exactly the usage they were asked about, so the base
/// negotiation's answer cannot vouch for a bit it never asked. `false` leaves the plan as is.
///
/// # Safety
///
/// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
pub(crate) unsafe fn allow_copy_out(
    dev: &DecodeDevice,
    profile: DecodeProfile,
    plan: &mut PoolPlan,
    format: vk::Format,
) -> bool {
    let usage = plan.picture_usage | vk::ImageUsageFlags::TRANSFER_SRC;
    // SAFETY: fn contract; a physical-device query.
    let answered = match unsafe { crate::caps::query_formats(dev, profile, usage) } {
        Ok(formats) => formats.iter().any(|f| {
            f.format == format
                && f.image_usage.contains(usage)
                && f.image_create_flags.contains(plan.picture_flags)
                && f.image_tiling == vk::ImageTiling::OPTIMAL
        }),
        Err(_) => false,
    };
    if answered {
        plan.picture_usage = usage;
    }
    tracing::debug!(?format, answered, "decode pictures copyable (TRANSFER_SRC)");
    answered
}

pub(crate) struct Picture {
    /// Shared with the other pictures when a layered coincide pool backs them
    /// all with one array image.
    pub image: vk::Image,
    /// Array layer this picture occupies; 0 when the backing image is private.
    pub layer: u32,
    /// Full-picture view of [`Self::layer`]: decode dst and (coincide) DPB.
    pub view: vk::ImageView,
    /// Per-plane views of [`Self::layer`] for the presenter's sampler, formats
    /// from [`crate::caps::plane_formats`].
    pub plane_views: [vk::ImageView; 2],
    /// The picture's own timeline semaphore (AVVkFrame contract).
    pub semaphore: vk::Semaphore,
    /// Latest timeline value signalled or enqueued. Decoder write, then
    /// presenter's write-back (`frame.value + 1`) once a release token
    /// reports the sample.
    pub value: u64,
    /// A DPB slot currently binds this picture (coincide mode).
    pub bound: bool,
    /// Decoded picture awaiting its output verdict.
    pub pending: bool,
    /// Frames over this picture not yet released (ready queue + consumer-held).
    pub held: u32,
}

impl Picture {
    pub(crate) fn is_free(&self) -> bool {
        !self.bound && !self.pending && self.held == 0
    }
}

/// Picture pool. Drop destroys every handle (null-safe). A pool with
/// consumer-held images is retired to the decoder's graveyard and dies
/// when the last release token arrives — do not Drop it while `held > 0`.
pub(crate) struct PicturePool {
    device: ash::Device,
    /// Backing images, once each: a layered coincide pool stores one array
    /// handle here while `pictures` repeats it with a different layer.
    images: Vec<vk::Image>,
    memory: Vec<vk::DeviceMemory>,
    /// Caps-resolved `output_format` this pool was created with. Stashed
    /// because a delivered frame outlives its generation's caps entry
    /// (session caps are keyed by profile). `build_frame` stamps it onto
    /// each `DecodedVkFrame`.
    pub(crate) format: vk::Format,
    /// The pictures carry TRANSFER_SRC ([`allow_copy_out`]).
    pub(crate) copyable: bool,
    pub(crate) pictures: Vec<Picture>,
}

impl PicturePool {
    /// `plan.picture_count` pictures at `extent` (granularity-aligned
    /// allocation extent, not coded size), each occupying one layer of a
    /// `picture_layers_per_image`-layer backing image.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        extent: vk::Extent2D,
        profile: DecodeProfile,
    ) -> Result<Self, AllocError> {
        let mut pool = Self {
            device: dev.ash().clone(),
            images: Vec::new(),
            memory: Vec::new(),
            format: caps.output_format,
            copyable: plan
                .picture_usage
                .contains(vk::ImageUsageFlags::TRANSFER_SRC),
            pictures: Vec::new(),
        };
        let families = dev.sharing_families();
        let layers = plan.picture_layers_per_image.max(1);
        let image_count = plan.picture_count.div_ceil(layers);
        for _ in 0..image_count {
            // SAFETY: fn contract (live device); each handle is parked in
            // `pool` so a mid-build failure unwinds through Drop.
            let (image, memory) = unsafe {
                create_video_image(
                    dev,
                    caps.output_format,
                    extent,
                    layers,
                    plan.picture_usage,
                    plan.picture_flags,
                    &families,
                    profile,
                )?
            };
            pool.images.push(image);
            pool.memory.push(memory);
        }
        for picture_index in 0..plan.picture_count {
            let image = pool.images[(picture_index / layers) as usize];
            let layer = picture_index % layers;
            // Park with null views/semaphore first: Drop ignores nulls, so a
            // later create failure still unwinds the images and everything
            // already filled.
            pool.pictures.push(Picture {
                image,
                layer,
                view: vk::ImageView::null(),
                plane_views: [vk::ImageView::null(); 2],
                semaphore: vk::Semaphore::null(),
                value: 0,
                bound: false,
                pending: false,
                held: 0,
            });
            let picture = pool.pictures.len() - 1;
            // SAFETY: `image` was created with `layers` layers, `layer` is in
            // range, and plane formats are caps-resolved for MUTABLE_FORMAT.
            unsafe {
                pool.pictures[picture].view = create_view(
                    &pool.device,
                    image,
                    caps.output_format,
                    vk::ImageAspectFlags::COLOR,
                    layer,
                )?;
                pool.pictures[picture].plane_views[0] = create_view(
                    &pool.device,
                    image,
                    caps.plane_view_formats[0],
                    vk::ImageAspectFlags::PLANE_0,
                    layer,
                )?;
                pool.pictures[picture].plane_views[1] = create_view(
                    &pool.device,
                    image,
                    caps.plane_view_formats[1],
                    vk::ImageAspectFlags::PLANE_1,
                    layer,
                )?;
            }
            let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let sem_ci = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
            // SAFETY: live device; timelineSemaphore enabled per the handles
            // contract.
            pool.pictures[picture].semaphore =
                unsafe { pool.device.create_semaphore(&sem_ci, None)? };
        }
        Ok(pool)
    }

    pub(crate) fn free_index(&self) -> Option<usize> {
        self.pictures.iter().position(Picture::is_free)
    }

    /// Unreleased frames across the pool (graveyard retirement key).
    pub(crate) fn held_total(&self) -> u32 {
        self.pictures.iter().map(|p| p.held).sum()
    }
}

/// Bound on [`PicturePool`]'s drop wait: a presenter copy takes milliseconds.
const DROP_WAIT_NS: u64 = 1_000_000_000;

impl Drop for PicturePool {
    fn drop(&mut self) {
        // A picture's last value includes the presenter's write-back (`frame.value + 1`), and
        // the native lane sends its token while that copy still runs: wait it out before the
        // image and the semaphore the copy signals are destroyed. Bounded; a wedge forfeits.
        let (sems, values): (Vec<_>, Vec<_>) = self
            .pictures
            .iter()
            .filter(|p| p.value > 0)
            .map(|p| (p.semaphore, p.value))
            .unzip();
        if !sems.is_empty() {
            let info = vk::SemaphoreWaitInfo::default()
                .semaphores(&sems)
                .values(&values);
            // SAFETY: live device; every semaphore is one of this pool's own timelines.
            let _ = unsafe { self.device.wait_semaphores(&info, DROP_WAIT_NS) };
        }
        // SAFETY: own handles on the contract-live device. The decoder drains
        // decode work before drop/retire, and the wait above covers the presenter's
        // last use of each picture. Destroys ignore NULL.
        unsafe {
            for p in self.pictures.drain(..) {
                self.device.destroy_image_view(p.view, None);
                self.device.destroy_image_view(p.plane_views[0], None);
                self.device.destroy_image_view(p.plane_views[1], None);
                self.device.destroy_semaphore(p.semaphore, None);
            }
            for image in self.images.drain(..) {
                self.device.destroy_image(image, None);
            }
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// Distinct-mode reference-only DPB. Slot↔layer mapping is fixed: these
/// images are never delivered, so nothing consumer-side pins them.
pub(crate) struct DpbPool {
    device: ash::Device,
    images: Vec<vk::Image>,
    memory: Vec<vk::DeviceMemory>,
    dpb_views: Vec<vk::ImageView>,
    dpb_location: Vec<(usize, u32)>,
}

impl DpbPool {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        extent: vk::Extent2D,
        profile: DecodeProfile,
    ) -> Result<Self, AllocError> {
        let mut pool = Self {
            device: dev.ash().clone(),
            images: Vec::new(),
            memory: Vec::new(),
            dpb_views: Vec::new(),
            dpb_location: Vec::new(),
        };
        let families = dev.sharing_families();
        for _ in 0..plan.dpb_image_count {
            // SAFETY: fn contract (live device); parked in `pool` for unwinding.
            let (image, memory) = unsafe {
                create_video_image(
                    dev,
                    caps.dpb_format,
                    extent,
                    plan.dpb_layers_per_image,
                    plan.dpb_usage,
                    vk::ImageCreateFlags::empty(),
                    &families,
                    profile,
                )?
            };
            pool.images.push(image);
            pool.memory.push(memory);
        }
        let slots = plan.dpb_image_count * plan.dpb_layers_per_image;
        for slot in 0..slots {
            let (image_index, layer) = if plan.dpb_image_count == 1 {
                (0usize, slot)
            } else {
                (slot as usize, 0u32)
            };
            // SAFETY: the image was created above with `layer` in range.
            let view = unsafe {
                create_view(
                    &pool.device,
                    pool.images[image_index],
                    caps.dpb_format,
                    vk::ImageAspectFlags::COLOR,
                    layer,
                )?
            };
            pool.dpb_views.push(view);
            pool.dpb_location.push((image_index, layer));
        }
        Ok(pool)
    }

    pub(crate) fn dpb_view(&self, slot: u8) -> vk::ImageView {
        self.dpb_views[usize::from(slot)]
    }

    /// Image and array layer for DPB `slot` (barrier targeting).
    pub(crate) fn dpb_target(&self, slot: u8) -> (vk::Image, u32) {
        let (image_index, layer) = self.dpb_location[usize::from(slot)];
        (self.images[image_index], layer)
    }
}

impl Drop for DpbPool {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device. The decoder drains
        // decode work before drop; nothing consumer-side references these.
        // Destroys ignore NULL.
        unsafe {
            for view in self.dpb_views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            for image in self.images.drain(..) {
                self.device.destroy_image(image, None);
            }
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// One OPTIMAL-tiling video image, bound to fresh DEVICE_LOCAL memory and
/// listed on `decode_profile`.
///
/// # Safety
///
/// `dev` wraps live handles.
#[allow(clippy::too_many_arguments)]
unsafe fn create_video_image(
    dev: &DecodeDevice,
    format: vk::Format,
    extent: vk::Extent2D,
    layers: u32,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
    families: &[u32],
    decode_profile: DecodeProfile,
) -> Result<(vk::Image, vk::DeviceMemory), AllocError> {
    let mut chain = decode_profile.chain();
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let mut ci = vk::ImageCreateInfo::default()
        .flags(flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut profile_list);
    ci = if families.len() >= 2 {
        ci.sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(families)
    } else {
        ci.sharing_mode(vk::SharingMode::EXCLUSIVE)
    };
    // SAFETY: only the image and memory created below go in, before any use.
    let mut unwind = unsafe { Unwind::new(dev.ash()) };
    // SAFETY: live device; `ci` roots a chain of locals outliving the call.
    let image = unsafe { dev.ash().create_image(&ci, None)? };
    unwind.image = image;
    // SAFETY: `image` was just created on this device.
    let req = unsafe { dev.ash().get_image_memory_requirements(image) };
    // DEVICE_LOCAL preferred; any type in `memoryTypeBits` is accepted (the
    // driver's placement contract, same as session bindings).
    let type_index = find_memory_type_preferring(
        &dev.memory_properties(),
        req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(type_index);
    // SAFETY: live device; `alloc` is a local.
    let memory = unsafe { dev.ash().allocate_memory(&alloc, None)? };
    unwind.memory = memory;
    // SAFETY: fresh image + fresh memory of the required size.
    unsafe { dev.ash().bind_image_memory(image, memory, 0)? };
    unwind.disarm();
    Ok((image, memory))
}

/// Single-layer 2D view (`base_array_layer = layer`, identity swizzle).
///
/// # Safety
///
/// `image` is live on `device` with `layer` in range; `format`/`aspect` are
/// compatible with the image's creation (same format for COLOR, plane-compatible
/// under MUTABLE_FORMAT for the plane aspects).
unsafe fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
    layer: u32,
) -> Result<vk::ImageView, vk::Result> {
    let ci = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: aspect,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: layer,
            layer_count: 1,
        });
    // SAFETY: fn contract: live `image`, `layer` in range, `format`/`aspect`
    // compatible (COLOR same-format; plane aspects under MUTABLE_FORMAT).
    unsafe { device.create_image_view(&ci, None) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::derive_caps;
    use crate::caps::MaxLevelIdc;
    use crate::caps::RawCaps;
    use crate::caps::VideoFormat;
    use crate::caps::NV12;

    fn caps(coincide: bool, layered: bool) -> DecodeCaps {
        // Each entry must advertise its role's full usage plus MUTABLE_FORMAT;
        // derivation gates on those. This module's table is downstream of that.
        let entry = |usage: vk::ImageUsageFlags| VideoFormat {
            format: NV12,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
            ..Default::default()
        };
        let raw = RawCaps {
            capability_flags: if layered {
                vk::VideoCapabilityFlagsKHR::empty()
            } else {
                vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES
            },
            decode_flags: if coincide {
                vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE
            } else {
                vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT
            },
            min_bitstream_buffer_offset_alignment: 0,
            min_bitstream_buffer_size_alignment: 0,
            picture_access_granularity: vk::Extent2D::default(),
            min_coded_extent: vk::Extent2D::default(),
            max_coded_extent: vk::Extent2D::default(),
            max_dpb_slots: 0,
            max_active_reference_pictures: 0,
            max_level: MaxLevelIdc::H264(0),
            std_header_version: vk::ExtensionProperties::default(),
            dpb_formats: vec![entry(DPB_USAGE)],
            output_formats: vec![entry(OUTPUT_USAGE)],
            coincide_formats: vec![entry(COINCIDE_USAGE)],
        };
        derive_caps(&raw, NV12).unwrap()
    }

    #[test]
    fn coincide_pools_are_headroomed_dual_use_pictures_with_no_dpb_array() {
        let plan = plan_pools(&caps(true, false), 8);
        assert_eq!(
            plan.dpb_image_count, 0,
            "the picture pool IS the DPB backing"
        );
        assert_eq!(
            plan.picture_count,
            8 + HOLD_HEADROOM,
            "the stream's DPB depth PLUS the consumer-hold headroom — a pool \
             sized to either alone starves (.25 field failure)"
        );
        assert_eq!(
            plan.picture_usage, COINCIDE_USAGE,
            "pool pictures are DPB + decode dst + sampled surface in one"
        );
        assert_eq!(plan.picture_layers_per_image, 1);
        assert_eq!(plan.picture_flags, vk::ImageCreateFlags::MUTABLE_FORMAT);
    }

    #[test]
    fn layered_coincide_puts_every_picture_in_one_array() {
        let plan = plan_pools(&caps(true, true), 8);
        assert_eq!(plan.dpb_image_count, 0);
        assert_eq!(plan.picture_count, 8 + HOLD_HEADROOM);
        assert_eq!(
            plan.picture_layers_per_image, plan.picture_count,
            "without SEPARATE_REFERENCE_IMAGES every DPB candidate is a layer"
        );
        assert_eq!(plan.picture_usage, COINCIDE_USAGE);
    }

    #[test]
    fn distinct_keeps_a_fixed_dpb_array_and_headrooms_the_output_pool() {
        let plan = plan_pools(&caps(false, true), 17);
        assert_eq!(
            (plan.dpb_image_count, plan.dpb_layers_per_image),
            (1, 17),
            "layered: one array, one layer per slot"
        );
        assert_eq!(plan.dpb_usage, DPB_USAGE);
        assert_eq!(plan.picture_count, 17 + HOLD_HEADROOM);
        assert_eq!(plan.picture_layers_per_image, 1);
        assert_eq!(plan.picture_usage, OUTPUT_USAGE);

        let plan = plan_pools(&caps(false, false), 3);
        assert_eq!(
            (plan.dpb_image_count, plan.dpb_layers_per_image),
            (3, 1),
            "separate reference images: one image per slot"
        );
        assert_eq!(plan.picture_count, 3 + HOLD_HEADROOM);
    }

    #[test]
    fn picture_occupancy_frees_only_when_unbound_unpending_and_released() {
        let mut p = Picture {
            image: vk::Image::null(),
            layer: 0,
            view: vk::ImageView::null(),
            plane_views: [vk::ImageView::null(); 2],
            semaphore: vk::Semaphore::null(),
            value: 0,
            bound: true,
            pending: true,
            held: 2,
        };
        assert!(!p.is_free());
        p.bound = false;
        assert!(!p.is_free(), "pending pictures are not decode targets");
        p.pending = false;
        assert!(!p.is_free(), "held frames are not decode targets");
        p.held = 1;
        assert!(!p.is_free());
        p.held = 0;
        assert!(p.is_free());
    }
}
