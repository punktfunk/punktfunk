//! Presenter bring-up: instance → surface → device → swapchain over an SDL window.
//!
//! [`Presenter::new`] is the only construction path. Device creation and
//! [`probe_decode`] both read a [`DecodeProbe`] into [`video_decode_gate`], so a probe
//! cannot report a capability the session then refuses.
//!
//! Pinning: the `vk_device`, `vk_adapter`, `present_mode`, `vrr_fifo` and `hdr10` knobs
//! ([`Knobs`]), read once by the shell.
//!
//! Evidence: [`present_mode_chain`], [`pick_device`], [`AdapterDecode::index`].

use super::timing_ext;
#[cfg(target_os = "linux")]
use super::HwCtx;
#[cfg(windows)]
use super::HwCtxWin;
use super::{OverlayPipe, Presenter};
use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf;
use crate::knobs::Knobs;
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use std::ffi::{c_char, CStr, CString};

/// Codec-agnostic Vulkan Video decode extensions.
/// [`probe_decode`] and device creation gate on this same list.
pub(crate) const VIDEO_BASE: [&std::ffi::CStr; 2] = [
    ash::khr::video_queue::NAME,
    ash::khr::video_decode_queue::NAME,
];

/// Per-codec decode extensions, same sharing rule as [`VIDEO_BASE`].
/// AV1 is a string: ash's headers do not name `VK_KHR_video_decode_av1`.
pub(crate) const VIDEO_CODECS: [&std::ffi::CStr; 3] = [
    ash::khr::video_decode_h264::NAME,
    ash::khr::video_decode_h265::NAME,
    c"VK_KHR_video_decode_av1",
];

/// All five conjuncts of "this device can host Vulkan Video decode".
/// [`DecodeProbe::usable`] is its one caller.
pub(crate) fn video_decode_gate(
    api_1_3: bool,
    features_ok: bool,
    has_decode_family: bool,
    base_exts_present: bool,
    any_codec_ext: bool,
) -> bool {
    api_1_3 && features_ok && has_decode_family && base_exts_present && any_codec_ext
}

/// The inputs to [`video_decode_gate`] for one physical device, read without a logical
/// device. Device creation and [`probe_decode`] both take them from here.
struct DecodeProbe {
    api_1_3: bool,
    /// Sampler YCbCr conversion, timeline semaphores and synchronization2.
    features_ok: bool,
    /// The first decode-capable family and its codec ops. Reported even without the
    /// extensions: hardware-can and driver-does-not-expose are different answers.
    family: Option<(u32, vk::VideoCodecOperationFlagsKHR)>,
    base_missing: Vec<&'static std::ffi::CStr>,
    codec_exts: Vec<&'static std::ffi::CStr>,
}

impl DecodeProbe {
    /// `has` answers whether `pdev` offers a device extension.
    fn of(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        has: impl Fn(&std::ffi::CStr) -> bool,
    ) -> DecodeProbe {
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let props = unsafe { instance.get_physical_device_properties(pdev) };
        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut feats = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);
        // SAFETY: read-only query; the pNext chain locals outlive the call.
        unsafe { instance.get_physical_device_features2(pdev, &mut feats) };
        let features_ok = f11.sampler_ycbcr_conversion == vk::TRUE
            && f12.timeline_semaphore == vk::TRUE
            && f13.synchronization2 == vk::TRUE;

        // SAFETY: read-only query; length only.
        let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pdev) };
        let mut video: Vec<vk::QueueFamilyVideoPropertiesKHR> =
            vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut qprops: Vec<vk::QueueFamilyProperties2> = video
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: read-only query; `qprops` / `video` outlive the call and receive the fill.
        unsafe { instance.get_physical_device_queue_family_properties2(pdev, &mut qprops) };
        // `qprops` mutably borrows `video` via push_next; copy the flags, drop `qprops`,
        // then read the driver-filled video properties.
        let flags: Vec<vk::QueueFlags> = qprops
            .iter()
            .map(|p| p.queue_family_properties.queue_flags)
            .collect();
        drop(qprops);
        let family = flags
            .iter()
            .zip(&video)
            .enumerate()
            .find(|(_, (f, _))| f.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
            .map(|(i, (_, v))| (i as u32, v.video_codec_operations));

        DecodeProbe {
            api_1_3: vk::api_version_major(props.api_version) > 1
                || vk::api_version_minor(props.api_version) >= 3,
            features_ok,
            family,
            base_missing: VIDEO_BASE.into_iter().filter(|n| !has(n)).collect(),
            codec_exts: VIDEO_CODECS.into_iter().filter(|n| has(n)).collect(),
        }
    }

    fn usable(&self) -> bool {
        video_decode_gate(
            self.api_1_3,
            self.features_ok,
            self.family.is_some(),
            self.base_missing.is_empty(),
            !self.codec_exts.is_empty(),
        )
    }
}

/// Discrete, then integrated, then everything else: the order `pick_device` tries and
/// the adapter listings print. Hybrids enumerate the iGPU first.
fn device_rank(props: &vk::PhysicalDeviceProperties) -> u8 {
    match props.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        _ => 2,
    }
}

/// One physical device's Vulkan Video decode capability: no logical device, no
/// surface. What `punktfunk-session --probe-decode` prints.
#[derive(Debug, Clone)]
pub struct AdapterDecode {
    /// Raw `vkEnumeratePhysicalDevices` index: the value `PUNKTFUNK_VK_DEVICE` takes.
    /// `pick_device` uses that unsorted index. This report is discrete-first, so the
    /// display row is not that number — hybrids enumerate the iGPU first.
    pub index: usize,
    /// Marketing name, also the `PUNKTFUNK_VK_ADAPTER` match key. Not unique: a hybrid
    /// can expose the same iGPU twice, and a match takes whoever enumerates first.
    pub name: String,
    /// Discrete-first, matching `pick_device`: row 0 is the default present device.
    pub discrete: bool,
    pub api_1_3: bool,
    pub features_ok: bool,
    pub decode_family: Option<u32>,
    /// `VkVideoCodecOperationFlagsKHR` from that family. Driver claim, independent of extensions.
    pub codec_ops: u32,
    pub base_missing: Vec<String>,
    pub codec_exts: Vec<String>,
    /// [`video_decode_gate`] over the fields above.
    pub usable: bool,
    /// Video image-format answers from [`pf_vkdecode::probe`], one row per (profile, usage).
    /// Empty when the gate already failed — no video queue to ask. A miss here is why a
    /// `usable` device still cannot host the decoder.
    pub formats: Vec<pf_vkdecode::probe::ProfileProbe>,
}

impl AdapterDecode {
    /// One reason per conjunct of [`video_decode_gate`] that failed; empty exactly when
    /// `usable`.
    pub fn why_not(&self) -> Vec<String> {
        let mut why = Vec::new();
        if !self.api_1_3 {
            why.push("device is not Vulkan 1.3".to_string());
        }
        if !self.features_ok {
            why.push(
                "missing samplerYcbcrConversion / timelineSemaphore / synchronization2".to_string(),
            );
        }
        if self.decode_family.is_none() {
            why.push("no queue family advertises VIDEO_DECODE".to_string());
        }
        if !self.base_missing.is_empty() {
            why.push(format!("missing {}", self.base_missing.join(", ")));
        }
        if self.codec_exts.is_empty() {
            why.push("no VK_KHR_video_decode_{h264,h265,av1} extension".to_string());
        }
        why
    }

    /// Every codec bit in `codec_ops`, VP9 included though no rung decodes it, so the names
    /// account for the whole mask. A bit with no name is reported by value.
    pub fn codec_names(&self) -> Vec<String> {
        const OPS: [(u32, &str); 4] = [
            (0x1, "H.264"),
            (0x2, "H.265"),
            (0x4, "AV1"),
            (0x8, "VP9 (no punktfunk rung)"),
        ];
        let mut names: Vec<String> = OPS
            .iter()
            .filter(|(bit, _)| self.codec_ops & bit != 0)
            .map(|(_, name)| (*name).to_string())
            .collect();
        let unknown = self.codec_ops & !OPS.iter().map(|(bit, _)| bit).sum::<u32>();
        if unknown != 0 {
            names.push(format!("unrecognised bits 0x{unknown:X}"));
        }
        names
    }
}

/// `VK_EXT_present_mode_fifo_latest_ready`, hand-declared: ash does not name it.
/// An unenabled driver reports the mode as the raw number `1000361000`.
///
/// FIFO that presents the latest ready image at each refresh and retires the rest.
/// Listing the mode on the surface is not permission; the device feature is.
pub(crate) mod fifo_latest_ready {
    use ash::vk;

    pub(super) const NAME: &std::ffi::CStr = c"VK_EXT_present_mode_fifo_latest_ready";
    pub(crate) const MODE: vk::PresentModeKHR = vk::PresentModeKHR::from_raw(1000361000);
    const S_TYPE: vk::StructureType = vk::StructureType::from_raw(1000361000);

    /// `VkPhysicalDevicePresentModeFifoLatestReadyFeaturesEXT`.
    /// Surface advertising is not permission; enable this at device creation.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Features {
        pub s_type: vk::StructureType,
        pub p_next: *mut std::ffi::c_void,
        pub present_mode_fifo_latest_ready: vk::Bool32,
    }

    impl Default for Features {
        fn default() -> Features {
            Features {
                s_type: S_TYPE,
                p_next: std::ptr::null_mut(),
                present_mode_fifo_latest_ready: vk::FALSE,
            }
        }
    }
}

/// `VK_KHR_present_id2` + `VK_KHR_present_wait2`, hand-declared: ash 0.38 predates them.
///
/// The per-surface successors of present-id/present-wait. A driver that cannot promise
/// the wait on every surface withholds the old pair altogether (AMD on Windows); this
/// pair is advertised per surface and asked for per swapchain. Values are the registry's
/// (`vk.xml`, extensions 480 and 481).
pub(crate) mod present_wait2 {
    use ash::vk;
    use std::ffi::c_void;

    pub(super) const ID_NAME: &std::ffi::CStr = c"VK_KHR_present_id2";
    pub(super) const WAIT_NAME: &std::ffi::CStr = c"VK_KHR_present_wait2";
    /// Loaded by name: no ash wrapper.
    pub(super) const WAIT_FN: &std::ffi::CStr = c"vkWaitForPresent2KHR";
    /// `PRESENT_ID_2_BIT_KHR | PRESENT_WAIT_2_BIT_KHR`: the swapchain opts in at create.
    pub(crate) const SWAPCHAIN_FLAGS: vk::SwapchainCreateFlagsKHR =
        vk::SwapchainCreateFlagsKHR::from_raw(0x40 | 0x80);

