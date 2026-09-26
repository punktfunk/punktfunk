//! Presenter bring-up: instance → surface → device → swapchain over an SDL window.
//!
//! [`Presenter::new`] is the only construction path. Device creation and
//! [`probe_decode`] share [`VIDEO_BASE`], [`VIDEO_CODECS`], and [`video_decode_gate`]
//! so a probe cannot report a capability the session then refuses.
//!
//! Pinning: `PUNKTFUNK_VK_DEVICE=<index>` is raw `vkEnumeratePhysicalDevices` order;
//! `PUNKTFUNK_VK_ADAPTER=<name>` matches the marketing name, then discrete-first;
//! `PUNKTFUNK_PRESENT_MODE=` fifo|mailbox|immediate|fifo_relaxed; `PUNKTFUNK_VRR_FIFO=1`
//! selects the FIFO-first VRR ladder when LATEST_READY is absent; `PUNKTFUNK_HDR10=0`
//! refuses the HDR10 swapchain.
//!
//! Evidence: [`present_mode_chain`], [`pick_device`], [`AdapterDecode::index`].

#[cfg(target_os = "linux")]
use super::HwCtx;
#[cfg(windows)]
use super::HwCtxWin;
use super::{OverlayPipe, Presenter};
use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf;
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use std::ffi::{c_char, CString};

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
/// Device creation and [`probe_decode`] call this same function.
pub(crate) fn video_decode_gate(
    api_1_3: bool,
    features_ok: bool,
    has_decode_family: bool,
    base_exts_present: bool,
    any_codec_ext: bool,
) -> bool {
    api_1_3 && features_ok && has_decode_family && base_exts_present && any_codec_ext
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

impl Presenter {
    /// Instance → surface → device → swapchain over an SDL window.
    /// `instance_extensions` is `VideoSubsystem::vulkan_instance_extensions()`.
    pub fn new(
        window: &sdl3::video::Window,
        instance_extensions: &[String],
        pref: PresentPref,
    ) -> Result<Presenter> {
        // SAFETY: dlopens libvulkan; no instance exists yet.
        let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;

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
        let surface_i = ash::khr::surface::Instance::new(&entry, &instance);

        // SAFETY: CREATE — `instance` is live; SDL returns a surface we own and later destroy.
        let surface = unsafe { window.vulkan_create_surface(instance.handle()) }
            .map_err(|e| anyhow!("SDL_Vulkan_CreateSurface: {e}"))?;

        let (pdev, qfi) = pick_device(&instance, &surface_i, surface)?;
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        {
            // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            tracing::info!(device = %name, queue_family = qfi, "vulkan device");
        }

        // Optional: all four import extensions, else `supports_dmabuf()` is false.
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let available = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
        let has = |name: &std::ffi::CStr| {
            available
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };
        #[cfg(target_os = "linux")]
        let hw_capable = dmabuf::DEVICE_EXTENSIONS.iter().all(|n| has(n));
        // Optional on top: the decode fence as a semaphore instead of a CPU poll.
        #[cfg(target_os = "linux")]
        let sync_fd_ext = hw_capable && has(ash::khr::external_semaphore_fd::NAME);
        let mut dev_exts = vec![ash::khr::swapchain::NAME.as_ptr()];
        #[cfg(target_os = "linux")]
        if hw_capable {
            dev_exts.extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()));
            if sync_fd_ext {
                dev_exts.push(ash::khr::external_semaphore_fd::NAME.as_ptr());
            }
        } else {
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
        let import = crate::d3d11::import_supported(&instance, pdev);
        #[cfg(windows)]
        let win_capable = crate::d3d11::DEVICE_EXTENSIONS.iter().all(|n| has(n)) && import.bgra8;
        #[cfg(windows)]
        if win_capable {
            dev_exts.extend(crate::d3d11::DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()));
        } else {
            tracing::info!(
                "device lacks the win32 external-memory/keyed-mutex extensions — D3D11VA \
                 hardware frames unavailable"
            );
        }
        // Adapter LUID so D3D11VA creates its decode device on the same GPU. Core 1.1.
        let mut id_props = vk::PhysicalDeviceIDProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id_props);
        // SAFETY: read-only query; `id_props` / `props2` outlive the call.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
        let adapter_luid: Option<[u8; 8]> =
            (id_props.device_luid_valid == vk::TRUE).then_some(id_props.device_luid);
        // `vkSetHdrMetadataEXT` is what compositors key "this app is HDR" on.
        // The HDR10 colorspace alone still looks SDR to the shell.
        let has_hdr_metadata = has(ash::ext::hdr_metadata::NAME);
        if has_hdr_metadata {
            dev_exts.push(ash::ext::hdr_metadata::NAME.as_ptr());
        }

        // Optional: video extensions, decode queue, and decoder features, or
        // the exported `video_decode` fact stays `false`.
        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let dev_props = unsafe { instance.get_physical_device_properties(pdev) };
        let dev_is_13 = vk::api_version_major(dev_props.api_version) > 1
            || vk::api_version_minor(dev_props.api_version) >= 3;
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
        // The surface may list FIFO_LATEST_READY with the extension disabled; the
        // device feature is the gate on requesting it.
        let flr_ok = if has(fifo_latest_ready::NAME) {
            let mut feat = fifo_latest_ready::Features::default();
            let mut probe = vk::PhysicalDeviceFeatures2 {
                p_next: (&mut feat) as *mut _ as *mut std::ffi::c_void,
                ..Default::default()
            };
            // SAFETY: read-only query; `feat` is the pNext target and outlives the call.
            unsafe { instance.get_physical_device_features2(pdev, &mut probe) };
            feat.present_mode_fifo_latest_ready == vk::TRUE
        } else {
            false
        };
        let present_wait_ok = present_wait_exts
            && have_pid.present_id == vk::TRUE
            && have_pwait.present_wait == vk::TRUE;
        let features_ok = have_f11.sampler_ycbcr_conversion == vk::TRUE
            && have_f12.timeline_semaphore == vk::TRUE
            && have_f13.synchronization2 == vk::TRUE;
        // PyroWave is Vulkan 1.3 compute on this device — no video extensions.
        // Probe here so a capable device enables the features and advertises the codec.
        let pyrowave_ok = dev_is_13
            && have_shader_int16 == vk::TRUE
            && have_f12.storage_buffer8_bit_access == vk::TRUE
            && have_f12.timeline_semaphore == vk::TRUE
            && have_f13.subgroup_size_control == vk::TRUE
            && have_f13.compute_full_subgroups == vk::TRUE
            && have_f13.synchronization2 == vk::TRUE;

        let decode_family: Option<(u32, vk::VideoCodecOperationFlagsKHR)> = {
            // SAFETY: read-only query; length only.
            let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pdev) };
            let mut video: Vec<vk::QueueFamilyVideoPropertiesKHR> =
                vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
            let mut props: Vec<vk::QueueFamilyProperties2> = video
                .iter_mut()
                .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
                .collect();
            // SAFETY: read-only query; `props` / `video` outlive the call and receive the fill.
            unsafe { instance.get_physical_device_queue_family_properties2(pdev, &mut props) };
            // `props` mutably borrows `video` via push_next; copy the flags, drop `props`,
            // then read the driver-filled video properties.
            let flags: Vec<vk::QueueFlags> = props
                .iter()
                .map(|p| p.queue_family_properties.queue_flags)
                .collect();
            drop(props);
            flags
                .iter()
                .zip(&video)
                .enumerate()
                .find(|(_, (f, _))| f.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
                .map(|(i, (_, v))| (i as u32, v.video_codec_operations))
        };

        let codec_exts: Vec<&std::ffi::CStr> =
            VIDEO_CODECS.into_iter().filter(|n| has(n)).collect();
        let video_ok = video_decode_gate(
            dev_is_13,
            features_ok,
            decode_family.is_some(),
            VIDEO_BASE.iter().all(|n| has(n)),
            !codec_exts.is_empty(),
        );

        let (decode_qf, decode_caps) = decode_family.unwrap_or((qfi, Default::default()));
        let mut video_ext_names: Vec<&std::ffi::CStr> = Vec::new();
        if video_ok {
            video_ext_names.extend(VIDEO_BASE);
            video_ext_names.extend(&codec_exts);
            // Optional; pf-vkdecode probes these rather than requiring them.
            for opt in [c"VK_KHR_video_maintenance1", c"VK_KHR_video_maintenance2"] {
                if has(opt) {
                    video_ext_names.push(opt);
                }
            }
            dev_exts.extend(video_ext_names.iter().map(|n| n.as_ptr()));
            tracing::info!(
                decode_qf,
                caps = ?decode_caps,
                exts = ?video_ext_names,
                "Vulkan Video decode available on this device"
            );
        } else {
            // Log every conjunct. Empty `codec_exts` next to non-empty `queue_codec_ops`
            // means the extensions are missing, not the hardware.
            let base_missing: Vec<&str> = VIDEO_BASE
                .iter()
                .filter(|n| !has(n))
                .map(|n| n.to_str().unwrap_or("?"))
                .collect();
            let codec_ext_names: Vec<&str> = codec_exts
                .iter()
                .map(|n| n.to_str().unwrap_or("?"))
                .collect();
            tracing::info!(
                dev_is_13,
                features_ok,
                decode_family = decode_family.is_some(),
                video_base_missing = ?base_missing,
                codec_exts_present = ?codec_ext_names,
                queue_codec_ops = ?decode_family.map(|(_, ops)| ops),
                device = %dev_props
                    .device_name_as_c_str()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                vendor_id = format_args!("0x{:04X}", dev_props.vendor_id),
                "Vulkan Video decode unavailable on this device — the decoder falls back \
                 one rung (D3D11VA on Windows, VAAPI on Linux, then software)"
            );
        }

        // Present-wait on: PresentTimer stamps on-glass; otherwise the stamp is submit-time.
        if present_wait_ok {
            dev_exts.push(ash::khr::present_id::NAME.as_ptr());
            dev_exts.push(ash::khr::present_wait::NAME.as_ptr());
        }
        if flr_ok {
            dev_exts.push(fifo_latest_ready::NAME.as_ptr());
        }
        let mut en_flr = fifo_latest_ready::Features {
            present_mode_fifo_latest_ready: vk::TRUE,
            ..Default::default()
        };
        let mut en_pid = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let mut en_pwait = vk::PhysicalDevicePresentWaitFeaturesKHR::default().present_wait(true);

        let mut en_f11 = vk::PhysicalDeviceVulkan11Features::default()
            .sampler_ycbcr_conversion(have_f11.sampler_ycbcr_conversion == vk::TRUE);
        let mut en_f12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(have_f12.timeline_semaphore == vk::TRUE)
            .storage_buffer8_bit_access(pyrowave_ok)
            .shader_float16(pyrowave_ok && have_f12.shader_float16 == vk::TRUE);
        let mut en_f13 = vk::PhysicalDeviceVulkan13Features::default()
            .synchronization2(have_f13.synchronization2 == vk::TRUE)
            .subgroup_size_control(pyrowave_ok)
            .compute_full_subgroups(pyrowave_ok);
        let mut en_f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut en_f11)
            .push_next(&mut en_f12)
            .push_next(&mut en_f13);
        if present_wait_ok {
            en_f2 = en_f2.push_next(&mut en_pid).push_next(&mut en_pwait);
        }
        if flr_ok {
            // Hand-rolled struct: splice onto the pNext list head by hand.
            en_flr.p_next = en_f2.p_next;
            en_f2.p_next = (&mut en_flr) as *mut _ as *mut std::ffi::c_void;
        }
        en_f2.features.shader_int16 = if pyrowave_ok { vk::TRUE } else { vk::FALSE };

        let priorities = [1.0f32];
        let mut queue_info = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&priorities)];
        if video_ok && decode_qf != qfi {
            queue_info.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(decode_qf)
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
        let swap_d = ash::khr::swapchain::Device::new(&instance, &device);
        let present_timer = present_wait_ok.then(|| {
            super::present_timing::PresentTimer::spawn(ash::khr::present_wait::Device::new(
                &instance, &device,
            ))
        });
        tracing::info!(
            present_wait = present_wait_ok,
            "on-glass present timing (VK_KHR_present_wait)"
        );
        let hdr_metadata_d =
            has_hdr_metadata.then(|| ash::ext::hdr_metadata::Device::new(&instance, &device));
        // SAFETY: `device` is live; queue 0 of `qfi` was requested at create.
        let queue = unsafe { device.get_device_queue(qfi, 0) };
        #[cfg(target_os = "linux")]
        let hw = if hw_capable {
            let sync = sync_fd_ext
                .then(|| crate::dmabuf::SyncImport::new(&instance, pdev, &device))
                .flatten();
            tracing::info!(
                semaphore = sync.is_some(),
                "dmabuf decode sync (a decode fence the sampling submit waits; \
                 `false` polls the fence on the presenter thread)"
            );
            Some(HwCtx {
                ext_mem_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                modifier_cache: Default::default(),
                imports: Default::default(),
                sync,
            })
        } else {
            None
        };
        #[cfg(windows)]
        let hw_win = win_capable.then(|| HwCtxWin {
            ext_mem_win32: ash::khr::external_memory_win32::Device::new(&instance, &device),
            imports: crate::d3d11::ImportCache::default(),
        });
        let csc = CscPass::new(&device, super::VIDEO_FORMAT)?;
        // Writes the same intermediate as `csc`. Always built: the software decode rung
        // renders through it. Gating on the pyrowave probe would hide software frames.
        let csc_planar = CscPass::new_planar(&device, super::VIDEO_FORMAT)?;

        // Export the selected device facts when any consumer needs the handles —
        // on Linux always: the presenter has a selected device and every consumer
        // gates individual lanes on the booleans. Extension lists must match
        // creation: the pyrowave decoder replays them into its pinned create-info.
        // One `queue_lock` per device (decode + Skia + presenter; see its docs).
        let queue_lock = std::sync::Arc::new(pf_client_core::video::QueueLock::new());
        #[cfg(windows)]
        let export_worthy = video_ok || win_capable || pyrowave_ok;
        #[cfg(target_os = "linux")]
        let export_worthy = true;
        let video_export = if export_worthy {
            let mut device_extensions: Vec<CString> =
                vec![CString::from(ash::khr::swapchain::NAME)];
            #[cfg(target_os = "linux")]
            if hw_capable {
                device_extensions
                    .extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| CString::from(*n)));
            }
            #[cfg(windows)]
            if win_capable {
                device_extensions.extend(
                    crate::d3d11::DEVICE_EXTENSIONS
                        .iter()
                        .map(|n| CString::from(*n)),
                );
            }
            if has_hdr_metadata {
                device_extensions.push(CString::from(ash::ext::hdr_metadata::NAME));
            }
            device_extensions.extend(video_ext_names.iter().map(|n| CString::from(*n)));
            let decode_ops =
                pf_client_core::video::usable_decode_ops(dev_props.vendor_id, decode_caps.as_raw());
            Some(pf_client_core::video::VulkanDecodeDevice {
                get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
                instance: instance.handle().as_raw() as usize,
                physical_device: pdev.as_raw() as usize,
                device: device.handle().as_raw() as usize,
                vendor_id: dev_props.vendor_id,
                device_name: dev_props
                    .device_name_as_c_str()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                graphics_qf: qfi,
                decode_qf,
                decode_video_caps: decode_ops,
                instance_extensions: instance_extensions
                    .iter()
                    .map(|e| CString::new(e.as_str()).unwrap())
                    .collect(),
                device_extensions,
                f_sampler_ycbcr: have_f11.sampler_ycbcr_conversion == vk::TRUE,
                f_timeline_semaphore: have_f12.timeline_semaphore == vk::TRUE,
                f_synchronization2: have_f13.synchronization2 == vk::TRUE,
                f_shader_int16: pyrowave_ok,
                f_storage_buffer8: pyrowave_ok,
                f_subgroup_size_control: pyrowave_ok,
                f_compute_full_subgroups: pyrowave_ok,
                f_shader_float16: pyrowave_ok && have_f12.shader_float16 == vk::TRUE,
                api_version: dev_props.api_version,
                queue_families: queue_info.iter().map(|q| q.queue_family_index).collect(),
                pyrowave_decode: pyrowave_ok,
                video_decode: video_ok,
                // On-glass latch stamps exist only while PresentTimer (present-wait) runs.
                present_timing: present_timer.is_some(),
                #[cfg(windows)]
                d3d11_import: win_capable,
                #[cfg(not(windows))]
                d3d11_import: false,
                #[cfg(target_os = "linux")]
                dmabuf_import: hw_capable,
                #[cfg(not(target_os = "linux"))]
                dmabuf_import: false,
                #[cfg(target_os = "linux")]
                vaapi_av1_decode: hw_capable
                    && pf_client_core::video::vaapi_av1_decodable(
                        dev_props.vendor_id,
                        video_ok
                            && decode_ops & vk::VideoCodecOperationFlagsKHR::DECODE_AV1.as_raw()
                                != 0,
                    ),
                #[cfg(not(target_os = "linux"))]
                vaapi_av1_decode: false,
                // HDR10 surface facts arrive with `pick_formats` below.
                d3d11_hdr10: false,
                d3d11_nv12: false,
                d3d11_p010: false,
                adapter_luid,
                queue_lock: queue_lock.clone(),
            })
        } else {
            None
        };
        #[cfg(windows)]
        let mut video_export = video_export;

        let (format, hdr10_format) = pick_formats(&surface_i, pdev, surface, has_colorspace_ext)?;
        // D3D11VA may emit its RGB10 PQ ring only when this device imports 10-bit and the
        // surface offers an HDR10 swapchain. Planar NV12/P010 slots need the planar import
        // and a vendor that survives it (`planar_allowed`).
        #[cfg(windows)]
        if let Some(v) = video_export.as_mut() {
            v.d3d11_hdr10 = win_capable && import.rgb10 && hdr10_format.is_some();
            let planar = win_capable && crate::d3d11::planar_allowed(v.vendor_id);
            v.d3d11_nv12 = planar && import.nv12;
            v.d3d11_p010 = planar && import.p010;
        }
        let mut pref = pref;
        pref.vrr_fifo_opt_in = vrr_fifo_opt_in();
        pref.fifo_latest_ready = flr_ok;
        let present_mode = pick_present_mode(&surface_i, pdev, surface, pref)?;
        tracing::info!(
            ?format,
            ?hdr10_format,
            ?present_mode,
            vsync = pref.vsync,
            allow_vrr = pref.allow_vrr,
            fifo_latest_ready = flr_ok,
            hdr_metadata = has_hdr_metadata,
            "swapchain config"
        );
        let overlay_pipe = OverlayPipe::new(&device, format.format, false)?;
        let scale = crate::scale::ScalePass::new(&device, format.format)?;

        // SAFETY: CREATE — CreateInfo is a local; the pool is owned by the Presenter being built.
        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(qfi),
                None,
            )
        }?;
        // SAFETY: ALLOCATE — the pool is ours and empty; we take the single primary buffer.
        let cmd_buf = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let acquire_sem =
            // SAFETY: CREATE — CreateInfo is a local; the handle is stored on the Presenter.
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        // SAFETY: CREATE — CreateInfo is a local; SIGNALED so the first wait is a no-op.
        let fence = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }?;

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
            hw,
            #[cfg(windows)]
            hw_win,
            csc,
            csc_planar,
            cpu_planes: None,
            video_export,
            overlay_pipe,
            scale,
            retired_hw: None,
            #[cfg(windows)]
            retained_slot: None,
            last_import_us: 0,
            last_submit_us: 0,
            last_fence_us: 0,
            last_acquire_us: 0,
            last_present_us: 0,
            queue_lock,
            format,
            hdr10_format,
            hdr_active: false,
            hdr_downgrade_warned: false,
            hdr_metadata_d,
            hdr_meta: None,
            present_mode,
            swapchain: vk::SwapchainKHR::null(),
            images: Vec::new(),
            extent: vk::Extent2D::default(),
            render_sems: Vec::new(),
            acquire_sem,
            fence,
            cmd_pool,
            cmd_buf,
            staging: None,
            video: None,
            submitted: false,
            acquired: None,
            present_timer,
            next_present_id: 0,
            last_presented: None,
            video_fit: Default::default(),
            placement_logged: None,
        };
        p.recreate_swapchain(window)?;
        Ok(p)
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
    // `enumerate()` before filtering or sorting: this index is `PUNKTFUNK_VK_DEVICE`.
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
        let rank = match props.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => 0u8,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
            _ => 2,
        };
        let api_1_3 = vk::api_version_major(props.api_version) > 1
            || vk::api_version_minor(props.api_version) >= 3;

        // Same three features the creation path demands (`features_ok` there).
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

        // SAFETY: read-only query on the live instance; `pdev` was enumerated from it.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
        let has = |n: &std::ffi::CStr| {
            ext_props
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(n))
        };
        let base_missing: Vec<String> = VIDEO_BASE
            .iter()
            .filter(|n| !has(n))
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let codec_exts: Vec<String> = VIDEO_CODECS
            .iter()
            .filter(|n| has(n))
            .map(|n| n.to_string_lossy().into_owned())
            .collect();

        // Report the family's codec ops even without the extensions: hardware-can vs
        // driver-does-not-expose is a different answer from "this GPU cannot".
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
        let flags: Vec<vk::QueueFlags> = qprops
            .iter()
            .map(|p| p.queue_family_properties.queue_flags)
            .collect();
        drop(qprops);
        let found = flags
            .iter()
            .zip(&video)
            .enumerate()
            .find(|(_, (f, _))| f.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
            .map(|(i, (_, v))| (i as u32, v.video_codec_operations));

        let usable = video_decode_gate(
            api_1_3,
            features_ok,
            found.is_some(),
            base_missing.is_empty(),
            !codec_exts.is_empty(),
        );
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
                api_1_3,
                features_ok,
                decode_family: found.map(|(i, _)| i),
                codec_ops: found.map_or(0, |(_, ops)| ops.as_raw()),
                base_missing,
                codec_exts,
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
            let rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 0u8,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 2,
            };
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

