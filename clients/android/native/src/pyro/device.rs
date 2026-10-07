//! Vulkan bring-up for the PyroWave lane: loader → instance → physical device → logical
//! device, exported as the [`VulkanDecodeDevice`] the shared decoder shares.
//!
//! This is the whole reason PyroWave can exist here at all. Every other codec on this
//! client reaches glass through MediaCodec, which has no wavelet decoder and never will;
//! PyroWave is Vulkan compute, so the lane brings up its own device and presents from it
//! (`super::present`). Nothing here touches the MediaCodec path.
//!
//! The feature set is not ours to choose — it mirrors `pf-presenter`'s probe exactly
//! (`crates/pf-presenter/src/vk/setup.rs`), because pyrowave 0.4.0 reconstructs the
//! create-infos from what we report and reads those features to pick its kernels. A
//! device that misses one is not a slower device, it is a device the codec refuses.

use anyhow::{anyhow, bail, Result};
use ash::vk;
use pf_client_core::video_vk::{QueueLock, VulkanDecodeDevice};
use std::ffi::CString;

/// Instance extensions, in creation order. `VulkanDecodeDevice::instance_extensions` must
/// list exactly these — pyrowave replays them and a mismatch is a wrong answer, not an error.
const INSTANCE_EXTS: [&std::ffi::CStr; 2] =
    [ash::khr::surface::NAME, ash::khr::android_surface::NAME];

/// Device extensions, same contract. Only the swapchain: decode and CSC are core 1.3.
const DEVICE_EXTS: [&std::ffi::CStr; 1] = [ash::khr::swapchain::NAME];

/// The instance and the window surface on it. Dropping destroys the surface, then the instance.
pub(super) struct WindowSurface {
    pub(super) entry: ash::Entry,
    pub(super) instance: ash::Instance,
    /// Holds a reference on the `ANativeWindow` until destroyed.
    pub(super) surface: vk::SurfaceKHR,
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
        // SAFETY: only `PyroDevice` holds this past bring-up, and it destroys its device (and
        // with it every swapchain on the surface) before its fields drop.
        unsafe {
            ash::khr::surface::Instance::new(&self.entry, &self.instance)
                .destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

/// The device this lane decodes and presents on, plus the ash wrappers its callers need.
/// Dropping destroys the device, then `base`.
pub(super) struct PyroDevice {
    pub(super) device: ash::Device,
    pub(super) pdev: vk::PhysicalDevice,
    pub(super) queue: vk::Queue,
    pub(super) qf: u32,
    /// The handoff the shared PyroWave decoder is built from.
    pub(super) vkd: VulkanDecodeDevice,
    /// The surface every swapchain is built on, outliving the device as Vulkan requires.
    pub(super) base: WindowSurface,
}

/// The compute feature set pyrowave's kernels need, read off a physical device.
///
/// Vulkan 1.3 is a hard floor (the kernels are 1.3 core compute), and each feature below
/// is used unconditionally by some path in the codec. `float16` is the one exception —
/// preferred, not required — so it is reported separately.
struct FeatureProbe {
    ok: bool,
    float16: bool,
    api_version: u32,
    vendor_id: u32,
    name: String,
}

/// Read the pyrowave feature set off `pdev`. Pure queries; no device is created.
fn probe_features(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> FeatureProbe {
    // SAFETY: read-only query; the returned struct is plain data.
    let props = unsafe { instance.get_physical_device_properties(pdev) };
    let name = props
        .device_name_as_c_str()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut f12)
        .push_next(&mut f13);
    // SAFETY: read-only query; the pNext chain locals outlive the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut f2) };
    // Copy before the chained structs are read: `f2` mutably borrows them.
    let shader_int16 = f2.features.shader_int16;
    let ok = props.api_version >= vk::API_VERSION_1_3
        && shader_int16 == vk::TRUE
        && f12.storage_buffer8_bit_access == vk::TRUE
        && f12.timeline_semaphore == vk::TRUE
        && f13.subgroup_size_control == vk::TRUE
        && f13.compute_full_subgroups == vk::TRUE
        && f13.synchronization2 == vk::TRUE;
    FeatureProbe {
        ok,
        float16: f12.shader_float16 == vk::TRUE,
        api_version: props.api_version,
        vendor_id: props.vendor_id,
        name,
    }
}

/// Create the instance both the probe and the real bring-up use.
///
/// `with_surface` is false for the capability probe: it enumerates devices and asks about
/// features only, and asking for `VK_KHR_android_surface` there would fail bring-up on a
/// driver that has the codec's compute but not the window system entry point.
fn create_instance(entry: &ash::Entry, with_surface: bool) -> Result<ash::Instance> {
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let ptrs: Vec<*const std::ffi::c_char> = if with_surface {
        INSTANCE_EXTS.iter().map(|n| n.as_ptr()).collect()
    } else {
        Vec::new()
    };
    let ci = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&ptrs);
    // SAFETY: `ci` and the name pointers it borrows outlive the call; the loader owns the
    // returned instance until `destroy_instance`.
    unsafe { entry.create_instance(&ci, None) }.map_err(|e| anyhow!("vkCreateInstance: {e}"))
}

/// Does any GPU on this device decode PyroWave?
///
/// Drives the `CODEC_PYROWAVE` advertisement, so it must be honest in both directions: a
/// false yes costs the session its video (the host encodes a codec nothing here can
/// decode), a false no silently downgrades a capable box to HEVC. Creates and destroys a
/// bare instance — no surface, no logical device, no GPU work — so it is safe to call
/// before a session exists.
pub(crate) fn pyrowave_capable() -> bool {
    // SAFETY: dlopens libvulkan.so and reads its entry points. `Entry` owns the handle.
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            log::info!("pyro: no Vulkan loader on this device ({e}) — PyroWave unavailable");
            return false;
        }
    };
    let instance = match create_instance(&entry, false) {
        Ok(i) => i,
        Err(e) => {
            log::info!("pyro: Vulkan 1.3 instance refused ({e}) — PyroWave unavailable");
            return false;
        }
    };
    // SAFETY: `instance` is live; the enumeration only reads.
    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    let capable = devices.iter().any(|&pdev| {
        let p = probe_features(&instance, pdev);
        log::info!(
            "pyro: {} (Vulkan {}.{}) pyrowave-capable: {}",
            p.name,
            vk::api_version_major(p.api_version),
            vk::api_version_minor(p.api_version),
            p.ok
        );
        p.ok
    });
    // SAFETY: no child objects were created from this instance.
    unsafe { instance.destroy_instance(None) };
    capable
}