    const CAPS_ID: i32 = 1_000_479_000;
    const PRESENT_ID: i32 = 1_000_479_001;
    const FEATURE_ID: i32 = 1_000_479_002;
    const CAPS_WAIT: i32 = 1_000_480_000;
    const FEATURE_WAIT: i32 = 1_000_480_001;
    const WAIT_INFO: i32 = 1_000_480_002;

    /// One `VkBool32` behind an sType: the two feature structs and the two
    /// surface-capability structs share this layout.
    #[repr(C)]
    pub(super) struct Flag {
        pub s_type: vk::StructureType,
        pub p_next: *mut c_void,
        pub value: vk::Bool32,
    }

    impl Flag {
        fn new(s_type: i32, value: vk::Bool32) -> Flag {
            Flag {
                s_type: vk::StructureType::from_raw(s_type),
                p_next: std::ptr::null_mut(),
                value,
            }
        }

        /// `VkPhysicalDevicePresentId2FeaturesKHR`.
        pub(super) fn id_feature(value: vk::Bool32) -> Flag {
            Flag::new(FEATURE_ID, value)
        }

        /// `VkPhysicalDevicePresentWait2FeaturesKHR`.
        pub(super) fn wait_feature(value: vk::Bool32) -> Flag {
            Flag::new(FEATURE_WAIT, value)
        }

        /// `VkSurfaceCapabilitiesPresentId2KHR`.
        pub(super) fn id_caps() -> Flag {
            Flag::new(CAPS_ID, vk::FALSE)
        }

        /// `VkSurfaceCapabilitiesPresentWait2KHR`.
        pub(super) fn wait_caps() -> Flag {
            Flag::new(CAPS_WAIT, vk::FALSE)
        }
    }

    /// `VkPresentId2KHR`, chained onto `VkPresentInfoKHR`. Borrows `ids` by pointer: the
    /// slice must outlive the present call.
    #[repr(C)]
    pub(crate) struct PresentId2 {
        pub s_type: vk::StructureType,
        pub p_next: *const c_void,
        pub swapchain_count: u32,
        pub p_present_ids: *const u64,
    }

    impl PresentId2 {
        pub(crate) fn new(ids: &[u64]) -> PresentId2 {
            PresentId2 {
                s_type: vk::StructureType::from_raw(PRESENT_ID),
                p_next: std::ptr::null(),
                swapchain_count: ids.len() as u32,
                p_present_ids: ids.as_ptr(),
            }
        }
    }

    /// `VkPresentWait2InfoKHR`.
    #[repr(C)]
    pub(crate) struct WaitInfo {
        pub s_type: vk::StructureType,
        pub p_next: *const c_void,
        pub present_id: u64,
        pub timeout: u64,
    }

    impl WaitInfo {
        pub(crate) fn new(present_id: u64, timeout: u64) -> WaitInfo {
            WaitInfo {
                s_type: vk::StructureType::from_raw(WAIT_INFO),
                p_next: std::ptr::null(),
                present_id,
                timeout,
            }
        }
    }

    /// `vkWaitForPresent2KHR`.
    pub(crate) type WaitFn =
        unsafe extern "system" fn(vk::Device, vk::SwapchainKHR, *const WaitInfo) -> vk::Result;
}

/// What the picked device offers and what the presenter enables on it, decided before
/// the logical device exists. Every value is copied out of its query, so nothing here
/// borrows a pNext chain.
struct DeviceCaps {
    /// Name, vendor and API version, for logs and the decode export.
    dev_props: vk::PhysicalDeviceProperties,
    /// Adapter LUID so D3D11VA creates its decode device on the same GPU. Core 1.1.
    adapter_luid: Option<[u8; 8]>,
    /// All four dmabuf import extensions; without them `supports_dmabuf()` is false.
    #[cfg(target_os = "linux")]
    hw_capable: bool,
    /// On top of `hw_capable`: the decode fence as a semaphore instead of a CPU poll.
    #[cfg(target_os = "linux")]
    sync_fd_ext: bool,
    /// The D3D11 ring formats the driver reports as importable.
    #[cfg(windows)]
    import: crate::d3d11::ImportSupport,
    /// The win32 external-memory and keyed-mutex extensions, and a BGRA8 import.
    #[cfg(windows)]
    win_capable: bool,
    /// `vkSetHdrMetadataEXT`, which compositors key "this app is HDR" on. The HDR10
    /// colorspace alone still looks SDR to the shell.
    has_hdr_metadata: bool,
    /// Core features, enabled as found.
    sampler_ycbcr: bool,
    timeline_semaphore: bool,
    synchronization2: bool,
    shader_float16: bool,
    /// The FIFO_LATEST_READY device feature. The surface may list the mode with the
    /// extension disabled; the feature is the gate on requesting it.
    flr_ok: bool,
    /// Present-wait in use: the original pair or the per-surface successors, never both.
    present_wait_ok: bool,
    use_wait2: bool,
    /// Present-timing stages the engine stamps; 0 = none.
    timing_stages: u32,
    /// PyroWave: Vulkan 1.3 compute on this device, no video extensions.
    pyrowave_ok: bool,
    /// Vulkan Video decode passed [`video_decode_gate`], on `decode_qf` with `decode_caps`.
    video_ok: bool,
    decode_qf: u32,
    decode_caps: vk::VideoCodecOperationFlagsKHR,
    /// Video extensions to enable; empty without `video_ok`.
    video_ext_names: Vec<&'static CStr>,
    /// `VK_EXT_full_screen_exclusive` with per-surface queries. `Presenter::new` narrows it
    /// to an opted-in fullscreen session.
    #[cfg(windows)]
    fse_ok: bool,
}

impl DeviceCaps {
    /// Query `pdev` and decide what to enable on it. `caps2` is the per-surface query
    /// table, there when the instance enabled `VK_KHR_get_surface_capabilities2`;
    /// present-wait2, present timing and exclusive fullscreen ask through it. Logs each
    /// optional lane's verdict. `qfi` is the decode queue's fallback. The `present_wait2`
    /// and `present_timing` knobs opt into those two.
    fn probe(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        qfi: u32,
        surface: vk::SurfaceKHR,
        caps2: Option<&ash::khr::get_surface_capabilities2::Instance>,
        knobs: &Knobs,
    ) -> Result<DeviceCaps> {
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let available = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
        let has = |name: &CStr| {
            available
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };
        #[cfg(target_os = "linux")]
        let hw_capable = dmabuf::DEVICE_EXTENSIONS.iter().all(|n| has(n));
        #[cfg(target_os = "linux")]
        if !hw_capable {
            tracing::info!(
                "device lacks the dmabuf import extensions — VAAPI hardware frames \
                 unavailable"
            );
        }
        // D3D11 shared-texture import, optional like dmabuf. Extensions are not
        // enough: the driver must report the ring's BGRA8 (and, for PQ pass-through,
        // RGB10A2) as IMPORTABLE. Creating an unsupported external image is UB
        // (`VK_ERROR_DEVICE_LOST` on first submit).
        #[cfg(windows)]
        let import = crate::d3d11::import_supported(instance, pdev);
        #[cfg(windows)]
        let win_capable = crate::d3d11::DEVICE_EXTENSIONS.iter().all(|n| has(n)) && import.bgra8;
        #[cfg(windows)]
        if !win_capable {
            tracing::info!(
                "device lacks the win32 external-memory/keyed-mutex extensions — D3D11VA \
                 hardware frames unavailable"
            );
        }
        let mut id_props = vk::PhysicalDeviceIDProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id_props);
        // SAFETY: read-only query; `id_props` / `props2` outlive the call.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
        let adapter_luid = (id_props.device_luid_valid == vk::TRUE).then_some(id_props.device_luid);

        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let dev_props = unsafe { instance.get_physical_device_properties(pdev) };
        let decode = DecodeProbe::of(instance, pdev, has);
        let mut have_pid = vk::PhysicalDevicePresentIdFeaturesKHR::default();
        let mut have_pwait = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
        let mut have_f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut have_f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut have_f13 = vk::PhysicalDeviceVulkan13Features::default();
        let present_wait_exts =
            has(ash::khr::present_id::NAME) && has(ash::khr::present_wait::NAME);
        let mut have_f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut have_f11)
            .push_next(&mut have_f12)
            .push_next(&mut have_f13);
        if present_wait_exts {
            have_f2 = have_f2.push_next(&mut have_pid).push_next(&mut have_pwait);
        }
        // SAFETY: read-only query; the pNext chain locals outlive the call.
        unsafe { instance.get_physical_device_features2(pdev, &mut have_f2) };
        // Copy shader_int16 out now: `have_f2` mutably borrows the pNext chain, so
        // later reads of chained structs must come after this last use of `have_f2`.
        let have_shader_int16 = have_f2.features.shader_int16;
        let flr_ok = has(fifo_latest_ready::NAME) && fifo_latest_ready_enabled(instance, pdev);
        let present_wait_ok = present_wait_exts
            && have_pid.present_id == vk::TRUE
            && have_pwait.present_wait == vk::TRUE;
        let present_wait2_ok = match caps2 {
            Some(caps2) if has(present_wait2::ID_NAME) && has(present_wait2::WAIT_NAME) => {
                present_wait2_offered(instance, pdev, surface, caps2)
            }
            _ => false,
        };
        // The engine's own display stamps ride on present-id2.
        let timing_stages = match caps2 {
            Some(caps2)
                if present_wait2_ok
                    && knobs.present_timing.unwrap_or(cfg!(target_os = "linux"))
                    && has(timing_ext::NAME)
                    && has(ash::khr::calibrated_timestamps::NAME) =>
            {
                timing_stages_offered(instance, pdev, surface, caps2)
            }
            _ => 0,
        };
        let (present_wait_ok, use_wait2) = present_wait_generation(
            present_wait_ok,
            present_wait2_ok,
            knobs.present_wait2 || timing_stages != 0,
        );
        // PyroWave is Vulkan 1.3 compute on this device — no video extensions.
        // Probe here so a capable device enables the features and advertises the codec.
        let pyrowave_ok = decode.api_1_3
            && have_shader_int16 == vk::TRUE
            && have_f12.storage_buffer8_bit_access == vk::TRUE
            && have_f12.timeline_semaphore == vk::TRUE
            && have_f13.subgroup_size_control == vk::TRUE
            && have_f13.compute_full_subgroups == vk::TRUE
            && have_f13.synchronization2 == vk::TRUE;
        let (decode_qf, decode_caps) = decode.family.unwrap_or((qfi, Default::default()));
        let video_ext_names = video_ext_names(&decode, has, &dev_props, decode_caps, decode_qf);
        Ok(DeviceCaps {
            dev_props,
            adapter_luid,
            #[cfg(target_os = "linux")]
            hw_capable,
            #[cfg(target_os = "linux")]
            sync_fd_ext: hw_capable && has(ash::khr::external_semaphore_fd::NAME),
            #[cfg(windows)]
            import,
            #[cfg(windows)]
            win_capable,
            has_hdr_metadata: has(ash::ext::hdr_metadata::NAME),
            sampler_ycbcr: have_f11.sampler_ycbcr_conversion == vk::TRUE,
            timeline_semaphore: have_f12.timeline_semaphore == vk::TRUE,
            synchronization2: have_f13.synchronization2 == vk::TRUE,
            shader_float16: have_f12.shader_float16 == vk::TRUE,
            flr_ok,
            present_wait_ok,
            use_wait2,
            timing_stages,
            pyrowave_ok,
            video_ok: decode.usable(),
            decode_qf,
            decode_caps,
            video_ext_names,
            #[cfg(windows)]
            fse_ok: caps2.is_some() && has(ash::ext::full_screen_exclusive::NAME),
        })
    }

    /// The device extensions to enable.
    fn dev_exts(&self) -> Vec<&'static CStr> {
        let mut exts = vec![ash::khr::swapchain::NAME];
        #[cfg(target_os = "linux")]
        if self.hw_capable {
            exts.extend(dmabuf::DEVICE_EXTENSIONS);
            if self.sync_fd_ext {
                exts.push(ash::khr::external_semaphore_fd::NAME);
            }
        }
        #[cfg(windows)]
        if self.win_capable {
            exts.extend(crate::d3d11::DEVICE_EXTENSIONS);
            if self.import.fence {
                exts.push(crate::d3d11::FENCE_EXTENSION);
            }
        }
        if self.has_hdr_metadata {
            exts.push(ash::ext::hdr_metadata::NAME);
        }
        exts.extend(&self.video_ext_names);
        // Present-wait on: PresentTimer stamps on-glass; otherwise the stamp is submit-time.
        if self.present_wait_ok {
            exts.extend([ash::khr::present_id::NAME, ash::khr::present_wait::NAME]);
        }
        if self.use_wait2 {
            exts.extend([present_wait2::ID_NAME, present_wait2::WAIT_NAME]);
        }
        if self.timing_stages != 0 {
            exts.extend([timing_ext::NAME, ash::khr::calibrated_timestamps::NAME]);
        }
        if self.flr_ok {
            exts.push(fifo_latest_ready::NAME);
        }
        #[cfg(windows)]
        if self.fse_ok {
            exts.push(ash::ext::full_screen_exclusive::NAME);
        }
        exts
    }
}

