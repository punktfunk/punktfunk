//! Shared Vulkan Video bring-up for the `#[ignore]`d GPU legs.
//!
//! `tests/gpu_smoke.rs` and `tests/gpu_parity.rs` each drive three codecs
//! through one path: loader → instance → physical device whose queue families
//! carry the codec's decode ops → logical device with the decode extensions,
//! `timelineSemaphore`, and `synchronization2`.
//!
//! [`Graphics`] is the only caller difference: parity requires a graphics
//! queue for readback; smoke accepts decode-only and falls back, which also
//! picks EXCLUSIVE vs CONCURRENT pool images. Pin a PCI vendor with
//! `PF_VKD_SMOKE_VENDOR` (`0x1002`/`0x10de`). RADV needs
//! `RADV_PERFTEST=video_decode` or no device advertises the extensions.
//! [`Request::report_families`] is a physical-device property query, never a
//! recorded RESULT_STATUS query, so it cannot hang RADV's VCN.
//!
//! Compiled as `mod common;` of each test binary (Cargo does not auto-discover
//! this file), so `#![deny(clippy::undocumented_unsafe_blocks)]` applies here.

// Smoke never reads `Setup::pd`; parity never passes `DecodeFamilyIsFine` or splits IVF.
// A test binary has no `pub` exemption for dead code or unused re-exports.
#![allow(dead_code, unused_imports)]

use std::io::Cursor;

use ash::vk;
use ash::vk::Handle;
pub use pf_bitstream::testing::split_h264_aus;
pub use pf_bitstream::testing::split_h265_aus;
pub use pf_bitstream::testing::split_ivf;
use pf_vkdecode::DeviceHandles;

/// Vendored H.264 vector: two slice NALUs per picture, so the splitter's
/// `first_mb_in_slice == 0` branch is load-bearing, and the slice-control
/// buffer is two records wide where the HEVC vector's is one.
pub const TEST_25FPS_H264: &[u8] = pf_bitstream::testing::H264_25FPS;

/// Vendored H.265 twin: one IDR_N_LP then 249 TRAIL pictures.
pub const TEST_25FPS_H265: &[u8] = pf_bitstream::testing::H265_25FPS;

/// Vendored AV1 twin: 250 temporal units, 274 coded frames. 24 units carry a
/// hidden second frame (decoded, referenced, never shown); the vector has no
/// `show_existing_frame`. The directory also has byte-identical
/// `test-25fps.av1.ivf`; this name is the one with `.md5`/`.crc` beside it.
pub const TEST_25FPS_AV1: &[u8] = pf_bitstream::testing::AV1_25FPS;

/// Rewrite every Annex-B start code to four-byte (`00 00 00 01`).
/// Vendored vectors are three-byte; the host emits four-byte. A fixed `+3 + 2`
/// skip into a four-byte prefix lands a byte early (`pps_id`); `ring::pack_slices`
/// trims the leading zero. Payload is `nalu.data[nalu.offset..]` after
/// `Nalu::next` drops `trailing_zero_8bits`. Generic over the NAL header;
/// AU splitters cannot share (boundary rules differ).
fn four_byte_start_codes<H>(stream: &[u8]) -> Vec<u8>
where
    H: cros_codecs::codec::h264::nalu::Header + std::fmt::Debug,
{
    use cros_codecs::codec::h264::nalu::Nalu;

    // Lower bound: one extra byte per three-byte prefix, minus trailing zeroes.
    let mut out = Vec::with_capacity(stream.len());
    let mut cursor = Cursor::new(stream);
    while let Ok(nalu) = Nalu::<H>::next(&mut cursor) {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&nalu.data[nalu.offset..]);
    }
    out
}

pub fn h264_four_byte_start_codes(stream: &[u8]) -> Vec<u8> {
    four_byte_start_codes::<cros_codecs::codec::h264::parser::NaluHeader>(stream)
}

pub fn h265_four_byte_start_codes(stream: &[u8]) -> Vec<u8> {
    four_byte_start_codes::<cros_codecs::codec::h265::parser::NaluHeader>(stream)
}

/// Serializes GPU legs in one test binary. Hold it for the whole leg.
/// Cargo parallelizes tests: two decoders would share a decode queue, and
/// the parity legs' `PF_VKD_TEST_READBACK` `set_var` is not thread-safe.
/// Poison is ignored so a panicked first leg does not skip the second codec.
pub fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
    static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GPU.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Decode op + extension a bring-up must find on silicon.
