//! Vulkan bring-up for the PyroWave lane: loader → instance → physical device → logical
//! device, exported as the [`VulkanDecodeDevice`] the shared decoder shares.
//!
//! This is the whole reason PyroWave can exist here at all. Every other codec on this
//! client reaches glass through MediaCodec, which has no wavelet decoder and never will;
//! PyroWave is Vulkan compute, so the lane brings up its own device and presents from it
//! (`super::present`). Nothing here touches the MediaCodec path.
//!
//! The feature set is not ours to choose: pyrowave reconstructs the create-infos from what
//! we report and reads those features to pick its kernels. A device that misses one is
//! not a slower device, it is a device the codec refuses. The floor is Vulkan 1.1 with
//! each later feature's extension ([`features`]).

use anyhow::{anyhow, bail, Result};
use ash::vk;
use pf_client_video::video_pyrowave::features;
use pf_client_video::video_vk::{QueueLock, VulkanDecodeDevice};
use std::ffi::{CStr, CString};

/// Instance extensions, in creation order. `VulkanDecodeDevice::instance_extensions` must
/// list exactly these — pyrowave replays them and a mismatch is a wrong answer, not an error.
const INSTANCE_EXTS: [&std::ffi::CStr; 2] =
    [ash::khr::surface::NAME, ash::khr::android_surface::NAME];

/// Device extensions, same contract: the swapchain, [`BUFFER_EXTS`] where all three are
/// offered, and the codec's own below the version that made its features core.
const DEVICE_EXTS: [&std::ffi::CStr; 1] = [ash::khr::swapchain::NAME];

/// What presenting through `AHardwareBuffer`s takes: import, hand-over to the compositor, and
/// fences as sync fds.
const BUFFER_EXTS: [&std::ffi::CStr; 3] = [
    ash::android::external_memory_android_hardware_buffer::NAME,
    ash::ext::queue_family_foreign::NAME,
    ash::khr::external_semaphore_fd::NAME,
];

/// The instance and the window surface on it. Dropping destroys the surface, then the instance.
pub(super) struct WindowSurface {
    pub(super) entry: ash::Entry,
    pub(super) instance: ash::Instance,
    /// Holds a reference on the `ANativeWindow` until destroyed.
    pub(super) surface: vk::SurfaceKHR,
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
        // SAFETY: only `PyroDevice` holds this past bring-up. Every swapchain on the surface
        // is a `Present`'s, which borrows the device and destroys its swapchain when it drops;
        // the device is destroyed after that and before this struct's fields drop.
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
    /// [`BUFFER_EXTS`] are enabled: the lane may present `AHardwareBuffer`s.
    pub(super) buffers: bool,
}

/// The compute feature set pyrowave's kernels need, read off a physical device.
struct FeatureProbe {
    ok: bool,
    /// What the device has of the set. Only `int16` and the subgroup facts are required.
    facts: features::Facts,
    /// The properties pyrowave checks when it opens a decoder.
    limits: features::Limits,
    /// A required extension the device does not offer.
    missing: Option<&'static CStr>,
    /// The codec's extensions to enable on this device.
    exts: Vec<&'static CStr>,
    /// The version the device may use: its own, capped at the instance's.
    api: u32,
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
    let api = props.api_version.min(INSTANCE_API);
    // SAFETY: read-only query on a live physical device.
    let offered =
        unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
    let has = |want: &CStr| {
        offered
            .iter()
            .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == want))
    };
    let exts = features::extensions(api, has);
    let mut chain = features::Chain::new(api, has);
    // SAFETY: read-only query; the chain holds only structs this version and these
    // extensions define, and outlives the call.
    unsafe { instance.get_physical_device_features2(pdev, chain.head()) };
    let facts = chain.facts();
    // SAFETY: `pdev` is a physical device of the live `instance`.
    let limits = unsafe { features::Limits::read(instance, pdev, api, has) };
    FeatureProbe {
        ok: props.api_version >= features::MIN_API
            && exts.is_ok()
            && facts.decodes()
            && limits.decode(facts.storage8),
        facts,
        limits,
        missing: exts.as_ref().err().copied(),
        exts: exts.unwrap_or_default(),
        api,
        api_version: props.api_version,
        vendor_id: props.vendor_id,
        name,
    }
}