/// The present-wait generation to enable, as (present-wait, present-wait2). The successors
/// win when asked for, or wherever the engine stamps its presents, since those stamps need
/// present-id2; the original pair then stays off. AMD's Windows driver completes the
/// original wait late enough that the glass gate drops frames; without the successors the
/// vblank waiter runs.
fn present_wait_generation(wait: bool, wait2: bool, wait2_wanted: bool) -> (bool, bool) {
    let use_wait2 = wait2 && wait2_wanted;
    (wait && !use_wait2, use_wait2)
}

/// The FIFO_LATEST_READY device feature, on a device that lists the extension.
fn fifo_latest_ready_enabled(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> bool {
    let mut feat = fifo_latest_ready::Features::default();
    let mut probe = vk::PhysicalDeviceFeatures2 {
        p_next: (&mut feat) as *mut _ as *mut std::ffi::c_void,
        ..Default::default()
    };
    // SAFETY: read-only query; `feat` is the pNext target and outlives the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut probe) };
    feat.present_mode_fifo_latest_ready == vk::TRUE
}

/// The per-surface successors: the device features, then what this surface promises. On
/// a device that lists both extensions.
fn present_wait2_offered(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    caps2_i: &ash::khr::get_surface_capabilities2::Instance,
) -> bool {
    let mut wait = present_wait2::Flag::wait_feature(vk::FALSE);
    let mut id = present_wait2::Flag::id_feature(vk::FALSE);
    id.p_next = (&mut wait) as *mut _ as *mut std::ffi::c_void;
    let mut probe = vk::PhysicalDeviceFeatures2 {
        p_next: (&mut id) as *mut _ as *mut std::ffi::c_void,
        ..Default::default()
    };
    // SAFETY: read-only query; the chained locals outlive the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut probe) };
    let mut wait_caps = present_wait2::Flag::wait_caps();
    let mut id_caps = present_wait2::Flag::id_caps();
    id_caps.p_next = (&mut wait_caps) as *mut _ as *mut std::ffi::c_void;
    let mut caps2 = vk::SurfaceCapabilities2KHR {
        p_next: (&mut id_caps) as *mut _ as *mut std::ffi::c_void,
        ..Default::default()
    };
    let surface_info = vk::PhysicalDeviceSurfaceInfo2KHR::default().surface(surface);
    // SAFETY: live handles; the chained locals outlive the call.
    let queried = unsafe {
        caps2_i.get_physical_device_surface_capabilities2(pdev, &surface_info, &mut caps2)
    }
    .is_ok();
    queried
        && id.value == vk::TRUE
        && wait.value == vk::TRUE
        && id_caps.value == vk::TRUE
        && wait_caps.value == vk::TRUE
}

/// The present-timing stages the engine stamps on this surface: the device feature, then
/// the stages the surface reports. 0 = none. On a device that lists the extension and
/// calibrated timestamps.
fn timing_stages_offered(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    caps2_i: &ash::khr::get_surface_capabilities2::Instance,
) -> u32 {
    let mut feat = timing_ext::Features::new(vk::FALSE);
    let mut probe = vk::PhysicalDeviceFeatures2 {
        p_next: (&mut feat) as *mut _ as *mut std::ffi::c_void,
        ..Default::default()
    };
    // SAFETY: read-only query; `feat` is the pNext target and outlives the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut probe) };
    let mut caps = timing_ext::SurfaceCaps::default();
    let mut caps2 = vk::SurfaceCapabilities2KHR {
        p_next: (&mut caps) as *mut _ as *mut std::ffi::c_void,
        ..Default::default()
    };
    let surface_info = vk::PhysicalDeviceSurfaceInfo2KHR::default().surface(surface);
    // SAFETY: live handles; the chained locals outlive the call.
    let queried = unsafe {
        caps2_i.get_physical_device_surface_capabilities2(pdev, &surface_info, &mut caps2)
    }
    .is_ok();
    let stages = caps.present_stage_queries
        & (timing_ext::STAGE_PIXEL_OUT | timing_ext::STAGE_PIXEL_VISIBLE);
    tracing::info!(
        feature = feat.present_timing == vk::TRUE,
        surface = caps.present_timing_supported == vk::TRUE,
        stages = format_args!("{:#x}", caps.present_stage_queries),
        at_absolute_time = caps.present_at_absolute_time_supported == vk::TRUE,
        at_relative_time = caps.present_at_relative_time_supported == vk::TRUE,
        "engine display stamps (VK_EXT_present_timing)"
    );
    if queried && feat.present_timing == vk::TRUE && caps.present_timing_supported == vk::TRUE {
        stages
    } else {
        0
    }
}

/// The Vulkan Video extensions to enable, logged either way: what decode runs on, or every
/// conjunct of why it cannot. Empty when `decode` fails the gate.
fn video_ext_names(
    decode: &DecodeProbe,
    has: impl Fn(&CStr) -> bool,
    dev_props: &vk::PhysicalDeviceProperties,
    decode_caps: vk::VideoCodecOperationFlagsKHR,
    decode_qf: u32,
) -> Vec<&'static CStr> {
    let mut video_ext_names: Vec<&'static CStr> = Vec::new();
    if decode.usable() {
        video_ext_names.extend(VIDEO_BASE);
        video_ext_names.extend(&decode.codec_exts);
        // Optional; pf-vkdecode probes these rather than requiring them.
        for opt in [c"VK_KHR_video_maintenance1", c"VK_KHR_video_maintenance2"] {
            if has(opt) {
                video_ext_names.push(opt);
            }
        }
        tracing::info!(
            decode_qf,
            caps = ?decode_caps,
            exts = ?video_ext_names,
            "Vulkan Video decode available on this device"
        );
        return video_ext_names;
    }
    // Log every conjunct. Empty `codec_exts` next to non-empty `queue_codec_ops`
    // means the extensions are missing, not the hardware.
    let base_missing: Vec<&str> = decode
        .base_missing
        .iter()
        .map(|n| n.to_str().unwrap_or("?"))
        .collect();
    let codec_ext_names: Vec<&str> = decode
        .codec_exts
        .iter()
        .map(|n| n.to_str().unwrap_or("?"))
        .collect();
    tracing::info!(
        dev_is_13 = decode.api_1_3,
        features_ok = decode.features_ok,
        decode_family = decode.family.is_some(),
        video_base_missing = ?base_missing,
        codec_exts_present = ?codec_ext_names,
        queue_codec_ops = ?decode.family.map(|(_, ops)| ops),
        device = %dev_props
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default(),
        vendor_id = format_args!("0x{:04X}", dev_props.vendor_id),
        "Vulkan Video decode unavailable on this device — the decoder falls back \
         one rung (D3D11VA on Windows, VAAPI on Linux, then software)"
    );
    video_ext_names
}