impl PyroDevice {
    /// Bring up the device the session decodes and presents on.
    ///
    /// Picks the first physical device that both passes the feature probe and can present
    /// to `base`'s surface — on a phone or TV box there is exactly one GPU, so the loop is a
    /// formality that costs nothing and keeps the emulator (a software device beside a
    /// hardware one) honest. An error drops `base`.
    pub(super) fn new(base: WindowSurface) -> Result<PyroDevice> {
        let (instance, surface) = (&base.instance, base.surface);
        let surface_i = ash::khr::surface::Instance::new(&base.entry, instance);
        // SAFETY: `instance` is live; the enumeration only reads.
        let devices = unsafe { instance.enumerate_physical_devices() }?;
        let mut chosen = None;
        for pdev in devices {
            let probe = probe_features(instance, pdev);
            if !probe.ok {
                log::info!(
                    "pyro: skipping {} — missing the PyroWave compute feature set",
                    probe.name
                );
                continue;
            }
            // SAFETY: read-only query on a live physical device.
            let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let qf = families.iter().enumerate().position(|(i, f)| {
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    // SAFETY: `surface` is live while `base` is.
                    && unsafe {
                        surface_i.get_physical_device_surface_support(pdev, i as u32, surface)
                    }
                    .unwrap_or(false)
            });
            if let Some(qf) = qf {
                chosen = Some((pdev, qf as u32, probe));
                break;
            }
        }
        let (pdev, qf, probe) = chosen
            .ok_or_else(|| anyhow!("no Vulkan 1.3 device here decodes PyroWave and presents"))?;

        let prio = [1.0f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&prio)];
        let ext_ptrs: Vec<*const std::ffi::c_char> =
            DEVICE_EXTS.iter().map(|n| n.as_ptr()).collect();
        // Exactly the probe's conjuncts, enabled. `shader_float16` rides along when the
        // driver has it: pyrowave picks a half-precision kernel path off it, and reporting
        // it enabled when it is not would make the codec choose a path the device refuses.
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .storage_buffer8_bit_access(true)
            .shader_float16(probe.float16);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .synchronization2(true)
            .subgroup_size_control(true)
            .compute_full_subgroups(true);
        let mut f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f12)
            .push_next(&mut f13);
        f2.features.shader_int16 = vk::TRUE;
        let dev_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_ptrs)
            .push_next(&mut f2);
        // SAFETY: every builder above is a local that outlives the call; the returned
        // device is owned by this struct and destroyed in its `Drop`.
        let device = unsafe { instance.create_device(pdev, &dev_ci, None) }
            .map_err(|e| anyhow!("vkCreateDevice: {e}"))?;
        // SAFETY: queue 0 of `qf` was requested at create.
        let queue = unsafe { device.get_device_queue(qf, 0) };

        log::info!(
            "pyro: {} on Vulkan {}.{} — PyroWave decode + present (float16: {})",
            probe.name,
            vk::api_version_major(probe.api_version),
            vk::api_version_minor(probe.api_version),
            probe.float16
        );

        let vkd = VulkanDecodeDevice {
            get_instance_proc_addr: base.entry.static_fn().get_instance_proc_addr as usize,
            instance: ash::vk::Handle::as_raw(instance.handle()) as usize,
            physical_device: ash::vk::Handle::as_raw(pdev) as usize,
            device: ash::vk::Handle::as_raw(device.handle()) as usize,
            vendor_id: probe.vendor_id,
            device_name: probe.name,
            graphics_qf: qf,
            // No Vulkan Video here: this lane is wavelet compute only, and MediaCodec owns
            // every other codec on this client. The decode family mirrors graphics so the
            // struct stays well-formed for the fields pyrowave does read.
            decode_qf: qf,
            decode_video_caps: 0,
            instance_extensions: INSTANCE_EXTS.iter().map(|n| CString::from(*n)).collect(),
            device_extensions: DEVICE_EXTS.iter().map(|n| CString::from(*n)).collect(),
            f_sampler_ycbcr: false,
            f_timeline_semaphore: true,
            f_synchronization2: true,
            video_decode: false,
            pyrowave_decode: true,
            f_shader_int16: true,
            f_storage_buffer8: true,
            f_subgroup_size_control: true,
            f_compute_full_subgroups: true,
            f_shader_float16: probe.float16,
            api_version: probe.api_version,
            queue_families: vec![qf],
            d3d11_import: false,
            dmabuf_import: false,
            vaapi_av1_decode: false,
            vaapi_hevc_decode: false,
            d3d11_nv12: false,
            d3d11_p010: false,
            d3d11_hdr10: false,
            adapter_luid: None,
            queue_lock: std::sync::Arc::new(QueueLock::new()),
        };
        Ok(PyroDevice {
            device,
            pdev,
            queue,
            qf,
            vkd,
            base,
        })
    }

    /// Load the loader, create the instance, and make a surface on `window`.
    ///
    /// Separate from [`PyroDevice::new`] because the surface must exist before a physical
    /// device can be chosen (presentation support is a per-family property of the surface).
    pub(super) fn open_surface(window: &ndk::native_window::NativeWindow) -> Result<WindowSurface> {
        // SAFETY: dlopens libvulkan.so; `Entry` owns the handle for the session.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| anyhow!("no Vulkan loader: {e}"))?;
        let instance = create_instance(&entry, true)?;
        let android = ash::khr::android_surface::Instance::new(&entry, &instance);
        let ci = vk::AndroidSurfaceCreateInfoKHR::default().window(window.ptr().as_ptr().cast());
        // SAFETY: `window` is live for the call; the surface takes its own reference on it.
        let surface = match unsafe { android.create_android_surface(&ci, None) } {
            Ok(s) => s,
            Err(e) => {
                // SAFETY: no child objects exist yet.
                unsafe { instance.destroy_instance(None) };
                bail!("vkCreateAndroidSurfaceKHR: {e}");
            }
        };
        Ok(WindowSurface {
            entry,
            instance,
            surface,
        })
    }
}

impl Drop for PyroDevice {
    fn drop(&mut self) {
        let _q = self.vkd.queue_lock.guard();
        // SAFETY: `Present` borrows this device, and `run_inner` declares the decoder after it,
        // so both are destroyed by now; idling retires any work still on the queue.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
        }
    }
}