#[derive(Clone, Copy)]
pub struct Codec {
    /// Decode op the chosen queue family must advertise. Per family, not per
    /// device: the extension can be present while only one family carries the op.
    pub op: vk::VideoCodecOperationFlagsKHR,
    /// Device extension: required on the physical device and enabled on the
    /// logical one. Skipping enable still reaches `vkCreateVideoSessionKHR`.
    pub extension: &'static std::ffi::CStr,
}

pub const H264: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
    extension: ash::khr::video_decode_h264::NAME,
};

pub const H265: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_H265,
    extension: ash::khr::video_decode_h265::NAME,
};

/// A box that decodes both H.26x codecs may still lack `VK_KHR_video_decode_av1`;
/// on RADV it is behind `RADV_PERFTEST=video_decode`.
pub const AV1: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
    extension: ash::khr::video_decode_av1::NAME,
};

/// Graphics-queue requirement: the one smoke vs parity difference, explicit
/// so it cannot drift into two copied loops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Graphics {
    /// Fall back to the decode family. `decode_qf == graphics_qf` makes pool
    /// images EXCLUSIVE rather than CONCURRENT (`DecodeDevice::sharing_families`).
    DecodeFamilyIsFine,
    /// Skip a device with no graphics family. Parity readback records
    /// `vkCmdCopyImageToBuffer` on that queue.
    Required,
}

/// Bring-up request. A struct, not `bring_up(H265, true, false)`, so the
/// smoke/parity disagreement stays named.
pub struct Request {
    pub codec: Codec,
    pub graphics: Graphics,
    /// Print each candidate's per-family `flags / video_ops /
    /// query_result_status` table. Property query only: recording a
    /// RESULT_STATUS query hangs RADV's VCN.
    pub report_families: bool,
}

/// Live instance + logical device a decoder can be constructed on.
/// Torn down through [`Setup::destroy`], not `Drop`, so decoder-first
/// teardown stays visible in the test body.
pub struct Setup {
    /// [`Setup::handles`] hands the loader's `vkGetInstanceProcAddr` to the
    /// decoder, which resolves everything through it.
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pd: vk::PhysicalDevice,
    pub device: ash::Device,
    pub decode_qf: u32,
    /// Graphics family, or `decode_qf` under [`Graphics::DecodeFamilyIsFine`].
    pub graphics_qf: u32,
}

impl Setup {
    /// Borrowed-handle bundle for decoder construction. Valid only while `self`
    /// is alive and un-destroyed ([`DeviceHandles`] contract).
    pub fn handles(&self) -> DeviceHandles {
        DeviceHandles {
            get_instance_proc_addr: self.entry.static_fn().get_instance_proc_addr as usize,
            instance: self.instance.handle().as_raw() as usize,
            physical_device: self.pd.as_raw() as usize,
            device: self.device.handle().as_raw() as usize,
            decode_qf: self.decode_qf,
            decode_queue_index: 0,
            graphics_qf: self.graphics_qf,
        }
    }

    /// # Safety
    ///
    /// Every object created from this device — the decoder's session/pools, any
    /// readback handles — is already destroyed, and no [`DeviceHandles`] taken
    /// from [`Setup::handles`] is still in use.
    pub unsafe fn destroy(self) {
        // SAFETY: fn contract — nothing derived from these handles survives, so
        // the device can be destroyed and then the instance it came from.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        // Leak the loader. Dropping `ash::Entry` `dlclose`s it; the next
        // `gpu_lock` leg would re-`dlopen` a torn-down loader and fault in
        // `Entry::load` or at exit after both legs reported ok. The process
        // is ending, so the leak is free.
        std::mem::forget(self.entry);
    }
}

/// Optional `PF_VKD_SMOKE_VENDOR` pin. First-match on a multi-GPU box hides
/// every other decode-capable device.
fn vendor_pin() -> Option<u32> {
    std::env::var("PF_VKD_SMOKE_VENDOR").ok().map(|raw| {
        let trimmed = raw.trim();
        trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .map_or_else(|| trimmed.parse(), |hex| u32::from_str_radix(hex, 16))
            .unwrap_or_else(|_| panic!("PF_VKD_SMOKE_VENDOR is not a PCI vendor id: {raw:?}"))
    })
}