/// The version the instance asks for. A 1.1 loader accepts it and the device then runs at
/// its own version.
const INSTANCE_API: u32 = vk::API_VERSION_1_3;

/// Create the instance both the probe and the real bring-up use.
///
/// `with_surface` is false for the capability probe: it enumerates devices and asks about
/// features only, and asking for `VK_KHR_android_surface` there would fail bring-up on a
/// driver that has the codec's compute but not the window system entry point.
fn create_instance(entry: &ash::Entry, with_surface: bool) -> Result<ash::Instance> {
    let app = vk::ApplicationInfo::default().api_version(INSTANCE_API);
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
            log::info!("pyro: Vulkan instance refused ({e}) — PyroWave unavailable");
            return false;
        }
    };
    // SAFETY: `instance` is live; the enumeration only reads.
    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    let capable = devices.iter().any(|&pdev| {
        let p = probe_features(&instance, pdev);
        // Everything the verdict rests on, so a "false" in a user's log names its reason.
        log::info!(
            "pyro: {} (Vulkan {}.{}) pyrowave-capable: {}; {:?}; {:?}; missing extension: {:?}",
            p.name,
            vk::api_version_major(p.api_version),
            vk::api_version_minor(p.api_version),
            p.ok,
            p.facts,
            p.limits,
            p.missing
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
        let (pdev, qf, probe) =
            chosen.ok_or_else(|| anyhow!("no Vulkan device here decodes PyroWave and presents"))?;

        let prio = [1.0f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&prio)];
        // SAFETY: read-only query on a live physical device.
        let offered =
            unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
        let buffers = BUFFER_EXTS.iter().all(|want| {
            offered
                .iter()
                .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == *want))
        });
        let exts: Vec<&std::ffi::CStr> = DEVICE_EXTS
            .iter()
            .chain(buffers.then_some(BUFFER_EXTS.iter()).into_iter().flatten())
            .chain(probe.exts.iter())
            .copied()
            .collect();
        let ext_ptrs: Vec<*const std::ffi::c_char> = exts.iter().map(|n| n.as_ptr()).collect();
        // Exactly what the probe found, enabled. Reporting a feature on when it is not
        // would make the codec choose a path the device refuses.
        let facts = features::Facts {
            sampler_ycbcr: false,
            ..probe.facts
        };
        let mut chain = features::Chain::new(probe.api, |name| exts.contains(&name));
        chain.enable(facts);
        let mut dev_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_ci)
            .enabled_extension_names(&ext_ptrs);
        dev_ci.p_next = chain.as_ptr();
        // SAFETY: every builder above is a local that outlives the call; the returned
        // device is owned by this struct and destroyed in its `Drop`.
        let device = unsafe { instance.create_device(pdev, &dev_ci, None) }
            .map_err(|e| anyhow!("vkCreateDevice: {e}"))?;
        // SAFETY: queue 0 of `qf` was requested at create.
        let queue = unsafe { device.get_device_queue(qf, 0) };

        log::info!(
            "pyro: {} on Vulkan {}.{} — PyroWave decode + present (float16: {}, sync2: {}, \
             timeline: {}, storage8: {})",
            probe.name,
            vk::api_version_major(probe.api_version),
            vk::api_version_minor(probe.api_version),
            facts.float16,
            facts.sync2,
            facts.timeline,
            facts.storage8
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
            device_extensions: exts.iter().map(|n| CString::from(*n)).collect(),
            f_sampler_ycbcr: false,
            f_timeline_semaphore: facts.timeline,
            f_synchronization2: facts.sync2,
            video_decode: false,
            pyrowave_decode: true,
            f_shader_int16: true,
            f_storage_buffer8: facts.storage8,
            f_storage_buffer16: facts.storage16,
            f_subgroup_size_control: true,
            f_compute_full_subgroups: true,
            f_shader_float16: facts.float16,
            api_version: probe.api_version,
            queue_families: vec![qf],
            d3d11_import: false,
            dmabuf_import: false,
            vaapi_av1_decode: false,
            vaapi_hevc_decode: false,
            d3d11_nv12: false,
            d3d11_p010: false,
            d3d11_present: false,
            d3d11_fence: false,
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
            buffers,
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