/// First physical device with a graphics+present queue family here.
/// `PUNKTFUNK_VK_DEVICE=<index>` overrides (raw enumeration order).
fn pick_device(
    instance: &ash::Instance,
    surface_i: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    // SAFETY: read-only query on the live instance; result by value.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let forced: Option<usize> = std::env::var("PUNKTFUNK_VK_DEVICE")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut candidates: Vec<vk::PhysicalDevice> = match forced {
        Some(i) => devices.get(i).copied().into_iter().collect(),
        None => devices,
    };
    // Rank when the index override is absent (stable sort):
    // 1. `PUNKTFUNK_VK_ADAPTER` marketing name: exact, then substring; unmatched keeps order.
    // 2. Discrete over integrated — enumeration puts the iGPU first on some hybrids.
    if forced.is_none() {
        let want = std::env::var("PUNKTFUNK_VK_ADAPTER")
            .ok()
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
            let type_rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 2,
            };
            (name_rank, type_rank)
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
/// offer one; otherwise the shader tonemaps.
pub(super) fn pick_formats(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    colorspace_ext: bool,
) -> Result<(vk::SurfaceFormatKHR, Option<vk::SurfaceFormatKHR>)> {
    // `PUNKTFUNK_HDR10=0` refuses the HDR10 swapchain; PQ stays shader-tonemapped.
    // Compositors advertise HDR10 on SDR desktops.
    let colorspace_ext = colorspace_ext
        && !std::env::var("PUNKTFUNK_HDR10")
            .is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"));
    // SAFETY: read-only query; `pdev` and `surface` are live on this instance.
    let formats = unsafe { surface_i.get_physical_device_surface_formats(pdev, surface) }?;
    let mut sdr = None;
    for want in [
        vk::Format::A2B10G10R10_UNORM_PACK32,
        vk::Format::A2R10G10B10_UNORM_PACK32,
        vk::Format::B8G8R8A8_UNORM,
        vk::Format::R8G8B8A8_UNORM,
    ] {
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
    /// Opt-in for the VRR FIFO-first ladder (`PUNKTFUNK_VRR_FIFO=1`). Default off.
    pub vrr_fifo_opt_in: bool,
    /// Device enabled `VK_EXT_present_mode_fifo_latest_ready`; the mode may be requested.
    /// Set during device creation, never by callers.
    pub fifo_latest_ready: bool,
    /// Session started fullscreen. Mode is chosen once at swapchain creation; F11
    /// mid-session does not re-pick.
    pub fullscreen: bool,
}

/// First offered mode from this ladder; FIFO always last (spec-guaranteed).
///
/// V-sync on admits no tearing rung: IMMEDIATE and FIFO_RELAXED stay out, so a
/// surface offering neither MAILBOX nor LATEST_READY lands on FIFO.
/// V-sync + VRR + fullscreen with LATEST_READY or `PUNKTFUNK_VRR_FIFO=1`:
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

/// `PUNKTFUNK_VRR_FIFO=1` opts into the FIFO-first ladder for variable-refresh panels.
fn vrr_fifo_opt_in() -> bool {
    std::env::var("PUNKTFUNK_VRR_FIFO").is_ok_and(|v| v != "0")
}

/// Resolve the present mode. `PUNKTFUNK_PRESENT_MODE` pins one; otherwise the first
/// entry of [`present_mode_chain`] the surface offers.
fn pick_present_mode(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    pref: PresentPref,
) -> Result<vk::PresentModeKHR> {
    // SAFETY: read-only query; `pdev` and `surface` are live on this instance.
    let modes = unsafe { surface_i.get_physical_device_surface_present_modes(pdev, surface) }?;
    let pinned = match std::env::var("PUNKTFUNK_PRESENT_MODE").ok().as_deref() {
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
    let chain = present_mode_chain(pref);
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