impl Presenter {
    /// Instance → surface → device pick → `DeviceCaps` → device → swapchain over an SDL
    /// window. `instance_extensions` is `VideoSubsystem::vulkan_instance_extensions()`;
    /// `knobs` are the shell's, kept for the per-frame and rebuild paths. Each object lands
    /// in a [`Bringup`] as it is made, so a failed step destroys everything before it.
    pub(crate) fn new(
        window: &sdl3::video::Window,
        instance_extensions: &[String],
        pref: PresentPref,
        knobs: &Knobs,
    ) -> Result<Presenter> {
        // SAFETY: dlopens libvulkan; no instance exists yet.
        let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;

        let (instance, instance_extensions, has_colorspace_ext, has_caps2_ext) =
            create_instance(&entry, instance_extensions)?;
        #[cfg(windows)]
        let want_fse = knobs.fullscreen_exclusive;
        let surface_i = ash::khr::surface::Instance::new(&entry, &instance);
        // Declared after `entry`, so libvulkan outlives its `Drop`.
        let mut b = Bringup::default();
        b.instance = Some(instance.clone());
        b.surface_i = Some(surface_i.clone());

        // SAFETY: CREATE — `instance` is live; SDL returns a surface we own and later destroy.
        let surface = unsafe { window.vulkan_create_surface(instance.handle()) }
            .map_err(|e| anyhow!("SDL_Vulkan_CreateSurface: {e}"))?;
        b.surface = surface;

        let (pdev, qfi) = pick_device(&instance, &surface_i, surface, knobs)?;
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        {
            let mut driver = vk::PhysicalDeviceDriverProperties::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
            // SAFETY: read-only query on the live instance; `pdev` was enumerated from it,
            // and `driver` / `props` outlive the call.
            unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
            let lossy = |c: Result<&std::ffi::CStr, _>| {
                c.map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            // The driver and its version name a software or foreign ICD in one line.
            tracing::info!(
                device = %lossy(props.properties.device_name_as_c_str()),
                kind = ?props.properties.device_type,
                driver = %lossy(driver.driver_name_as_c_str()),
                version = %lossy(driver.driver_info_as_c_str()),
                queue_family = qfi,
                "vulkan device"
            );
        }

        let caps2_i = has_caps2_ext
            .then(|| ash::khr::get_surface_capabilities2::Instance::new(&entry, &instance));
        let caps = DeviceCaps::probe(&instance, pdev, qfi, surface, caps2_i.as_ref(), knobs)?;
        // Exclusive fullscreen, opt-in and fullscreen sessions only.
        #[cfg(windows)]
        let caps = DeviceCaps {
            fse_ok: caps.fse_ok && want_fse && pref.fullscreen,
            ..caps
        };
        let (device, queue_families) = create_device(&instance, pdev, qfi, &caps)?;
        b.device = Some(device.clone());
        let swap_d = ash::khr::swapchain::Device::new(&instance, &device);
        let (timing, present_timer, present_id2) = present_timing(&instance, &device, &caps);
        // No present-wait in use: the output's vblank stands in for the ledger and the VRR
        // verdict; the ledger says `glass=est`.
        #[cfg(windows)]
        let vblank_timer = present_timer
            .is_none()
            .then(|| crate::win32::window_monitor(window))
            .flatten()
            .and_then(|m| super::vblank_timing::VblankTimer::spawn(device.clone(), m));
        #[cfg(windows)]
        if present_timer.is_none() {
            tracing::info!(
                active = vblank_timer.is_some(),
                "glass clock from the output's vblank (estimated stamps)"
            );
        }
        b.made.present_timer = present_timer;
        #[cfg(windows)]
        {
            b.made.vblank_timer = vblank_timer;
        }
        #[cfg(windows)]
        let fse = caps
            .fse_ok
            .then(|| crate::win32::window_monitor(window))
            .flatten()
            .map(|monitor| super::FullScreenExclusive {
                device: ash::ext::full_screen_exclusive::Device::new(&instance, &device),
                instance: ash::ext::full_screen_exclusive::Instance::new(&entry, &instance),
                caps2: ash::khr::get_surface_capabilities2::Instance::new(&entry, &instance),
                monitor,
            });
        #[cfg(windows)]
        if want_fse {
            tracing::info!(
                active = fse.is_some(),
                fullscreen = pref.fullscreen,
                "exclusive fullscreen (VK_EXT_full_screen_exclusive)"
            );
        }
        let hdr_metadata_d = caps
            .has_hdr_metadata
            .then(|| ash::ext::hdr_metadata::Device::new(&instance, &device));
        // SAFETY: `device` is live; queue 0 of `qfi` was requested at create.
        let queue = unsafe { device.get_device_queue(qfi, 0) };
        #[cfg(target_os = "linux")]
        {
            b.made.hw = caps
                .hw_capable
                .then(|| hw_ctx(&instance, pdev, &device, &caps));
        }
        #[cfg(windows)]
        let hw_win = caps.win_capable.then(|| HwCtxWin {
            ext_mem_win32: ash::khr::external_memory_win32::Device::new(&instance, &device),
            ext_sem_win32: caps
                .import
                .fence
                .then(|| ash::khr::external_semaphore_win32::Device::new(&instance, &device)),
            imports: crate::d3d11::ImportCache::default(),
            reads: 0,
            parked: None,
        });
        b.made.csc = CscPass::new(&device, super::VIDEO_FORMAT)?;
        // Writes the same intermediate as `csc`. Always built: the software decode rung
        // renders through it. Gating on the pyrowave probe would hide software frames.
        b.made.csc_planar = CscPass::new_planar(&device, super::VIDEO_FORMAT)?;

        // Export the selected device facts when any consumer needs the handles; on Linux
        // always, since every consumer gates its lane on the booleans. One `queue_lock` per
        // device (decode + Skia + presenter; see its docs).
        let queue_lock = std::sync::Arc::new(pf_client_core::video::QueueLock::new());
        queue_lock.take_turns();
        #[cfg(windows)]
        let export_worthy = caps.video_ok || caps.win_capable || caps.pyrowave_ok;
        #[cfg(target_os = "linux")]
        let export_worthy = true;
        let video_export = export_worthy.then(|| {
            decode_export(
                &entry,
                &instance,
                pdev,
                &device,
                &caps,
                qfi,
                queue_families,
                &instance_extensions,
                queue_lock.clone(),
            )
        });
        #[cfg(windows)]
        let mut video_export = video_export;

        let (format, hdr10_format) =
            pick_formats(&surface_i, pdev, surface, has_colorspace_ext, knobs)?;
        // D3D11VA may emit its RGB10 PQ ring only when this device imports 10-bit and the
        // surface offers an HDR10 swapchain. Planar NV12/P010 slots need the planar import
        // and a vendor that survives it (`planar_allowed`).
        #[cfg(windows)]
        if let Some(v) = video_export.as_mut() {
            v.d3d11_hdr10 = caps.win_capable && caps.import.rgb10 && hdr10_format.is_some();
            let planar =
                caps.win_capable && crate::d3d11::planar_allowed(v.vendor_id, knobs.d3d11_planar);
            v.d3d11_nv12 = planar && caps.import.nv12;
            v.d3d11_p010 = planar && caps.import.p010;
        }
        let mut pref = pref;
        pref.vrr_fifo_opt_in = knobs.vrr_fifo;
        pref.fifo_latest_ready = caps.flr_ok;
        let pinned = knobs.present_mode.as_deref();
        let present_mode = pick_present_mode(&surface_i, pdev, surface, pref, pinned)?;
        tracing::info!(
            ?format,
            ?hdr10_format,
            ?present_mode,
            vsync = pref.vsync,
            allow_vrr = pref.allow_vrr,
            fifo_latest_ready = caps.flr_ok,
            hdr_metadata = caps.has_hdr_metadata,
            "swapchain config"
        );
        b.made.overlay_pipe = OverlayPipe::new(&device, format.format, false)?;
        b.made.scale = crate::scale::ScalePass::new(&device, format.format)?;
        b.made.direct = crate::csc::DirectPass::new(
            &device,
            format.format,
            b.made.csc.pass.pipeline_layout,
            b.made.csc_planar.pass.pipeline_layout,
        )?;
        frame_sync(&device, qfi, caps.timeline_semaphore, &mut b.made)?;

        // Every fallible step is behind: the presenter takes each object over.
        let m = b.into_parts();
        #[cfg(target_os = "linux")]
        let native_timelines = m.hw.as_ref().is_some_and(|h| h.timelines.is_some());
        let mut p = Presenter {
            entry,
            instance,
            surface_i,
            surface,
            pdev,
            mem_props,
            device,
            swap_d,
            queue,
            qfi,
            #[cfg(target_os = "linux")]
            hw: m.hw,
            #[cfg(windows)]
            hw_win,
            csc: m.csc,
            csc_planar: m.csc_planar,
            csc_ycbcr: CscPass::default(),
            csc_ycbcr_format: vk::Format::UNDEFINED,
            cpu_planes: None,
            video_export,
            overlay_pipe: m.overlay_pipe,
            scale: m.scale,
            direct: m.direct,
            cpu_direct: None,
            retired_hw: None,
            #[cfg(windows)]
            retained_slot: None,
            last_import_us: 0,
            last_submit_us: 0,
            last_fence_us: 0,
            last_acquire_us: 0,
            last_present_us: 0,
            queue_lock,
            hdr10_format,
            hdr_active: false,
            hdr_downgrade_warned: false,
            hdr_metadata_d,
            hdr_meta: None,
            pref,
            present_mode,
            targets: super::targets::Targets {
                format,
                ..Default::default()
            },
            acquire_sem: m.acquire_sem,
            done_sem: m.done_sem,
            fence: m.fence,
            cmd_pool: m.cmd_pool,
            cmd_buf: m.cmd_buf,
            staging: None,
            video: None,
            owner: crate::reconfig_model::Owner::Swapchain,
            state: crate::swapchain_state::SwapchainState::new(),
            glass: {
                let glass = crate::glass::GlassClock::new(timing, m.present_timer, present_id2);
                #[cfg(windows)]
                let glass = glass.with_vblank(m.vblank_timer);
                #[cfg(target_os = "linux")]
                let glass = glass.with_feedback(if knobs.surface_feedback {
                    crate::wl_feedback::SurfaceFeedback::new(window).unwrap_or_else(|e| {
                        tracing::warn!(error = %format!("{e:#}"), "surface feedback unavailable");
                        None
                    })
                } else {
                    None
                });
                glass
            },
            #[cfg(windows)]
            fse,
            video_fit: Default::default(),
            placement_logged: None,
            #[cfg(target_os = "linux")]
            native: if crate::wl_native::enabled() {
                crate::wl_native::NativeLane::new(window, native_timelines).unwrap_or_else(|e| {
                    tracing::warn!(error = %format!("{e:#}"), "native scanout lane unavailable");
                    None
                })
            } else {
                None
            },
            #[cfg(target_os = "linux")]
            export_ring: None,
            #[cfg(target_os = "linux")]
            export_refused: None,
            #[cfg(target_os = "linux")]
            export_gen: 0,
            #[cfg(target_os = "linux")]
            overlay_ring: None,
            #[cfg(target_os = "linux")]
            overlay_refused: None,
            #[cfg(target_os = "linux")]
            overlay_shown: None,
            #[cfg(target_os = "linux")]
            vaapi_sync: Default::default(),
            #[cfg(target_os = "linux")]
            native_keys: Vec::new(),
            #[cfg(target_os = "linux")]
            native_flip: (knobs.native_scanout.as_deref() == Some("flip"))
                .then(std::time::Instant::now),
            overlay_blocks_native: false,
            knobs: knobs.clone(),
        };
        p.recreate_swapchain(window)?;
        Ok(p)
    }
}

/// The instance with SDL's extensions, plus the colorspace and per-surface query
/// extensions where the loader has them. Returns the enabled list and those two answers.
fn create_instance(
    entry: &ash::Entry,
    instance_extensions: &[String],
) -> Result<(ash::Instance, Vec<String>, bool, bool)> {
    let app_name = CString::new("punktfunk-session").unwrap();
    // 1.3: Video decode and PyroWave both need it. The instance version caps
    // what the device can report; `SharedDevice::api_version` is this same constant.
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(super::INSTANCE_API_VERSION);
    // HDR10 needs `VK_EXT_swapchain_colorspace` at instance level, not device.
    let mut instance_extensions: Vec<String> = instance_extensions.to_vec();
    let inst_available =
        // SAFETY: read-only loader query; no instance yet; result by value.
        unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
    let has_colorspace_ext = inst_available
        .iter()
        .any(|e| e.extension_name_as_c_str() == Ok(c"VK_EXT_swapchain_colorspace"));
    if has_colorspace_ext {
        instance_extensions.push("VK_EXT_swapchain_colorspace".into());
    }
    // Per-surface queries (present-wait2, exclusive fullscreen) go through
    // `VK_KHR_get_surface_capabilities2`.
    let has_caps2_ext = inst_available
        .iter()
        .any(|e| e.extension_name_as_c_str() == Ok(ash::khr::get_surface_capabilities2::NAME));
    if has_caps2_ext {
        instance_extensions.push("VK_KHR_get_surface_capabilities2".into());
    }
    let ext_cstrings: Vec<CString> = instance_extensions
        .iter()
        .map(|e| CString::new(e.as_str()).unwrap())
        .collect();
    // `c_char`, not `i8`: `char` is signed on x86_64 and unsigned on aarch64, so
    // `*const i8` compiles on the desktop and fails to match ash on ARM.
    let ext_ptrs: Vec<*const c_char> = ext_cstrings.iter().map(|e| e.as_ptr()).collect();
    // SAFETY: CREATE — `app_info` and `ext_ptrs` outlive the call; we own the instance.
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app_info)
                .enabled_extension_names(&ext_ptrs),
            None,
        )
    }
    .context("vkCreateInstance")?;
    Ok((
        instance,
        instance_extensions,
        has_colorspace_ext,
        has_caps2_ext,
    ))
}

