//! The device features PyroWave decode turns on, in the structs the device's Vulkan version
//! takes: `PhysicalDeviceVulkan12Features` / `13Features` where that version is core, the
//! extension's own struct below it. pyrowave reads the same structs back off the device
//! create-info, so the probe, the device create and the replica handed to pyrowave all
//! build this one chain.

use ash::vk;
use std::ffi::{c_void, CStr};

const RENDERPASS2: &CStr = c"VK_KHR_create_renderpass2";
const TIMELINE: &CStr = c"VK_KHR_timeline_semaphore";
const STORAGE8: &CStr = c"VK_KHR_8bit_storage";
const FLOAT16: &CStr = c"VK_KHR_shader_float16_int8";
const SIZE_CONTROL: &CStr = c"VK_EXT_subgroup_size_control";
const SYNC2: &CStr = c"VK_KHR_synchronization2";

/// The lowest device version the lane runs on.
pub const MIN_API: u32 = vk::API_VERSION_1_1;

/// What the codec and this lane turn on. `storage16`, `float16`, `sync2` and
/// `sampler_ycbcr` are optional: pyrowave picks other paths without them. Its
/// storage-buffer kernels read 16-bit words, so `storage16` goes on wherever it exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub int16: bool,
    pub storage8: bool,
    pub storage16: bool,
    pub float16: bool,
    pub timeline: bool,
    pub size_control: bool,
    pub full_subgroups: bool,
    pub sync2: bool,
    pub sampler_ycbcr: bool,
}

impl Facts {
    /// The device decodes PyroWave on this lane.
    pub fn decodes(&self) -> bool {
        self.int16 && self.storage8 && self.timeline && self.size_control && self.full_subgroups
    }
}

/// Device extensions to enable for a device usable at `api`: each feature's extension below
/// the version that made it core. `has` answers whether the device offers one.
///
/// `None` when a required one is missing. Below 1.2 pyrowave's render passes need
/// `VK_KHR_create_renderpass2`.
pub fn extensions(api: u32, has: impl Fn(&CStr) -> bool) -> Option<Vec<&'static CStr>> {
    let mut out = Vec::new();
    let mut take = |name: &'static CStr, required: bool| {
        let offered = has(name);
        if offered {
            out.push(name);
        }
        offered || !required
    };
    let mut ok = true;
    if api < vk::API_VERSION_1_2 {
        ok &= take(RENDERPASS2, true);
        ok &= take(TIMELINE, true);
        ok &= take(STORAGE8, true);
        take(FLOAT16, false);
    }
    if api < vk::API_VERSION_1_3 {
        ok &= take(SIZE_CONTROL, true);
        take(SYNC2, false);
    }
    ok.then_some(out)
}

/// One `VkPhysicalDeviceFeatures2` chain. Boxed, so the links survive a move.
pub struct Chain {
    head: Box<vk::PhysicalDeviceFeatures2<'static>>,
    v11: Box<vk::PhysicalDeviceVulkan11Features<'static>>,
    v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    ycbcr: Box<vk::PhysicalDeviceSamplerYcbcrConversionFeatures<'static>>,
    storage16: Box<vk::PhysicalDevice16BitStorageFeatures<'static>>,
    timeline: Box<vk::PhysicalDeviceTimelineSemaphoreFeatures<'static>>,
    storage8: Box<vk::PhysicalDevice8BitStorageFeatures<'static>>,
    float16: Box<vk::PhysicalDeviceShaderFloat16Int8Features<'static>>,
    size_control: Box<vk::PhysicalDeviceSubgroupSizeControlFeatures<'static>>,
    sync2: Box<vk::PhysicalDeviceSynchronization2Features<'static>>,
    core12: bool,
    core13: bool,
}