/// Loader → instance → physical device for `request.codec` → logical device.
/// Panics naming the codec's own extension, so a box with H.264 but no H.265
/// does not report "no Vulkan Video".
pub fn bring_up(request: &Request) -> Setup {
    // SAFETY: loads the system Vulkan loader; no Vulkan objects exist yet.
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader on this box");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let instance_ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info rooted in locals; the instance is destroyed by
    // `Setup::destroy` after everything created from it.
    let instance =
        unsafe { entry.create_instance(&instance_ci, None) }.expect("create a Vulkan 1.3 instance");

    let vendor_filter = vendor_pin();

    // SAFETY: live instance.
    let physical_devices =
        unsafe { instance.enumerate_physical_devices() }.expect("enumerate physical devices");
    let mut picked: Option<(vk::PhysicalDevice, u32, u32)> = None;
    for pd in physical_devices {
        // SAFETY: `pd` was just enumerated from this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        if vendor_filter.is_some_and(|vendor| props.vendor_id != vendor) {
            continue;
        }
        // SAFETY: `pd` was just enumerated from this instance.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
        let has = |name: &std::ffi::CStr| {
            ext_props.iter().any(|e| {
                e.extension_name_as_c_str()
                    .is_ok_and(|extension| extension == name)
            })
        };
        if !(has(ash::khr::video_queue::NAME)
            && has(ash::khr::video_decode_queue::NAME)
            && has(request.codec.extension))
        {
            continue;
        }
        // SAFETY: live physical device; the two-call form fills the chained video
        // properties for each family.
        let family_count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
        let mut families: Vec<vk::QueueFamilyProperties2<'_>> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: as above, arrays sized to the reported count.
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families) };
        let flags_per_family: Vec<vk::QueueFlags> = families
            .iter()
            .map(|f| f.queue_family_properties.queue_flags)
            .collect();
        drop(families); // release the &mut borrows so video_props is readable

        // Per candidate, not just the picked device — a multi-GPU box reports
        // every device considered.
        if request.report_families {
            let mut status_props =
                vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); family_count];
            let mut families2: Vec<vk::QueueFamilyProperties2<'_>> = status_props
                .iter_mut()
                .map(|s| vk::QueueFamilyProperties2::default().push_next(s))
                .collect();
            // SAFETY: as the query above.
            unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families2) };
            drop(families2);
            for (i, s) in status_props.iter().enumerate() {
                eprintln!(
                    "family {i}: flags={:?} video_ops={:?} query_result_status={}",
                    flags_per_family[i],
                    video_props[i].video_codec_operations,
                    s.query_result_status_support != vk::FALSE,
                );
            }
        }

        let mut decode_qf = None;
        let mut graphics_qf = None;
        for (index, flags) in flags_per_family.iter().enumerate() {
            if flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                graphics_qf = Some(index as u32);
            }
            if flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                && video_props[index]
                    .video_codec_operations
                    .contains(request.codec.op)
                && decode_qf.is_none()
            {
                decode_qf = Some(index as u32);
            }
        }
        match (request.graphics, decode_qf, graphics_qf) {
            (Graphics::Required, Some(decode), Some(graphics)) => {
                picked = Some((pd, decode, graphics));
                break;
            }
            (Graphics::DecodeFamilyIsFine, Some(decode), graphics) => {
                picked = Some((pd, decode, graphics.unwrap_or(decode)));
                break;
            }
            (Graphics::Required, _, None) | (_, None, _) => {}
        }
    }
    let (pd, decode_qf, graphics_qf) = picked.unwrap_or_else(|| {
        panic!(
            "no physical device with {} and a decode queue{}{}",
            request.codec.extension.to_string_lossy(),
            match request.graphics {
                Graphics::Required => " and a graphics queue",
                Graphics::DecodeFamilyIsFine => "",
            },
            match vendor_filter {
                Some(vendor) => format!(" (PF_VKD_SMOKE_VENDOR pinned vendor 0x{vendor:04x})"),
                None => String::new(),
            },
        )
    });

    {
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        // SAFETY: live physical device; the chain fills the Vulkan 1.2 core
        // driver-identity struct.
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
        let props = props2.properties;
        eprintln!(
            "picked: {:?} vendor=0x{:04x} driver={:?} info={:?}",
            props.device_name_as_c_str().unwrap_or(c"?"),
            props.vendor_id,
            driver_props.driver_name_as_c_str().unwrap_or(c"?"),
            driver_props.driver_info_as_c_str().unwrap_or(c"?"),
        );
    }

    let priorities = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(decode_qf)
        .queue_priorities(&priorities)];
    if graphics_qf != decode_qf {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_qf)
                .queue_priorities(&priorities),
        );
    }
    let extensions = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_decode_queue::NAME.as_ptr(),
        request.codec.extension.as_ptr(),
    ];
    let mut features12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut features13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let device_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&extensions)
        .push_next(&mut features12)
        .push_next(&mut features13);
    // SAFETY: live physical device, valid create info rooted in locals; destroyed
    // by `Setup::destroy` after the decoder drops.
    let device =
        unsafe { instance.create_device(pd, &device_ci, None) }.expect("create the decode device");

    Setup {
        entry,
        instance,
        pd,
        device,
        decode_qf,
        graphics_qf,
    }
}