/// The logical device with what `caps` enables: extensions, the feature chain, and a
/// decode queue where it is another family. Returns the queue families it asked for.
fn create_device(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    qfi: u32,
    caps: &DeviceCaps,
) -> Result<(ash::Device, Vec<u32>)> {
    let dev_exts: Vec<*const c_char> = caps.dev_exts().iter().map(|n| n.as_ptr()).collect();
    let mut en_flr = fifo_latest_ready::Features {
        present_mode_fifo_latest_ready: vk::TRUE,
        ..Default::default()
    };
    let mut en_pid = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
    let mut en_pwait = vk::PhysicalDevicePresentWaitFeaturesKHR::default().present_wait(true);

    let mut en_f11 =
        vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(caps.sampler_ycbcr);
    let mut en_f12 = vk::PhysicalDeviceVulkan12Features::default()
        .timeline_semaphore(caps.timeline_semaphore)
        .storage_buffer8_bit_access(caps.pyrowave_ok)
        .shader_float16(caps.pyrowave_ok && caps.shader_float16);
    let mut en_f13 = vk::PhysicalDeviceVulkan13Features::default()
        .synchronization2(caps.synchronization2)
        .subgroup_size_control(caps.pyrowave_ok)
        .compute_full_subgroups(caps.pyrowave_ok);
    let mut en_f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut en_f11)
        .push_next(&mut en_f12)
        .push_next(&mut en_f13);
    if caps.present_wait_ok {
        en_f2 = en_f2.push_next(&mut en_pid).push_next(&mut en_pwait);
    }
    if caps.flr_ok {
        // Hand-rolled struct: splice onto the pNext list head by hand.
        en_flr.p_next = en_f2.p_next;
        en_f2.p_next = (&mut en_flr) as *mut _ as *mut std::ffi::c_void;
    }
    let mut en_wait2 = present_wait2::Flag::wait_feature(vk::TRUE);
    let mut en_id2 = present_wait2::Flag::id_feature(vk::TRUE);
    if caps.use_wait2 {
        // Hand-rolled structs again: id2 → wait2 → the rest of the chain.
        en_wait2.p_next = en_f2.p_next;
        en_id2.p_next = (&mut en_wait2) as *mut _ as *mut std::ffi::c_void;
        en_f2.p_next = (&mut en_id2) as *mut _ as *mut std::ffi::c_void;
    }
    let mut en_timing = timing_ext::Features::new(vk::TRUE);
    if caps.timing_stages != 0 {
        en_timing.p_next = en_f2.p_next;
        en_f2.p_next = (&mut en_timing) as *mut _ as *mut std::ffi::c_void;
    }
    en_f2.features.shader_int16 = vk::Bool32::from(caps.pyrowave_ok);

    let priorities = [1.0f32];
    let mut queue_info = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(qfi)
        .queue_priorities(&priorities)];
    if caps.video_ok && caps.decode_qf != qfi {
        queue_info.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(caps.decode_qf)
                .queue_priorities(&priorities),
        );
    }
    // SAFETY: CREATE — `queue_info`, `dev_exts`, and `en_f2` outlive the call; we own the device.
    let device = unsafe {
        instance.create_device(
            pdev,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&dev_exts)
                .push_next(&mut en_f2),
            None,
        )
    }
    .context("vkCreateDevice")?;
    let families = queue_info.iter().map(|q| q.queue_family_index).collect();
    Ok((device, families))
}

/// The present-wait waiter of the generation `caps` picked, and the engine's own stamps
/// where that waiter reads them. Returns (stamps, waiter, present-id2 in use).
fn present_timing(
    instance: &ash::Instance,
    device: &ash::Device,
    caps: &DeviceCaps,
) -> (
    Option<std::sync::Arc<timing_ext::Engine>>,
    Option<super::present_timing::PresentTimer>,
    bool,
) {
    use super::present_timing::{PresentTimer, Waiter};
    // SAFETY: the device was created with the extension and calibrated timestamps.
    let timing = (caps.timing_stages != 0)
        .then(|| unsafe { timing_ext::Engine::load(instance, device, caps.timing_stages) })
        .flatten()
        .map(std::sync::Arc::new);
    let present_timer = if caps.present_wait_ok {
        let wait_d = ash::khr::present_wait::Device::new(instance, device);
        Some(PresentTimer::spawn(
            Waiter::V1(wait_d),
            device.clone(),
            None,
        ))
    } else if caps.use_wait2 {
        // SAFETY: a name lookup on the live device; `None` if the driver has no such entry.
        let raw = unsafe {
            instance.get_device_proc_addr(device.handle(), present_wait2::WAIT_FN.as_ptr())
        };
        raw.map(|f| {
            // SAFETY: the registry declares `vkWaitForPresent2KHR` with this signature.
            let wait = unsafe {
                std::mem::transmute::<unsafe extern "system" fn(), present_wait2::WaitFn>(f)
            };
            let waiter = Waiter::V2 {
                device: device.handle(),
                wait,
            };
            PresentTimer::spawn(waiter, device.clone(), timing.clone())
        })
    } else {
        None
    };
    // The swapchain and every present opt into the successors only with a live waiter.
    let present_id2 = caps.use_wait2 && present_timer.is_some();
    // The engine's stamps are read by that waiter.
    let timing = timing.filter(|_| present_id2);
    tracing::info!(
        present_wait = caps.present_wait_ok,
        present_wait2 = present_id2,
        engine_stamps = timing.is_some(),
        "on-glass present timing (VK_KHR_present_wait / present_wait2)"
    );
    (timing, present_timer, present_id2)
}

/// Dmabuf import on a device with all four import extensions: the decode fence as a
/// semaphore where it can be one, and exportable timelines for the native lane.
#[cfg(target_os = "linux")]
fn hw_ctx(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    device: &ash::Device,
    caps: &DeviceCaps,
) -> HwCtx {
    let sync = caps
        .sync_fd_ext
        .then(|| crate::dmabuf::SyncImport::new(instance, pdev, device))
        .flatten();
    tracing::info!(
        semaphore = sync.is_some(),
        "dmabuf decode sync (a decode fence the sampling submit waits; \
         `false` polls the fence on the presenter thread)"
    );
    // SAFETY: live, paired handles; the extension and feature are checked alongside.
    let timelines = (caps.sync_fd_ext && caps.timeline_semaphore)
        .then(|| unsafe { super::sync_timeline::TimelineMaker::new(instance, pdev, device) })
        .flatten();
    HwCtx {
        ext_mem_fd: ash::khr::external_memory_fd::Device::new(instance, device),
        modifier_cache: Default::default(),
        imports: Default::default(),
        sync,
        timelines,
    }
}