impl Chain {
    /// The chain for a device usable at `api`, every feature off. `has` answers for a
    /// device extension: offered when probing, enabled when replaying a create-info. A
    /// struct whose extension is absent stays out of the chain.
    pub fn new(api: u32, has: impl Fn(&CStr) -> bool) -> Chain {
        let mut c = Chain {
            head: Box::default(),
            v11: Box::default(),
            v12: Box::default(),
            v13: Box::default(),
            ycbcr: Box::default(),
            storage16: Box::default(),
            timeline: Box::default(),
            storage8: Box::default(),
            float16: Box::default(),
            size_control: Box::default(),
            sync2: Box::default(),
            core12: api >= vk::API_VERSION_1_2,
            core13: api >= vk::API_VERSION_1_3,
        };
        let mut links: Vec<*mut c_void> = Vec::new();
        let mut link = |on: bool, s: *mut c_void| {
            if on {
                links.push(s);
            }
        };
        link(c.core12, (&raw mut *c.v11).cast());
        link(c.core12, (&raw mut *c.v12).cast());
        // Core in 1.1, so these two structs need no extension.
        link(!c.core12, (&raw mut *c.ycbcr).cast());
        link(!c.core12, (&raw mut *c.storage16).cast());
        link(!c.core12 && has(TIMELINE), (&raw mut *c.timeline).cast());
        link(!c.core12 && has(STORAGE8), (&raw mut *c.storage8).cast());
        link(!c.core12 && has(FLOAT16), (&raw mut *c.float16).cast());
        link(c.core13, (&raw mut *c.v13).cast());
        link(
            !c.core13 && has(SIZE_CONTROL),
            (&raw mut *c.size_control).cast(),
        );
        link(!c.core13 && has(SYNC2), (&raw mut *c.sync2).cast());
        let mut prev: *mut *mut c_void = &raw mut c.head.p_next;
        for s in links {
            // SAFETY: `prev` is the `p_next` of the head or of a struct this chain boxes, and
            // `s` is one of those boxes. Every Vulkan feature struct opens with `sType`,
            // `pNext`, so the `BaseOutStructure` view of `s` names its `p_next`.
            unsafe {
                *prev = s;
                prev = (&raw mut (*s.cast::<vk::BaseOutStructure<'static>>()).p_next).cast();
            }
        }
        c
    }

    /// The head, for `vkGetPhysicalDeviceFeatures2`.
    pub fn head(&mut self) -> &mut vk::PhysicalDeviceFeatures2<'static> {
        &mut self.head
    }

    /// The head as a `VkDeviceCreateInfo::pNext`. Valid while the chain lives.
    pub fn as_ptr(&self) -> *const c_void {
        (&raw const *self.head).cast()
    }

    pub fn facts(&self) -> Facts {
        let on = |b: vk::Bool32| b == vk::TRUE;
        let (c12, c13) = (self.core12, self.core13);
        Facts {
            int16: on(self.head.features.shader_int16),
            storage8: on(if c12 {
                self.v12.storage_buffer8_bit_access
            } else {
                self.storage8.storage_buffer8_bit_access
            }),
            storage16: on(if c12 {
                self.v11.storage_buffer16_bit_access
            } else {
                self.storage16.storage_buffer16_bit_access
            }),
            float16: on(if c12 {
                self.v12.shader_float16
            } else {
                self.float16.shader_float16
            }),
            timeline: on(if c12 {
                self.v12.timeline_semaphore
            } else {
                self.timeline.timeline_semaphore
            }),
            size_control: on(if c13 {
                self.v13.subgroup_size_control
            } else {
                self.size_control.subgroup_size_control
            }),
            full_subgroups: on(if c13 {
                self.v13.compute_full_subgroups
            } else {
                self.size_control.compute_full_subgroups
            }),
            sync2: on(if c13 {
                self.v13.synchronization2
            } else {
                self.sync2.synchronization2
            }),
            sampler_ycbcr: on(if c12 {
                self.v11.sampler_ycbcr_conversion
            } else {
                self.ycbcr.sampler_ycbcr_conversion
            }),
        }
    }

    /// Turn on exactly `f`. A fact whose struct is out of the chain reaches no device.
    pub fn enable(&mut self, f: Facts) {
        let b = |on: bool| if on { vk::TRUE } else { vk::FALSE };
        self.head.features.shader_int16 = b(f.int16);
        self.v12.storage_buffer8_bit_access = b(f.storage8);
        self.storage8.storage_buffer8_bit_access = b(f.storage8);
        self.v11.storage_buffer16_bit_access = b(f.storage16);
        self.storage16.storage_buffer16_bit_access = b(f.storage16);
        self.v12.shader_float16 = b(f.float16);
        self.float16.shader_float16 = b(f.float16);
        self.v12.timeline_semaphore = b(f.timeline);
        self.timeline.timeline_semaphore = b(f.timeline);
        self.v13.subgroup_size_control = b(f.size_control);
        self.size_control.subgroup_size_control = b(f.size_control);
        self.v13.compute_full_subgroups = b(f.full_subgroups);
        self.size_control.compute_full_subgroups = b(f.full_subgroups);
        self.v13.synchronization2 = b(f.sync2);
        self.sync2.synchronization2 = b(f.sync2);
        self.v11.sampler_ycbcr_conversion = b(f.sampler_ycbcr);
        self.ycbcr.sampler_ycbcr_conversion = b(f.sampler_ycbcr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `sType`s a chain links, head first.
    fn stypes(c: &Chain) -> Vec<vk::StructureType> {
        let mut out = Vec::new();
        let mut p = c.as_ptr().cast::<vk::BaseOutStructure<'static>>();
        while !p.is_null() {
            // SAFETY: the chain links only its own boxed feature structs, all alive in `c`.
            unsafe {
                out.push((*p).s_type);
                p = (*p).p_next.cast_const();
            }
        }
        out
    }

    const ALL: Facts = Facts {
        int16: true,
        storage8: true,
        storage16: true,
        float16: true,
        timeline: true,
        size_control: true,
        full_subgroups: true,
        sync2: true,
        sampler_ycbcr: true,
    };

    #[test]
    fn each_version_takes_its_own_structs() {
        use vk::StructureType as S;
        let c = Chain::new(vk::API_VERSION_1_3, |_| false);
        assert_eq!(
            stypes(&c),
            [
                S::PHYSICAL_DEVICE_FEATURES_2,
                S::PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
                S::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                S::PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
            ]
        );
        let c = Chain::new(vk::API_VERSION_1_2, |_| true);
        assert_eq!(
            stypes(&c),
            [
                S::PHYSICAL_DEVICE_FEATURES_2,
                S::PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
                S::PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                S::PHYSICAL_DEVICE_SUBGROUP_SIZE_CONTROL_FEATURES,
                S::PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
            ]
        );
        // 1.1 with no sync2: its struct stays out, and the fact reads off.
        let mut c = Chain::new(vk::API_VERSION_1_1, |n| n != SYNC2);
        assert_eq!(
            stypes(&c),
            [
                S::PHYSICAL_DEVICE_FEATURES_2,
                S::PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES,
                S::PHYSICAL_DEVICE_16BIT_STORAGE_FEATURES,
                S::PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
                S::PHYSICAL_DEVICE_8BIT_STORAGE_FEATURES,
                S::PHYSICAL_DEVICE_SHADER_FLOAT16_INT8_FEATURES,
                S::PHYSICAL_DEVICE_SUBGROUP_SIZE_CONTROL_FEATURES,
            ]
        );
        c.enable(ALL);
        assert!(c.facts().decodes());
    }

    #[test]
    fn facts_round_trip_at_every_version() {
        for api in [
            vk::API_VERSION_1_1,
            vk::API_VERSION_1_2,
            vk::API_VERSION_1_3,
        ] {
            let mut c = Chain::new(api, |_| true);
            assert_eq!(c.facts(), Facts::default());
            c.enable(ALL);
            assert_eq!(c.facts(), ALL, "api {api:#x}");
        }
    }

    #[test]
    fn a_missing_required_extension_refuses_the_device() {
        let names = |api, has: &dyn Fn(&CStr) -> bool| extensions(api, has);
        assert_eq!(names(vk::API_VERSION_1_3, &|_| false), Some(vec![]));
        assert_eq!(
            names(vk::API_VERSION_1_2, &|_| true),
            Some(vec![SIZE_CONTROL, SYNC2])
        );
        assert_eq!(names(vk::API_VERSION_1_2, &|n| n == SYNC2), None);
        assert_eq!(
            names(vk::API_VERSION_1_1, &|n| n != SYNC2 && n != FLOAT16),
            Some(vec![RENDERPASS2, TIMELINE, STORAGE8, SIZE_CONTROL])
        );
        assert_eq!(names(vk::API_VERSION_1_1, &|n| n != TIMELINE), None);
    }
}