/// The selected device's facts and handles for the decode lanes. Extension lists match
/// creation: the pyrowave decoder replays them into its pinned create-info.
#[allow(clippy::too_many_arguments)]
fn decode_export(
    entry: &ash::Entry,
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    device: &ash::Device,
    caps: &DeviceCaps,
    qfi: u32,
    queue_families: Vec<u32>,
    instance_extensions: &[String],
    queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
) -> pf_client_core::video::VulkanDecodeDevice {
    let mut device_extensions: Vec<CString> = vec![CString::from(ash::khr::swapchain::NAME)];
    #[cfg(target_os = "linux")]
    if caps.hw_capable {
        device_extensions.extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| CString::from(*n)));
    }
    #[cfg(windows)]
    if caps.win_capable {
        device_extensions.extend(
            crate::d3d11::DEVICE_EXTENSIONS
                .iter()
                .map(|n| CString::from(*n)),
        );
    }
    if caps.has_hdr_metadata {
        device_extensions.push(CString::from(ash::ext::hdr_metadata::NAME));
    }
    device_extensions.extend(caps.video_ext_names.iter().map(|n| CString::from(*n)));
    let decode_ops = pf_client_core::video::usable_decode_ops(
        caps.dev_props.vendor_id,
        caps.decode_caps.as_raw(),
    );
    pf_client_core::video::VulkanDecodeDevice {
        get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
        instance: instance.handle().as_raw() as usize,
        physical_device: pdev.as_raw() as usize,
        device: device.handle().as_raw() as usize,
        vendor_id: caps.dev_props.vendor_id,
        device_name: caps
            .dev_props
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default(),
        graphics_qf: qfi,
        decode_qf: caps.decode_qf,
        decode_video_caps: decode_ops,
        instance_extensions: instance_extensions
            .iter()
            .map(|e| CString::new(e.as_str()).unwrap())
            .collect(),
        device_extensions,
        f_sampler_ycbcr: caps.sampler_ycbcr,
        f_timeline_semaphore: caps.timeline_semaphore,
        f_synchronization2: caps.synchronization2,
        f_shader_int16: caps.pyrowave_ok,
        f_storage_buffer8: caps.pyrowave_ok,
        f_subgroup_size_control: caps.pyrowave_ok,
        f_compute_full_subgroups: caps.pyrowave_ok,
        f_shader_float16: caps.pyrowave_ok && caps.shader_float16,
        api_version: caps.dev_props.api_version,
        queue_families,
        pyrowave_decode: caps.pyrowave_ok,
        video_decode: caps.video_ok,
        d3d11_present: false,
        #[cfg(windows)]
        d3d11_fence: caps.win_capable && caps.import.fence,
        #[cfg(not(windows))]
        d3d11_fence: false,
        #[cfg(windows)]
        d3d11_device: None,
        #[cfg(windows)]
        d3d11_import: caps.win_capable,
        #[cfg(not(windows))]
        d3d11_import: false,
        #[cfg(target_os = "linux")]
        dmabuf_import: caps.hw_capable,
        #[cfg(not(target_os = "linux"))]
        dmabuf_import: false,
        #[cfg(target_os = "linux")]
        vaapi_av1_decode: caps.hw_capable
            && pf_client_core::video::vaapi_av1_decodable(
                caps.dev_props.vendor_id,
                caps.video_ok
                    && decode_ops & vk::VideoCodecOperationFlagsKHR::DECODE_AV1.as_raw() != 0,
            ),
        #[cfg(not(target_os = "linux"))]
        vaapi_av1_decode: false,
        #[cfg(target_os = "linux")]
        vaapi_hevc_decode: caps.hw_capable
            && pf_client_core::video::vaapi_hevc_decodable(
                caps.dev_props.vendor_id,
                caps.video_ok
                    && decode_ops & vk::VideoCodecOperationFlagsKHR::DECODE_H265.as_raw() != 0,
            ),
        #[cfg(not(target_os = "linux"))]
        vaapi_hevc_decode: false,
        // `Presenter::new` sets the HDR10 surface facts after `pick_formats`.
        d3d11_hdr10: false,
        d3d11_nv12: false,
        d3d11_p010: false,
        adapter_luid: caps.adapter_luid,
        queue_lock,
    }
}

/// The one command buffer, the acquire semaphore, the `done_sem` timeline (null without
/// `timeline`), and the submit fence, each into `made` as it is created.
fn frame_sync(device: &ash::Device, qfi: u32, timeline: bool, made: &mut Made) -> Result<()> {
    // SAFETY: CREATE — CreateInfo is a local; the pool is owned by the bring-up.
    made.cmd_pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(qfi),
            None,
        )
    }?;
    // SAFETY: ALLOCATE — the pool is ours and empty; we take the single primary buffer.
    made.cmd_buf = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(made.cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }?[0];
    made.acquire_sem =
        // SAFETY: CREATE — CreateInfo is a local; the handle is owned by the bring-up.
        unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
    let mut timeline_type = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    // Null without the timeline feature: the waiter then keeps latch whole.
    if timeline {
        // SAFETY: CREATE — CreateInfo chains a local; the handle is owned by the bring-up.
        made.done_sem = unsafe {
            device.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline_type),
                None,
            )
        }?;
    }
    // SAFETY: CREATE — CreateInfo is a local; SIGNALED so the first wait is a no-op.
    made.fence = unsafe {
        device.create_fence(
            &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
            None,
        )
    }?;
    Ok(())
}

/// What [`Presenter::new`] owns before the presenter exists: the waiters, the import
/// context, the passes and the frame's sync objects. Null handles until made.
#[derive(Default)]
struct Made {
    present_timer: Option<super::present_timing::PresentTimer>,
    #[cfg(windows)]
    vblank_timer: Option<super::vblank_timing::VblankTimer>,
    #[cfg(target_os = "linux")]
    hw: Option<HwCtx>,
    csc: CscPass,
    csc_planar: CscPass,
    overlay_pipe: OverlayPipe,
    scale: crate::scale::ScalePass,
    direct: crate::csc::DirectPass,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    acquire_sem: vk::Semaphore,
    done_sem: vk::Semaphore,
    fence: vk::Fence,
}

/// The bring-up's guard: a failed step in [`Presenter::new`] drops it, and its `Drop`
/// destroys what was made, in reverse order. `instance`, `surface_i`, `surface` and
/// `device` mirror the bring-up's own handles; `made` is owned here until
/// [`Bringup::into_parts`] hands it on and clears the mirrors, after which the drop
/// destroys nothing.
#[derive(Default)]
struct Bringup {
    instance: Option<ash::Instance>,
    surface_i: Option<ash::khr::surface::Instance>,
    surface: vk::SurfaceKHR,
    device: Option<ash::Device>,
    made: Made,
}

impl Bringup {
    /// Disarm: the presenter owns every object from here, so nothing is left to destroy.
    fn into_parts(mut self) -> Made {
        self.instance = None;
        self.surface = vk::SurfaceKHR::null();
        self.device = None;
        std::mem::take(&mut self.made)
    }
}

impl Drop for Bringup {
    fn drop(&mut self) {
        // The waiters hold a device clone: they join before the device goes.
        self.made.present_timer = None;
        #[cfg(windows)]
        {
            self.made.vblank_timer = None;
        }
        if let Some(device) = self.device.take() {
            let m = &mut self.made;
            // SAFETY: DESTROY — nothing was ever submitted on this device (the first submit is
            // the presenter's), so the GPU uses none of it; null handles are no-ops. Each is
            // destroyed once: after `into_parts` the mirrors are cleared and `made` is empty.
            unsafe {
                #[cfg(target_os = "linux")]
                if let Some(s) = m.hw.take().and_then(|h| h.sync) {
                    s.destroy(&device);
                }
                m.direct.destroy(&device);
                m.scale.destroy(&device);
                m.overlay_pipe.destroy(&device);
                m.csc_planar.destroy(&device);
                m.csc.destroy(&device);
                device.destroy_fence(m.fence, None);
                device.destroy_semaphore(m.done_sem, None);
                device.destroy_semaphore(m.acquire_sem, None);
                device.destroy_command_pool(m.cmd_pool, None); // frees `cmd_buf`
                device.destroy_device(None);
            }
        }
        if let Some(instance) = self.instance.take() {
            // SAFETY: DESTROY — the device made from this instance is gone above; the surface
            // is ours and no swapchain was built on it. A null surface is a no-op.
            unsafe {
                if let Some(si) = &self.surface_i {
                    si.destroy_surface(self.surface, None);
                }
                instance.destroy_instance(None);
            }
        }
    }
}

/// Every physical device's Vulkan Video decode capability
/// (`punktfunk-session --probe-decode`). No surface, no logical device, no session.
///
/// Ordered like [`list_adapters`] (discrete first). Row 0 is the default present
/// (and decode) device. `PUNKTFUNK_VK_DEVICE=<index>` moves both; probing the
/// iGPU while the dGPU presents reports the wrong GPU.
pub fn probe_decode() -> Result<Vec<AdapterDecode>> {
    // SAFETY: dlopens libvulkan; no instance exists yet.
    let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;
    let app_name = CString::new("punktfunk-session").unwrap();
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(super::INSTANCE_API_VERSION);
    // SAFETY: CREATE — `app_info` outlives the call; we own the instance.
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .context("vkCreateInstance")?;

    // SAFETY: read-only query on the live instance; result by value.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let mut out: Vec<(u8, AdapterDecode)> = Vec::with_capacity(devices.len());
    // `enumerate()` before filtering or sorting: this index is the one `knobs.vk_device` names.
    for (raw_index, pdev) in devices.into_iter().enumerate() {
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let props = unsafe { instance.get_physical_device_properties(pdev) };
        let name = props
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let rank = device_rank(&props);
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
        let decode = DecodeProbe::of(&instance, pdev, |n: &std::ffi::CStr| {
            ext_props
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(n))
        });
        let usable = decode.usable();
        // Format queries need `VK_KHR_video_queue` entry points; asking without them
        // is a null dispatch, not an answer.
        let formats = if usable {
            // SAFETY: `instance` is live and `pdev` was enumerated from it; the probe only reads.
            unsafe { pf_vkdecode::probe::probe_video_formats(&entry, &instance, pdev) }
        } else {
            Vec::new()
        };
        out.push((
            rank,
            AdapterDecode {
                index: raw_index,
                name,
                discrete: rank == 0,
                api_1_3: decode.api_1_3,
                features_ok: decode.features_ok,
                decode_family: decode.family.map(|(i, _)| i),
                codec_ops: decode.family.map_or(0, |(_, ops)| ops.as_raw()),
                base_missing: names(&decode.base_missing),
                codec_exts: names(&decode.codec_exts),
                usable,
                formats,
            },
        ));
    }
    out.sort_by_key(|(rank, _)| *rank);
    // SAFETY: DESTROY — no logical device was created against this instance.
    unsafe { instance.destroy_instance(None) };
    Ok(out.into_iter().map(|(_, a)| a).collect())
}

fn names(exts: &[&std::ffi::CStr]) -> Vec<String> {
    exts.iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect()
}

/// Physical-device marketing names for the shells' GPU picker
/// (`punktfunk-session --list-adapters`). No surface, no logical device. Discrete
/// first (same tie-break as `pick_device`); duplicate names collapsed because the
/// name is the whole `PUNKTFUNK_VK_ADAPTER` key. Same 1.3 instance the presenter creates.
pub fn list_adapters() -> Result<Vec<String>> {
    // SAFETY: dlopens libvulkan; no instance exists yet.
    let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;
    let app_name = CString::new("punktfunk-session").unwrap();
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(super::INSTANCE_API_VERSION);
    // SAFETY: CREATE — `app_info` outlives the call; we own the instance.
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .context("vkCreateInstance")?;
    // SAFETY: read-only query on the live instance; result by value.
    let mut ranked: Vec<(u8, String)> = unsafe { instance.enumerate_physical_devices() }?
        .into_iter()
        .map(|d| {
            // SAFETY: read-only query on the live instance; `d` was enumerated from it.
            let props = unsafe { instance.get_physical_device_properties(d) };
            let rank = device_rank(&props);
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            (rank, name)
        })
        .filter(|(_, n)| !n.is_empty())
        .collect();
    // SAFETY: DESTROY — no logical device was created against this instance.
    unsafe { instance.destroy_instance(None) };
    ranked.sort_by_key(|(r, _)| *r); // stable: enumeration order within each tier
    let mut names: Vec<String> = Vec::new();
    for (_, n) in ranked {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    Ok(names)
}

/// Whether the device the knobs would present on imports D3D11 textures (`win_capable`:
/// both win32 extensions and an importable BGRA8), asked before any window exists. The
/// device is `pick_device`'s, ranked the same way, minus the present-queue check that needs
/// a surface. `None` without a Vulkan loader or instance. Keeps nothing it creates.
#[cfg(windows)]
pub(crate) fn probe_import(knobs: &Knobs) -> Option<bool> {
    // SAFETY: dlopens libvulkan; no instance exists yet.
    let entry = unsafe { ash::Entry::load() }.ok()?;
    let app_info = vk::ApplicationInfo::default().api_version(super::INSTANCE_API_VERSION);
    // SAFETY: CREATE — `app_info` outlives the call; the instance is destroyed below.
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .ok()?;
    // SAFETY: read-only query on the live instance; result by value.
    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    let want = knobs
        .vk_adapter
        .as_deref()
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty());
    let mut candidates: Vec<(u8, u8, vk::PhysicalDevice)> = devices
        .iter()
        .enumerate()
        .filter(|(i, _)| knobs.vk_device.is_none_or(|forced| forced == *i))
        .map(|(_, &d)| {
            // SAFETY: read-only query on the live instance; `d` was enumerated from it.
            let props = unsafe { instance.get_physical_device_properties(d) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let name_rank = match &want {
                Some(w) if name == *w => 0,
                Some(w) if name.contains(w.as_str()) || w.contains(&name) => 1,
                Some(_) => 2,
                None => 0,
            };
            (name_rank, device_rank(&props), d)
        })
        .collect();
    candidates.sort_by_key(|&(name_rank, rank, _)| (name_rank, rank));
    let graphics = |d: vk::PhysicalDevice| {
        // SAFETY: read-only query on the live instance; `d` was enumerated from it.
        unsafe { instance.get_physical_device_queue_family_properties(d) }
            .iter()
            .any(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
    };
    let imports = candidates
        .into_iter()
        .map(|(_, _, d)| d)
        .find(|&d| graphics(d))
        .is_some_and(|d| {
            // SAFETY: read-only query on the live instance; `d` was enumerated from it.
            let exts =
                unsafe { instance.enumerate_device_extension_properties(d) }.unwrap_or_default();
            let has = |n: &CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
            crate::d3d11::DEVICE_EXTENSIONS.iter().all(|n| has(n))
                && crate::d3d11::import_supported(&instance, d).bgra8
        });
    // SAFETY: DESTROY — no logical device was created against this instance.
    unsafe { instance.destroy_instance(None) };
    Some(imports)
}

/// First physical device with a graphics+present queue family here. The `vk_device`
/// knob overrides (raw enumeration order).
fn pick_device(
    instance: &ash::Instance,
    surface_i: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    knobs: &Knobs,
) -> Result<(vk::PhysicalDevice, u32)> {
    // SAFETY: read-only query on the live instance; result by value.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let forced = knobs.vk_device;
    let mut candidates: Vec<vk::PhysicalDevice> = match forced {
        Some(i) => devices.get(i).copied().into_iter().collect(),
        None => devices,
    };
    // Rank when the index override is absent (stable sort):
    // 1. The `vk_adapter` marketing name: exact, then substring; unmatched keeps order.
    // 2. Discrete over integrated — enumeration puts the iGPU first on some hybrids.
    if forced.is_none() {
        let want = knobs
            .vk_adapter
            .as_deref()
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty());
        candidates.sort_by_key(|d| {
            // SAFETY: read-only query on the live instance; `d` was enumerated from it.
            let props = unsafe { instance.get_physical_device_properties(*d) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let name_rank = match &want {
                Some(w) if name == *w => 0,
                Some(w) if name.contains(w.as_str()) || w.contains(&name) => 1,
                Some(_) => 2,
                None => 0,
            };
            (name_rank, device_rank(&props))
        });
    }
    for pdev in candidates {
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        for (i, f) in families.iter().enumerate() {
            let graphics = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present =
                // SAFETY: read-only query; `pdev` and `surface` are live on this instance.
                unsafe { surface_i.get_physical_device_surface_support(pdev, i as u32, surface) }
                    .unwrap_or(false);
            if graphics && present {
                return Ok((pdev, i as u32));
            }
        }
    }
    bail!("no Vulkan device with a graphics+present queue family")
}

/// SDR: a 10-bit UNORM, then BGRA8, then RGBA8, then any sRGB-space UNORM, else the first
/// format. 10-bit first: the console's gradients band in 8. UNORM not SRGB — decoded RGBA is
/// already display-referred; an SRGB blit would re-encode it.
/// HDR: a 10-bit UNORM + HDR10/ST.2084 colorspace when the instance ext and surface
/// offer one; otherwise the shader tonemaps. The `hdr10` knob can refuse HDR10; `sdr_8bit`
/// keeps SDR to 8 bits.
pub(super) fn pick_formats(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    colorspace_ext: bool,
    knobs: &Knobs,
) -> Result<(vk::SurfaceFormatKHR, Option<vk::SurfaceFormatKHR>)> {
    // Compositors advertise HDR10 on SDR desktops; the knob refuses it.
    let colorspace_ext = colorspace_ext && knobs.hdr10;
    // SAFETY: read-only query; `pdev` and `surface` are live on this instance.
    let formats = unsafe { surface_i.get_physical_device_surface_formats(pdev, surface) }?;
    let mut sdr = None;
    let eight_bit = knobs.sdr_8bit;
    let ranked: &[vk::Format] = if eight_bit {
        &[vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM]
    } else {
        &[
            vk::Format::A2B10G10R10_UNORM_PACK32,
            vk::Format::A2R10G10B10_UNORM_PACK32,
            vk::Format::B8G8R8A8_UNORM,
            vk::Format::R8G8B8A8_UNORM,
        ]
    };
    for &want in ranked {
        if let Some(f) = formats
            .iter()
            .find(|f| f.format == want && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        {
            sdr = Some(*f);
            break;
        }
    }
    let srgb_encoded = |f: vk::Format| {
        matches!(
            f,
            vk::Format::B8G8R8A8_SRGB
                | vk::Format::R8G8B8A8_SRGB
                | vk::Format::A8B8G8R8_SRGB_PACK32
        )
    };
    let sdr = sdr
        .or_else(|| {
            formats
                .iter()
                .find(|f| {
                    f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR && !srgb_encoded(f.format)
                })
                .copied()
        })
        .or_else(|| formats.first().copied())
        .ok_or_else(|| anyhow!("surface offers no formats"))?;
    let hdr10 = colorspace_ext
        .then(|| {
            formats
                .iter()
                .find(|f| {
                    f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT
                        && matches!(
                            f.format,
                            vk::Format::A2B10G10R10_UNORM_PACK32
                                | vk::Format::A2R10G10B10_UNORM_PACK32
                        )
                })
                .copied()
        })
        .flatten();
    Ok((sdr, hdr10))
}

/// User presentation intent, resolved to a swapchain present mode by [`present_mode_chain`].
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentPref {
    /// Tear-free (`vsync` setting, default on).
    pub vsync: bool,
    /// Variable-refresh follows the stream cadence (`allow_vrr`, default on).
    pub allow_vrr: bool,
    /// Opt-in for the VRR FIFO-first ladder (`knobs.vrr_fifo`). Default off.
    pub vrr_fifo_opt_in: bool,
    /// Device enabled `VK_EXT_present_mode_fifo_latest_ready`; the mode may be requested.
    /// Set during device creation, never by callers.
    pub fifo_latest_ready: bool,
    /// The window is fullscreen as the stream starts. Re-picked at each console launch
    /// ([`Presenter::set_present_pref`]); F11 mid-stream does not re-pick.
    pub fullscreen: bool,
}

/// First offered mode from this ladder; FIFO always last (spec-guaranteed).
///
/// V-sync on admits no tearing rung: IMMEDIATE and FIFO_RELAXED stay out, so a
/// surface offering neither MAILBOX nor LATEST_READY lands on FIFO.
/// V-sync + VRR + fullscreen with LATEST_READY or `knobs.vrr_fifo`:
/// vblank-locked family first — MAILBOX would re-quantize to the compositor.
/// LATEST_READY is newest-wins in the driver; plain FIFO waits a full refresh,
/// so without LATEST_READY that rung is opt-in.
/// Otherwise MAILBOX first so an arrival-paced presenter does not block.
/// V-sync off: IMMEDIATE, FIFO_RELAXED, MAILBOX.
fn present_mode_chain(pref: PresentPref) -> Vec<vk::PresentModeKHR> {
    use vk::PresentModeKHR as M;
    let flr = pref.fifo_latest_ready.then_some(fifo_latest_ready::MODE);
    let mut chain: Vec<M> = if !pref.vsync {
        vec![M::IMMEDIATE, M::FIFO_RELAXED, M::MAILBOX]
    } else if pref.allow_vrr && pref.fullscreen && (pref.fifo_latest_ready || pref.vrr_fifo_opt_in)
    {
        flr.into_iter().chain([M::FIFO, M::MAILBOX]).collect()
    } else {
        vec![M::MAILBOX].into_iter().chain(flr).collect()
    };
    if !pref.vsync {
        chain.extend(flr);
    }
    // FIFO last: the spec guarantees it, so the chain always lands.
    if !chain.contains(&M::FIFO) {
        chain.push(M::FIFO);
    }
    chain
}

/// The device feature says the mode may be asked for; only the surface's list says it is
/// served. A ladder led by a mode this surface lacks would land on plain FIFO.
fn serve_offered(pref: PresentPref, modes: &[vk::PresentModeKHR]) -> PresentPref {
    PresentPref {
        fifo_latest_ready: pref.fifo_latest_ready && modes.contains(&fifo_latest_ready::MODE),
        ..pref
    }
}

/// Resolve the present mode. `pinned` (the `present_mode` knob) picks one; otherwise the
/// first entry of [`present_mode_chain`] the surface offers.
pub(super) fn pick_present_mode(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    pref: PresentPref,
    pinned: Option<&str>,
) -> Result<vk::PresentModeKHR> {
    // SAFETY: read-only query; `pdev` and `surface` are live on this instance.
    let modes = unsafe { surface_i.get_physical_device_surface_present_modes(pdev, surface) }?;
    let pinned = match pinned {
        Some("fifo") => Some(vk::PresentModeKHR::FIFO),
        Some("immediate") => Some(vk::PresentModeKHR::IMMEDIATE),
        Some("fifo_relaxed") => Some(vk::PresentModeKHR::FIFO_RELAXED),
        Some("mailbox") => Some(vk::PresentModeKHR::MAILBOX),
        None => None,
        Some(other) => {
            tracing::warn!(
                value = other,
                "unknown PUNKTFUNK_PRESENT_MODE (expected fifo|mailbox|immediate|fifo_relaxed) — following the settings"
            );
            None
        }
    };
    if let Some(want) = pinned {
        if modes.contains(&want) {
            return Ok(want);
        }
        tracing::warn!(
            ?want,
            "PUNKTFUNK_PRESENT_MODE not offered by this surface — falling back"
        );
    }
    // Present modes are a (surface, device, fullscreen) property; log the set so a
    // fallback is visible as the driver's list, not our choice.
    tracing::info!(
        available = ?modes,
        "surface present modes"
    );
    let chain = present_mode_chain(serve_offered(pref, &modes));
    let chosen = chain
        .iter()
        .copied()
        .find(|m| modes.contains(m))
        .unwrap_or(vk::PresentModeKHR::FIFO); // always available per spec
                                              // A request the surface cannot serve is a driver fact; do not present it as our choice.
    if chosen != chain[0] {
        tracing::info!(
            requested = ?chain[0],
            active = ?chosen,
            vsync = pref.vsync,
            allow_vrr = pref.allow_vrr,
            "the surface does not offer the preferred present mode"
        );
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk::PresentModeKHR as M;

    /// An adapter whose five gate inputs are the low bits of `mask` (set = the input holds).
    fn adapter(mask: u32) -> AdapterDecode {
        let has = |bit: u32| mask & (1 << bit) != 0;
        AdapterDecode {
            index: 0,
            name: "test".into(),
            discrete: true,
            api_1_3: has(0),
            features_ok: has(1),
            decode_family: has(2).then_some(0),
            codec_ops: 0,
            base_missing: if has(3) {
                Vec::new()
            } else {
                vec!["VK_KHR_video_queue".into()]
            },
            codec_exts: if has(4) {
                vec!["VK_KHR_video_decode_h264".into()]
            } else {
                Vec::new()
            },
            usable: video_decode_gate(has(0), has(1), has(2), has(3), has(4)),
            formats: Vec::new(),
        }
    }

    /// `--probe-decode` prints `why_not` for a refused device: it names one reason per
    /// failed input and nothing for a device the gate admits. A conjunct added to the gate
    /// without a reason changes the gate's arity and stops this compiling.
    #[test]
    fn why_not_is_empty_exactly_when_the_gate_admits_the_device() {
        for mask in 0..32u32 {
            let a = adapter(mask);
            let failed = (!mask & 0b11111).count_ones() as usize;
            assert_eq!(a.why_not().len(), failed, "mask {mask:05b}");
            assert_eq!(a.usable, a.why_not().is_empty(), "mask {mask:05b}");
        }
    }

    /// The report names every bit the driver set, VP9 too, and a bit it cannot name by value.
    #[test]
    fn codec_names_account_for_the_whole_mask() {
        let ops = |codec_ops| AdapterDecode {
            codec_ops,
            ..adapter(0b11111)
        };
        assert!(ops(0).codec_names().is_empty());
        assert_eq!(ops(0x3).codec_names(), ["H.264", "H.265"]);
        assert_eq!(
            ops(0x4 | 0x8 | 0x10).codec_names(),
            ["AV1", "VP9 (no punktfunk rung)", "unrecognised bits 0x10"]
        );
    }

    /// Offered and wanted, present-wait2 replaces the original pair: a swapchain times its
    /// presents with one generation.
    #[test]
    fn present_wait2_supersedes_present_wait() {
        assert_eq!(present_wait_generation(true, true, true), (false, true));
        // Offered but not wanted, or wanted but not offered: the original pair.
        assert_eq!(present_wait_generation(true, true, false), (true, false));
        assert_eq!(present_wait_generation(true, false, true), (true, false));
        // The successors alone, where a driver withholds the original pair.
        assert_eq!(present_wait_generation(false, true, true), (false, true));
        assert_eq!(present_wait_generation(false, false, true), (false, false));
    }

    /// A device that enables LATEST_READY for a surface that does not list it must not
    /// lead with the FIFO family: that lands on plain FIFO.
    #[test]
    fn the_vrr_ladder_needs_the_surface_to_list_the_mode() {
        let pref = || PresentPref {
            vsync: true,
            allow_vrr: true,
            fullscreen: true,
            vrr_fifo_opt_in: false,
            fifo_latest_ready: true,
        };
        let listed = [fifo_latest_ready::MODE, M::MAILBOX, M::FIFO];
        assert_eq!(
            present_mode_chain(serve_offered(pref(), &listed))[0],
            fifo_latest_ready::MODE
        );
        let unlisted = [M::MAILBOX, M::FIFO];
        assert_eq!(
            present_mode_chain(serve_offered(pref(), &unlisted))[0],
            M::MAILBOX
        );
    }

    /// Preference ladders. Every chain ends at FIFO, which the spec guarantees —
    /// otherwise a surface that refuses every earlier entry has no landing.
    #[test]
    fn present_mode_chains_rank_by_intent() {
        let pref = |vsync, allow_vrr, fullscreen| PresentPref {
            vsync,
            allow_vrr,
            fullscreen,
            vrr_fifo_opt_in: true, // the ladder under test; default is off (see below)
            fifo_latest_ready: false,
        };
        let flr = fifo_latest_ready::MODE;

        // V-sync off: IMMEDIATE first, even when VRR+fullscreen.
        assert_eq!(present_mode_chain(pref(false, true, true))[0], M::IMMEDIATE);
        assert_eq!(
            present_mode_chain(pref(false, false, false))[0],
            M::IMMEDIATE
        );
        assert_eq!(
            present_mode_chain(pref(false, true, true))[1],
            M::FIFO_RELAXED,
            "tears only on a late frame — the gentler tearing rung"
        );

        // Tear-free + VRR + fullscreen prefers vblank-locked, but only when opted in.
        assert_eq!(present_mode_chain(pref(true, true, true))[0], M::FIFO);
        // Without the opt-in, MAILBOX-first stands: plain FIFO waits a full refresh.
        assert_eq!(
            present_mode_chain(PresentPref {
                vsync: true,
                allow_vrr: true,
                fullscreen: true,
                vrr_fifo_opt_in: false,
                fifo_latest_ready: false,
            })[0],
            M::MAILBOX,
            "without a queue-free vblank mode the VRR ladder would lead with plain FIFO, \
             which measured ~27 ms worse — so it stays opt-in there"
        );
        assert_eq!(
            present_mode_chain(PresentPref {
                vsync: true,
                allow_vrr: true,
                fullscreen: true,
                vrr_fifo_opt_in: false,
                fifo_latest_ready: true,
            })[0],
            fifo_latest_ready::MODE,
            "with LATEST_READY available, following the panel costs 0.6 ms over MAILBOX \
             instead of 27 — cheap enough to be automatic"
        );

        // LATEST_READY only where the device enabled it, and it outranks plain FIFO:
        // FIFO's vblank pacing without the standing queue.
        let with_flr = |vsync, allow_vrr, fullscreen| PresentPref {
            vsync,
            allow_vrr,
            fullscreen,
            vrr_fifo_opt_in: true,
            fifo_latest_ready: true,
        };
        for p in [
            pref(true, false, false),
            pref(true, true, true),
            pref(false, true, true),
        ] {
            assert!(
                !present_mode_chain(p).contains(&flr),
                "never requested unless the device enabled the extension"
            );
        }
        let default_flr = present_mode_chain(with_flr(true, false, false));
        assert_eq!(
            default_flr[0],
            M::MAILBOX,
            "MAILBOX still leads by measurement"
        );
        assert_eq!(default_flr[1], flr, "then the driver-native newest-wins");
        assert!(
            default_flr.iter().position(|m| *m == flr)
                < default_flr.iter().position(|m| *m == M::FIFO),
            "LATEST_READY must outrank plain FIFO — it is FIFO minus the standing queue"
        );
        assert_eq!(
            present_mode_chain(with_flr(true, true, true))[0],
            flr,
            "the VRR ladder takes the queue-free vblank mode first"
        );

        for p in [
            pref(true, true, true),
            pref(true, true, false),
            pref(true, false, true),
            pref(false, true, true),
            pref(false, false, false),
            with_flr(true, false, false),
        ] {
            assert!(
                present_mode_chain(p).contains(&M::FIFO),
                "FIFO is the guaranteed landing"
            );
        }
    }

    /// V-sync on may hold no tearing rung. The field surface offers only
    /// `[IMMEDIATE, FIFO]`, and a ladder that lists IMMEDIATE ahead of its FIFO
    /// landing tears on every such client.
    #[test]
    fn vsync_never_lands_on_a_tearing_mode() {
        let offered = [M::IMMEDIATE, M::FIFO];
        for (allow_vrr, fullscreen, vrr_fifo_opt_in, fifo_latest_ready) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, true, true, false),
            (true, true, false, true),
            (true, false, true, true),
            (false, true, true, true),
        ] {
            let chain = present_mode_chain(PresentPref {
                vsync: true,
                allow_vrr,
                fullscreen,
                vrr_fifo_opt_in,
                fifo_latest_ready,
            });
            assert!(
                !chain.contains(&M::IMMEDIATE) && !chain.contains(&M::FIFO_RELAXED),
                "tear-free intent, tearing rung in {chain:?}"
            );
            assert_eq!(
                chain.iter().find(|m| offered.contains(m)),
                Some(&M::FIFO),
                "{chain:?} on an IMMEDIATE/FIFO surface"
            );
        }
    }
}
