//! Wayland scanout probe (see Cargo.toml). Run inside the target session:
//!
//! ```sh
//! WAYLAND_DISPLAY=wayland-0 wl-scanout-probe [--format nv12|p010] \
//!   [--layout toplevel|subsurface|both] [--seconds 4] [--size WxH] [--dump-only]
//! ```
//!
//! Prints the compositor's protocols, its linux-dmabuf feedback (per tranche: device, scanout
//! flag, the modifiers of NV12, P010, XR24, AR30 and XB30), then per layout: frames presented,
//! how many the compositor called zero-copy, and commit-to-glass p50/p95 from `wp_presentation`.
//! `--format xr24` is an RGB control. `--modifier linear|<hex>` pins the modifier (default
//! `auto` tries each); `--planes joint|split` puts both planes in one dma-buf or one per plane
//! (default `both` tries joint first).

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("wl-scanout-probe is Linux-only (Wayland compositors)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{bail, Context, Result};
    use ash::vk;
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use std::collections::BTreeMap;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::FileExt;
    use std::time::{Duration, Instant};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
        wl_subsurface, wl_surface,
    };
    use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
    use wayland_protocols::wp::color_representation::v1::client::{
        wp_color_representation_manager_v1 as crm, wp_color_representation_surface_v1 as crs,
    };
    use wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
        zwp_linux_dmabuf_v1 as dmabuf,
    };
    use wayland_protocols::wp::presentation_time::client::{
        wp_presentation, wp_presentation_feedback as pfb,
    };
    use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    const DRM_FORMAT_NV12: u32 = 0x3231_564e;
    const DRM_FORMAT_P010: u32 = 0x3031_3050;
    const DRM_FORMAT_XR24: u32 = 0x3432_5258;
    const DRM_FORMAT_AR30: u32 = 0x3033_5241;
    const DRM_FORMAT_XB30: u32 = 0x3033_4258;
    /// linux-dmabuf tranche flag: buffers of this tranche can go straight to a plane.
    const TRANCHE_SCANOUT: u32 = 1;
    /// wp_presentation_feedback kinds.
    const KIND_VSYNC: u32 = 1;
    const KIND_ZERO_COPY: u32 = 8;

    fn fourcc_name(f: u32) -> String {
        let b = f.to_le_bytes();
        b.iter().map(|&c| c as char).collect()
    }

    fn dev_major_minor(dev: u64) -> (u64, u64) {
        let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
        let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
        (major, minor)
    }

    #[derive(Default, Clone)]
    struct Tranche {
        device: u64,
        flags: u32,
        formats: Vec<(u32, u64)>,
    }

    #[derive(Default)]
    struct Feedback {
        main_device: u64,
        table: Vec<(u32, u64)>,
        tranches: Vec<Tranche>,
        cur: Tranche,
        done: bool,
    }

    impl Feedback {
        fn modifiers(&self, fourcc: u32) -> Vec<(u64, bool)> {
            let mut out: Vec<(u64, bool)> = Vec::new();
            for t in &self.tranches {
                let scanout = t.flags & TRANCHE_SCANOUT != 0;
                for &(f, m) in &t.formats {
                    if f != fourcc {
                        continue;
                    }
                    match out.iter_mut().find(|(mm, _)| *mm == m) {
                        Some(e) => e.1 |= scanout,
                        None => out.push((m, scanout)),
                    }
                }
            }
            out
        }
    }

    #[derive(Clone, Copy)]
    enum FbKind {
        Default,
        Surface,
    }

    struct Sample {
        commit_ns: u64,
        presented_ns: Option<u64>,
        flags: u32,
        refresh_ns: u32,
        discarded: bool,
    }

    #[derive(Default)]
    struct App {
        compositor: Option<wl_compositor::WlCompositor>,
        subcompositor: Option<wl_subcompositor::WlSubcompositor>,
        shm: Option<wl_shm::WlShm>,
        wm: Option<xdg_wm_base::XdgWmBase>,
        dmabuf: Option<dmabuf::ZwpLinuxDmabufV1>,
        dmabuf_version: u32,
        viewporter: Option<wp_viewporter::WpViewporter>,
        presentation: Option<wp_presentation::WpPresentation>,
        color_repr: Option<crm::WpColorRepresentationManagerV1>,
        globals: BTreeMap<String, u32>,
        size: (i32, i32),
        configured: bool,
        closed: bool,
        clock_id: Option<u32>,
        default_fb: Feedback,
        surface_fb: Feedback,
        samples: Vec<Sample>,
        releases: u32,
        params_failed: bool,
        created: Option<wl_buffer::WlBuffer>,
        repr_pairs: Vec<String>,
    }

    // ---- Wayland dispatch -------------------------------------------------------------------

    impl Dispatch<wl_registry::WlRegistry, ()> for App {
        fn event(
            app: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            else {
                return;
            };
            app.globals.insert(interface.clone(), version);
            match interface.as_str() {
                "wl_compositor" => {
                    app.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_subcompositor" => {
                    app.subcompositor = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(registry.bind(name, version.min(2), qh, ())),
                "zwp_linux_dmabuf_v1" => {
                    app.dmabuf_version = version;
                    if version >= 4 {
                        app.dmabuf = Some(registry.bind(name, version.min(5), qh, ()));
                    }
                }
                "wp_viewporter" => app.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wp_presentation" => {
                    app.presentation = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wp_color_representation_manager_v1" => {
                    app.color_repr = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
        fn event(
            _: &mut Self,
            wm: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                wm.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for App {
        fn event(
            app: &mut Self,
            xs: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xs.ack_configure(serial);
                app.configured = true;
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
        fn event(
            app: &mut Self,
            _: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                xdg_toplevel::Event::Configure { width, height, .. } => {
                    if width > 0 && height > 0 {
                        app.size = (width, height);
                    }
                }
                xdg_toplevel::Event::Close => app.closed = true,
                _ => {}
            }
        }
    }

    impl Dispatch<wp_presentation::WpPresentation, ()> for App {
        fn event(
            app: &mut Self,
            _: &wp_presentation::WpPresentation,
            event: wp_presentation::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wp_presentation::Event::ClockId { clk_id } = event {
                app.clock_id = Some(clk_id);
            }
        }
    }

    impl Dispatch<pfb::WpPresentationFeedback, usize> for App {
        fn event(
            app: &mut Self,
            _: &pfb::WpPresentationFeedback,
            event: pfb::Event,
            index: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(s) = app.samples.get_mut(*index) else {
                return;
            };
            match event {
                pfb::Event::Presented {
                    tv_sec_hi,
                    tv_sec_lo,
                    tv_nsec,
                    refresh,
                    flags,
                    ..
                } => {
                    let sec = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                    s.presented_ns = Some(sec * 1_000_000_000 + u64::from(tv_nsec));
                    s.refresh_ns = refresh;
                    s.flags = match flags {
                        WEnum::Value(k) => k.bits(),
                        WEnum::Unknown(v) => v,
                    };
                }
                pfb::Event::Discarded => s.discarded = true,
                _ => {}
            }
        }
    }

    impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, FbKind> for App {
        fn event(
            app: &mut Self,
            _: &feedback::ZwpLinuxDmabufFeedbackV1,
            event: feedback::Event,
            kind: &FbKind,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let fb = match kind {
                FbKind::Default => &mut app.default_fb,
                FbKind::Surface => &mut app.surface_fb,
            };
            match event {
                feedback::Event::FormatTable { fd, size } => {
                    let file = std::fs::File::from(fd);
                    let mut bytes = vec![0u8; size as usize];
                    if file.read_exact_at(&mut bytes, 0).is_ok() {
                        fb.table = bytes
                            .chunks_exact(16)
                            .map(|c| {
                                let f = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                                let m = u64::from_ne_bytes([
                                    c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15],
                                ]);
                                (f, m)
                            })
                            .collect();
                    }
                }
                feedback::Event::MainDevice { device } => fb.main_device = dev_from(&device),
                feedback::Event::TrancheTargetDevice { device } => {
                    fb.cur.device = dev_from(&device);
                }
                feedback::Event::TrancheFlags { flags } => {
                    fb.cur.flags = match flags {
                        WEnum::Value(f) => f.bits(),
                        WEnum::Unknown(v) => v,
                    };
                }
                feedback::Event::TrancheFormats { indices } => {
                    fb.cur.formats = indices
                        .chunks_exact(2)
                        .filter_map(|c| fb.table.get(u16::from_ne_bytes([c[0], c[1]]) as usize))
                        .copied()
                        .collect();
                }
                feedback::Event::TrancheDone => {
                    let t = std::mem::take(&mut fb.cur);
                    fb.tranches.push(t);
                }
                feedback::Event::Done => fb.done = true,
                _ => {}
            }
        }
    }

    fn dev_from(bytes: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        for (d, s) in b.iter_mut().zip(bytes.iter()) {
            *d = *s;
        }
        u64::from_ne_bytes(b)
    }

    impl Dispatch<wl_buffer::WlBuffer, ()> for App {
        fn event(
            app: &mut Self,
            _: &wl_buffer::WlBuffer,
            event: wl_buffer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_buffer::Event::Release = event {
                app.releases += 1;
            }
        }
    }

    impl Dispatch<params::ZwpLinuxBufferParamsV1, ()> for App {
        fn event(
            app: &mut Self,
            _: &params::ZwpLinuxBufferParamsV1,
            event: params::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                params::Event::Created { buffer } => app.created = Some(buffer),
                params::Event::Failed => app.params_failed = true,
                _ => {}
            }
        }

        wayland_client::event_created_child!(App, params::ZwpLinuxBufferParamsV1, [
            params::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
        ]);
    }

    impl Dispatch<crm::WpColorRepresentationManagerV1, ()> for App {
        fn event(
            app: &mut Self,
            _: &crm::WpColorRepresentationManagerV1,
            event: crm::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let crm::Event::SupportedCoefficientsAndRanges {
                coefficients,
                range,
            } = event
            {
                app.repr_pairs.push(format!("{coefficients:?}/{range:?}"));
            }
        }
    }

    delegate_noop!(App: ignore wl_compositor::WlCompositor);
    delegate_noop!(App: ignore wl_subcompositor::WlSubcompositor);
    delegate_noop!(App: ignore wl_shm::WlShm);
    delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(App: ignore wl_surface::WlSurface);
    delegate_noop!(App: ignore wl_subsurface::WlSubsurface);
    delegate_noop!(App: ignore wl_region::WlRegion);
    delegate_noop!(App: ignore dmabuf::ZwpLinuxDmabufV1);
    delegate_noop!(App: ignore wp_viewporter::WpViewporter);
    delegate_noop!(App: ignore wp_viewport::WpViewport);
    delegate_noop!(App: ignore crs::WpColorRepresentationSurfaceV1);

    /// Flush, then wait up to `timeout` for events and dispatch them.
    fn pump(
        conn: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
        timeout: Duration,
    ) -> Result<()> {
        queue.dispatch_pending(app)?;
        conn.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let ts = Timespec {
                tv_sec: timeout.as_secs() as _,
                tv_nsec: timeout.subsec_nanos() as _,
            };
            if poll(&mut fds, Some(&ts))? > 0 {
                guard.read()?;
            }
        }
        queue.dispatch_pending(app)?;
        Ok(())
    }

    fn now_ns(clock_id: u32) -> Option<u64> {
        use rustix::time::{clock_gettime, ClockId};
        let id = match clock_id {
            0 => ClockId::Realtime,
            1 => ClockId::Monotonic,
            7 => ClockId::Boottime,
            _ => return None,
        };
        let t = clock_gettime(id);
        Some(t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64)
    }

    // ---- Vulkan: an exported test picture -----------------------------------------------------

    struct Gpu {
        _entry: ash::Entry,
        instance: ash::Instance,
        pdev: vk::PhysicalDevice,
        device: ash::Device,
        queue: vk::Queue,
        pool: vk::CommandPool,
        mem_props: vk::PhysicalDeviceMemoryProperties,
        name: String,
    }

    /// One exported picture: a joint two-plane image behind one dma-buf, or one single-plane
    /// image per plane behind two.
    struct Picture {
        images: Vec<vk::Image>,
        memories: Vec<vk::DeviceMemory>,
        fds: Vec<OwnedFd>,
        modifier: u64,
        /// (fd index, offset, pitch) per plane.
        planes: Vec<(usize, u64, u64)>,
        size: u64,
        width: u32,
        height: u32,
    }

    impl Gpu {
        fn open(main_device: u64) -> Result<Gpu> {
            // SAFETY: loading the Vulkan loader; the entry outlives every handle in `Gpu`.
            let entry = unsafe { ash::Entry::load() }.context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let ci = vk::InstanceCreateInfo::default().application_info(&app);
            // SAFETY: `ci` roots locals that outlive the call.
            let instance =
                unsafe { entry.create_instance(&ci, None) }.context("vkCreateInstance")?;
            // SAFETY: live instance.
            let pdevs = unsafe { instance.enumerate_physical_devices() }?;
            let (want_major, want_minor) = dev_major_minor(main_device);
            let mut chosen = None;
            for &pd in &pdevs {
                // SAFETY: live physical device.
                let exts = unsafe { instance.enumerate_device_extension_properties(pd) }?;
                let has =
                    |n: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
                let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                let mut p2 = vk::PhysicalDeviceProperties2::default();
                if has(ash::ext::physical_device_drm::NAME) {
                    p2 = p2.push_next(&mut drm);
                }
                // SAFETY: the chain roots locals that outlive the call.
                unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
                let name = p2
                    .properties
                    .device_name_as_c_str()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let matches = (drm.has_render == vk::TRUE
                    && drm.render_major as u64 == want_major
                    && drm.render_minor as u64 == want_minor)
                    || (drm.has_primary == vk::TRUE
                        && drm.primary_major as u64 == want_major
                        && drm.primary_minor as u64 == want_minor);
                let needed = has(ash::khr::external_memory_fd::NAME)
                    && has(ash::ext::external_memory_dma_buf::NAME)
                    && has(ash::ext::image_drm_format_modifier::NAME);
                if needed && (matches || chosen.is_none()) {
                    chosen = Some((pd, name.clone(), matches));
                    if matches {
                        break;
                    }
                }
            }
            let Some((pdev, name, matched)) = chosen else {
                bail!("no Vulkan device with dma-buf export and DRM modifiers");
            };
            println!(
                "vulkan: {name}{}",
                if matched {
                    " (the compositor's main device)"
                } else {
                    " (main device not matched by DRM node)"
                }
            );
            // SAFETY: live physical device.
            let qprops = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let qfi = qprops
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .context("graphics queue")? as u32;
            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfi)
                .queue_priorities(&prio)];
            let ext_names = [
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ];
            let mut f11 =
                vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(true);
            let dci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&qci)
                .enabled_extension_names(&ext_names)
                .push_next(&mut f11);
            // SAFETY: `dci` roots locals that outlive the call.
            let device =
                unsafe { instance.create_device(pdev, &dci, None) }.context("vkCreateDevice")?;
            // SAFETY: the queue family was requested above.
            let queue = unsafe { device.get_device_queue(qfi, 0) };
            let pci = vk::CommandPoolCreateInfo::default()
                .queue_family_index(qfi)
                .flags(vk::CommandPoolCreateFlags::TRANSIENT);
            // SAFETY: live device.
            let pool = unsafe { device.create_command_pool(&pci, None) }?;
            // SAFETY: live physical device.
            let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
            Ok(Gpu {
                _entry: entry,
                instance,
                pdev,
                device,
                queue,
                pool,
                mem_props,
                name,
            })
        }

        /// (modifier, memory plane count) pairs the driver can create `format` with.
        fn driver_modifiers(&self, format: vk::Format) -> Vec<(u64, u32)> {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: read-only query; locals outlive the call.
            unsafe {
                self.instance
                    .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
            };
            let mut props = vec![
                vk::DrmFormatModifierPropertiesEXT::default();
                list.drm_format_modifier_count as usize
            ];
            list.p_drm_format_modifier_properties = props.as_mut_ptr();
            let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: read-only query; `props` outlives the call.
            unsafe {
                self.instance
                    .get_physical_device_format_properties2(self.pdev, format, &mut fp2)
            };
            props.truncate(list.drm_format_modifier_count as usize);
            props
                .iter()
                .filter(|p| {
                    p.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::TRANSFER_DST)
                })
                .map(|p| (p.drm_format_modifier, p.drm_format_modifier_plane_count))
                .collect()
        }

        fn exportable(&self, format: vk::Format, modifier: u64) -> bool {
            let mut mi = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut mi)
                .push_next(&mut ext);
            let mut ep = vk::ExternalImageFormatProperties::default();
            let mut p = vk::ImageFormatProperties2::default().push_next(&mut ep);
            // SAFETY: read-only query; the chain roots locals that outlive the call.
            unsafe {
                self.instance
                    .get_physical_device_image_format_properties2(self.pdev, &info, &mut p)
                    .is_ok()
                    && ep
                        .external_memory_properties
                        .external_memory_features
                        .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
            }
        }

        fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
            let n = self.mem_props.memory_type_count as usize;
            let types = &self.mem_props.memory_types[..n];
            let pick = |need: vk::MemoryPropertyFlags| {
                types
                    .iter()
                    .enumerate()
                    .position(|(i, t)| bits & (1 << i) != 0 && t.property_flags.contains(need))
            };
            pick(want)
                .or_else(|| pick(vk::MemoryPropertyFlags::empty()))
                .map(|i| i as u32)
        }

        /// One exported picture: a moving luma bar over a gradient, four chroma tints. `split`
        /// puts each plane in its own single-plane image and dma-buf.
        fn picture(
            &self,
            fourcc: u32,
            width: u32,
            height: u32,
            candidates: &[u64],
            frame: u32,
            split: bool,
        ) -> Result<Picture> {
            if fourcc == DRM_FORMAT_XR24 {
                let ex = self.exported(vk::Format::B8G8R8A8_UNORM, width, height, candidates)?;
                let rgb = pattern_rgb(width, height, frame);
                let region = vk::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                };
                if let Err(e) = self.upload(ex.image, &rgb, &[region]) {
                    self.destroy_exported(ex);
                    return Err(e);
                }
                return Ok(Picture {
                    images: vec![ex.image],
                    memories: vec![ex.memory],
                    planes: ex.planes.iter().map(|&(o, s)| (0, o, s)).collect(),
                    fds: vec![ex.fd],
                    modifier: ex.modifier,
                    size: ex.size,
                    width,
                    height,
                });
            }
            let ten_bit = fourcc == DRM_FORMAT_P010;
            let (luma, chroma) = pattern(width, height, ten_bit, frame);
            let whole = if ten_bit {
                vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
            } else {
                vk::Format::G8_B8R8_2PLANE_420_UNORM
            };
            let (luma_fmt, chroma_fmt) = if ten_bit {
                (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM)
            } else {
                (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM)
            };
            let layers = |aspect| vk::ImageSubresourceLayers {
                aspect_mask: aspect,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            };
            let region = |aspect, offset: u64, w: u32, h: u32| vk::BufferImageCopy {
                buffer_offset: offset,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(aspect),
                image_offset: vk::Offset3D::default(),
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
            };
            let mut p = Picture {
                images: Vec::new(),
                memories: Vec::new(),
                fds: Vec::new(),
                modifier: 0,
                planes: Vec::new(),
                size: 0,
                width,
                height,
            };
            let push = |p: &mut Picture, ex: Exported, fd_index: usize| {
                p.images.push(ex.image);
                p.memories.push(ex.memory);
                p.fds.push(ex.fd);
                p.size += ex.size;
                for (offset, pitch) in ex.planes {
                    p.planes.push((fd_index, offset, pitch));
                }
                ex.modifier
            };
            if split {
                let l = match self.exported(luma_fmt, width, height, candidates) {
                    Ok(l) => l,
                    Err(e) => {
                        self.destroy(p);
                        return Err(e);
                    }
                };
                let l_mod = l.modifier;
                let mut data = luma;
                if let Err(e) = self.upload(
                    l.image,
                    &data,
                    &[region(vk::ImageAspectFlags::COLOR, 0, width, height)],
                ) {
                    self.destroy_exported(l);
                    self.destroy(p);
                    return Err(e);
                }
                p.modifier = push(&mut p, l, 0);
                let c = match self.exported(chroma_fmt, width / 2, height / 2, &[l_mod]) {
                    Ok(c) => c,
                    Err(e) => {
                        self.destroy(p);
                        return Err(e.context("chroma plane with the luma plane's modifier"));
                    }
                };
                data = chroma;
                if let Err(e) = self.upload(
                    c.image,
                    &data,
                    &[region(
                        vk::ImageAspectFlags::COLOR,
                        0,
                        width / 2,
                        height / 2,
                    )],
                ) {
                    self.destroy_exported(c);
                    self.destroy(p);
                    return Err(e);
                }
                push(&mut p, c, 1);
            } else {
                let ex = match self.exported(whole, width, height, candidates) {
                    Ok(ex) => ex,
                    Err(e) => {
                        self.destroy(p);
                        return Err(e);
                    }
                };
                let mut data = luma;
                let luma_bytes = data.len() as u64;
                data.extend_from_slice(&chroma);
                let regions = [
                    region(vk::ImageAspectFlags::PLANE_0, 0, width, height),
                    region(
                        vk::ImageAspectFlags::PLANE_1,
                        luma_bytes,
                        width / 2,
                        height / 2,
                    ),
                ];
                if let Err(e) = self.upload(ex.image, &data, &regions) {
                    self.destroy_exported(ex);
                    self.destroy(p);
                    return Err(e);
                }
                p.modifier = push(&mut p, ex, 0);
            }
            Ok(p)
        }

        /// One image on a modifier from `candidates`, bound to exportable memory, exported.
        fn exported(
            &self,
            format: vk::Format,
            width: u32,
            height: u32,
            candidates: &[u64],
        ) -> Result<Exported> {
            let usable: Vec<u64> = self
                .driver_modifiers(format)
                .iter()
                .map(|(m, _)| *m)
                .filter(|m| candidates.contains(m))
                .collect();
            if usable.is_empty() {
                bail!("the driver creates {format:?} with none of {candidates:x?}");
            }
            let mut mods = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&usable);
            let mut ext = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let ci = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut mods)
                .push_next(&mut ext);
            let d = &self.device;
            // SAFETY: live device; `ci` roots locals that outlive the call.
            let image =
                unsafe { d.create_image(&ci, None) }.context("vkCreateImage (modifier list)")?;
            let mod_dev = ash::ext::image_drm_format_modifier::Device::new(&self.instance, d);
            let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
            // SAFETY: `image` is live on this device.
            let got = unsafe { mod_dev.get_image_drm_format_modifier_properties(image, &mut mp) };
            if let Err(e) = got {
                // SAFETY: destroying the just-created, never-bound image.
                unsafe { d.destroy_image(image, None) };
                return Err(e).context("vkGetImageDrmFormatModifierPropertiesEXT");
            }
            let modifier = mp.drm_format_modifier;
            let plane_count = self
                .driver_modifiers(format)
                .iter()
                .find(|(m, _)| *m == modifier)
                .map_or(1, |(_, n)| *n);
            // SAFETY: `image` is live.
            let req = unsafe { d.get_image_memory_requirements(image) };
            let Some(ti) =
                self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            else {
                // SAFETY: destroying the just-created, never-bound image.
                unsafe { d.destroy_image(image, None) };
                bail!("memory type for the picture");
            };
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let ai = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            // SAFETY: live device; the chain roots locals that outlive the call.
            let memory = match unsafe { d.allocate_memory(&ai, None) } {
                Ok(m) => m,
                Err(e) => {
                    // SAFETY: destroying the just-created, never-bound image.
                    unsafe { d.destroy_image(image, None) };
                    return Err(e).context("vkAllocateMemory (exportable)");
                }
            };
            let fd_dev = ash::khr::external_memory_fd::Device::new(&self.instance, d);
            let gi = vk::MemoryGetFdInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            // SAFETY: fresh image and memory of the required size; the export names live memory.
            let raw = unsafe {
                d.bind_image_memory(image, memory, 0)
                    .and_then(|()| fd_dev.get_memory_fd(&gi))
            };
            let raw = match raw {
                Ok(fd) => fd,
                Err(e) => {
                    // SAFETY: unwinding the two objects created above.
                    unsafe {
                        d.destroy_image(image, None);
                        d.free_memory(memory, None);
                    }
                    return Err(e).context("bind + vkGetMemoryFdKHR");
                }
            };
            // SAFETY: the driver hands us a fresh fd we own.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let aspects = [
                vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
                vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
                vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
            ];
            let planes: Vec<(u64, u64)> = aspects[..plane_count.clamp(1, 4) as usize]
                .iter()
                .map(|&a| {
                    let sub = vk::ImageSubresource {
                        aspect_mask: a,
                        mip_level: 0,
                        array_layer: 0,
                    };
                    // SAFETY: `image` is live; memory-plane aspects are legal on a modifier image.
                    let l = unsafe { d.get_image_subresource_layout(image, sub) };
                    (l.offset, l.row_pitch)
                })
                .collect();
            Ok(Exported {
                image,
                memory,
                fd,
                modifier,
                planes,
                size: req.size,
            })
        }

        /// Copy `data` into `image` through a staging buffer, leaving it in GENERAL.
        fn upload(
            &self,
            image: vk::Image,
            data: &[u8],
            regions: &[vk::BufferImageCopy],
        ) -> Result<()> {
            let d = &self.device;
            let bci = vk::BufferCreateInfo::default()
                .size(data.len() as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            // SAFETY: live device.
            let staging = unsafe { d.create_buffer(&bci, None) }?;
            // SAFETY: `staging` is live.
            let req = unsafe { d.get_buffer_memory_requirements(staging) };
            let ti = self
                .memory_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .context("host-visible memory")?;
            let ai = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti);
            // SAFETY: live device.
            let smem = unsafe { d.allocate_memory(&ai, None) }?;
            // SAFETY: fresh buffer and memory; the mapping covers `data`.
            unsafe {
                d.bind_buffer_memory(staging, smem, 0)?;
                let p = d.map_memory(smem, 0, req.size, vk::MemoryMapFlags::empty())?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                d.unmap_memory(smem);
            }
            let cbi = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: live pool.
            let cb = unsafe { d.allocate_command_buffers(&cbi) }?[0];
            let all = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let to_dst = vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(all);
            let to_general = vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(all);
            // SAFETY: `cb` is fresh; every handle is live; the submit is waited below.
            unsafe {
                d.begin_command_buffer(
                    cb,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                d.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                d.cmd_copy_buffer_to_image(
                    cb,
                    staging,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    regions,
                );
                d.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_general],
                );
                d.end_command_buffer(cb)?;
                let cbs = [cb];
                let si = vk::SubmitInfo::default().command_buffers(&cbs);
                d.queue_submit(self.queue, &[si], vk::Fence::null())?;
                d.queue_wait_idle(self.queue)?;
                d.free_command_buffers(self.pool, &cbs);
                d.destroy_buffer(staging, None);
                d.free_memory(smem, None);
            }
            Ok(())
        }

        fn destroy_exported(&self, ex: Exported) {
            // SAFETY: nothing on the GPU references the image; the fd is dropped after.
            unsafe {
                self.device.destroy_image(ex.image, None);
                self.device.free_memory(ex.memory, None);
            }
            drop(ex.fd);
        }

        fn destroy(&self, p: Picture) {
            // SAFETY: the compositor's wl_buffer was destroyed and released before this.
            unsafe {
                for image in p.images {
                    self.device.destroy_image(image, None);
                }
                for memory in p.memories {
                    self.device.free_memory(memory, None);
                }
            }
            drop(p.fds);
        }
    }

    /// One image with its exported memory and per-memory-plane layout.
    struct Exported {
        image: vk::Image,
        memory: vk::DeviceMemory,
        fd: OwnedFd,
        modifier: u64,
        planes: Vec<(u64, u64)>,
        size: u64,
    }

    /// The control picture: BGRX gradient with the same moving bar.
    fn pattern_rgb(width: u32, height: u32, frame: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut out = vec![0u8; w * h * 4];
        let bar = ((frame as usize) * 24) % w;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let g = (x * 255 / w) as u8;
                let lit = x >= bar && x < bar + 48;
                out[i] = if y < h / 2 { 200 } else { 40 };
                out[i + 1] = if lit { 255 } else { g };
                out[i + 2] = if lit { 255 } else { 255 - g };
                out[i + 3] = 255;
            }
        }
        out
    }

    /// Luma and chroma planes of the test picture, packed (8-bit, or 10-bit MSB-aligned in 16).
    fn pattern(width: u32, height: u32, ten_bit: bool, frame: u32) -> (Vec<u8>, Vec<u8>) {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w / 2, h / 2);
        let bpp = if ten_bit { 2 } else { 1 };
        let mut luma = vec![0u8; w * h * bpp];
        let mut chroma = vec![0u8; cw * ch * 2 * bpp];
        let bar = ((frame as usize) * 24) % w;
        for y in 0..h {
            for x in 0..w {
                let mut v = (x * 219 / w + 16) as u16;
                if x >= bar && x < bar + 48 {
                    v = 235;
                }
                if y % 128 < 4 {
                    v = 16;
                }
                let i = y * w + x;
                if ten_bit {
                    let s = (v << 8).to_le_bytes();
                    luma[i * 2] = s[0];
                    luma[i * 2 + 1] = s[1];
                } else {
                    luma[i] = v as u8;
                }
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                let cb: u16 = if y < ch / 2 { 90 } else { 170 };
                let cr: u16 = if x < cw / 2 { 90 } else { 170 };
                let i = (y * cw + x) * 2 * bpp;
                if ten_bit {
                    let b = (cb << 8).to_le_bytes();
                    let r = (cr << 8).to_le_bytes();
                    chroma[i..i + 4].copy_from_slice(&[b[0], b[1], r[0], r[1]]);
                } else {
                    chroma[i] = cb as u8;
                    chroma[i + 1] = cr as u8;
                }
            }
        }
        (luma, chroma)
    }

    impl Drop for Gpu {
        fn drop(&mut self) {
            // SAFETY: every picture was destroyed; the pool and device are idle.
            unsafe {
                self.device.destroy_command_pool(self.pool, None);
                self.device.destroy_device(None);
                self.instance.destroy_instance(None);
            }
        }
    }

    // ---- The run ------------------------------------------------------------------------------

    fn arg(args: &[String], key: &str) -> Option<String> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1).cloned())
    }

    fn print_feedback(label: &str, fb: &Feedback) {
        let (maj, min) = dev_major_minor(fb.main_device);
        println!(
            "{label}: main device {maj}:{min}, {} formats in the table, {} tranches",
            fb.table.len(),
            fb.tranches.len()
        );
        for (i, t) in fb.tranches.iter().enumerate() {
            let (tm, tn) = dev_major_minor(t.device);
            let scanout = if t.flags & TRANCHE_SCANOUT != 0 {
                "scanout"
            } else {
                "render"
            };
            println!(
                "  tranche {i}: device {tm}:{tn} {scanout} {} pairs",
                t.formats.len()
            );
            for f in [
                DRM_FORMAT_NV12,
                DRM_FORMAT_P010,
                DRM_FORMAT_XR24,
                DRM_FORMAT_AR30,
                DRM_FORMAT_XB30,
            ] {
                let mods: Vec<String> = t
                    .formats
                    .iter()
                    .filter(|(ff, _)| *ff == f)
                    .map(|(_, m)| format!("{m:#x}"))
                    .collect();
                if !mods.is_empty() {
                    println!("    {} {}", fourcc_name(f), mods.join(" "));
                }
            }
            let mut all: Vec<u32> = t.formats.iter().map(|(f, _)| *f).collect();
            all.sort_unstable();
            all.dedup();
            let names: Vec<String> = all.into_iter().map(fourcc_name).collect();
            println!("    all fourccs: {}", names.join(" "));
        }
    }

    /// Offer `p` to the compositor; `None` when it refuses the import (a `failed` event, not a
    /// protocol error, so the next modifier can be tried).
    fn import(
        conn: &Connection,
        queue: &mut EventQueue<App>,
        app: &mut App,
        dmabuf: &dmabuf::ZwpLinuxDmabufV1,
        qh: &QueueHandle<App>,
        p: &Picture,
        fourcc: u32,
    ) -> Result<Option<wl_buffer::WlBuffer>> {
        let prm = dmabuf.create_params(qh, ());
        for (i, (fd, offset, pitch)) in p.planes.iter().enumerate() {
            prm.add(
                p.fds[*fd].as_fd(),
                i as u32,
                *offset as u32,
                *pitch as u32,
                (p.modifier >> 32) as u32,
                p.modifier as u32,
            );
        }
        app.created = None;
        app.params_failed = false;
        prm.create(
            p.width as i32,
            p.height as i32,
            fourcc,
            params::Flags::empty(),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.created.is_none() && !app.params_failed && Instant::now() < deadline {
            pump(conn, queue, app, Duration::from_millis(50))?;
        }
        prm.destroy();
        Ok(app.created.take())
    }

    struct Layout<'a> {
        name: &'static str,
        video: wl_surface::WlSurface,
        _viewport: Option<wp_viewport::WpViewport>,
        _sub: Option<wl_subsurface::WlSubsurface>,
        _repr: Option<crs::WpColorRepresentationSurfaceV1>,
        parent: &'a wl_surface::WlSurface,
    }

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let fourcc = match arg(&args, "--format").as_deref() {
            None | Some("nv12") => DRM_FORMAT_NV12,
            Some("p010") => DRM_FORMAT_P010,
            // The control: an RGB buffer exported the same way tells an import that refuses
            // YUV from an export that is wrong.
            Some("xr24") => DRM_FORMAT_XR24,
            Some(other) => bail!("unknown --format {other} (nv12|p010|xr24)"),
        };
        let layouts: Vec<&str> = match arg(&args, "--layout").as_deref() {
            None | Some("both") => vec!["toplevel", "subsurface"],
            Some("toplevel") => vec!["toplevel"],
            Some("subsurface") => vec!["subsurface"],
            Some(other) => bail!("unknown --layout {other}"),
        };
        let seconds: u64 = arg(&args, "--seconds")
            .and_then(|s| s.parse().ok())
            .unwrap_or(4);
        let size_arg = arg(&args, "--size").and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        });
        let dump_only = args.iter().any(|a| a == "--dump-only");

        let conn = Connection::connect_to_env().context("connect to the Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut app = App::default();
        queue.roundtrip(&mut app)?;

        let of_interest = [
            "wl_compositor",
            "wl_subcompositor",
            "zwp_linux_dmabuf_v1",
            "wp_viewporter",
            "wp_presentation",
            "wp_color_representation_manager_v1",
            "wp_color_manager_v1",
            "wp_linux_drm_syncobj_manager_v1",
            "wp_single_pixel_buffer_manager_v1",
            "wp_fifo_manager_v1",
            "wp_commit_timing_manager_v1",
            "wp_tearing_control_manager_v1",
            "wp_fractional_scale_manager_v1",
        ];
        let listed: Vec<String> = of_interest
            .iter()
            .map(|n| match app.globals.get(*n) {
                Some(v) => format!("{n} v{v}"),
                None => format!("{n} -"),
            })
            .collect();
        println!("protocols: {}", listed.join(", "));

        let (Some(comp), Some(wm)) = (app.compositor.clone(), app.wm.clone()) else {
            bail!("the compositor lacks wl_compositor or xdg_wm_base");
        };
        let Some(dmabuf) = app.dmabuf.clone() else {
            bail!(
                "zwp_linux_dmabuf_v1 v{} — feedback needs v4; nothing to measure",
                app.dmabuf_version
            );
        };
        dmabuf.get_default_feedback(&qh, FbKind::Default);
        queue.roundtrip(&mut app)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.default_fb.done && Instant::now() < deadline {
            pump(&conn, &mut queue, &mut app, Duration::from_millis(200))?;
        }
        print_feedback("default feedback", &app.default_fb);
        if !app.repr_pairs.is_empty() {
            println!("color-representation pairs: {}", app.repr_pairs.join(" "));
        }

        // The window: fullscreen toplevel, configured before any buffer.
        let parent = comp.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&parent, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title("punktfunk wl-scanout-probe".into());
        top.set_app_id("io.unom.punktfunk.wl-scanout-probe".into());
        top.set_fullscreen(None);
        parent.commit();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.configured && Instant::now() < deadline {
            pump(&conn, &mut queue, &mut app, Duration::from_millis(200))?;
        }
        if !app.configured {
            bail!("no xdg configure within 5 s");
        }
        let (win_w, win_h) = if app.size.0 > 0 {
            app.size
        } else {
            (1920, 1080)
        };
        let (bw, bh) = size_arg.unwrap_or((win_w as u32, win_h as u32));
        let (bw, bh) = (bw & !1, bh & !1);
        println!(
            "window {win_w}x{win_h}, picture {bw}x{bh} {}",
            fourcc_name(fourcc)
        );

        dmabuf.get_surface_feedback(&parent, &qh, FbKind::Surface);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.surface_fb.done && Instant::now() < deadline {
            pump(&conn, &mut queue, &mut app, Duration::from_millis(200))?;
        }
        print_feedback("surface feedback (unmapped)", &app.surface_fb);
        if dump_only {
            return Ok(());
        }

        // Candidate modifiers: the compositor's, scanout tranche first, that the driver exports.
        let gpu = Gpu::open(app.default_fb.main_device)?;
        let format = match fourcc {
            DRM_FORMAT_P010 => vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            DRM_FORMAT_XR24 => vk::Format::B8G8R8A8_UNORM,
            _ => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        };
        let driver: Vec<(u64, u32)> = gpu.driver_modifiers(format);
        let fb = if app.surface_fb.done {
            &app.surface_fb
        } else {
            &app.default_fb
        };
        let mut offered = fb.modifiers(fourcc);
        offered.sort_by_key(|(_, scanout)| !*scanout);
        let only: Option<u64> = match arg(&args, "--modifier").as_deref() {
            None | Some("auto") => None,
            Some("linear") => Some(0),
            Some(hex) => Some(
                u64::from_str_radix(hex.trim_start_matches("0x"), 16)
                    .context("--modifier takes linear|auto|<hex>")?,
            ),
        };
        let candidates: Vec<u64> = offered
            .iter()
            .map(|(m, _)| *m)
            .filter(|m| only.is_none_or(|o| o == *m))
            .filter(|m| driver.iter().any(|(dm, _)| dm == m))
            .filter(|m| gpu.exportable(format, *m))
            .collect();
        println!(
            "modifiers: compositor offers {} for {}, driver creates {}, exportable intersection {}: {}",
            offered.len(),
            fourcc_name(fourcc),
            driver.len(),
            candidates.len(),
            candidates.iter().map(|m| format!("{m:#x}")).collect::<Vec<_>>().join(" ")
        );
        if candidates.is_empty() {
            bail!(
                "no (format, modifier) both sides take — this compositor cannot take {} from {}",
                fourcc_name(fourcc),
                gpu.name
            );
        }

        let shm = app.shm.clone().context("wl_shm")?;
        let subcomp = app.subcompositor.clone();
        let viewporter = app.viewporter.clone();
        let presentation = app
            .presentation
            .clone()
            .context("wp_presentation: no on-glass stamps here")?;
        let clock = app.clock_id.unwrap_or(1);

        let plane_modes: Vec<bool> = match arg(&args, "--planes").as_deref() {
            None | Some("both") => vec![false, true],
            Some("joint") => vec![false],
            Some("split") => vec![true],
            Some(other) => bail!("unknown --planes {other} (joint|split|both)"),
        };
        let mut winner: Option<(u64, bool)> = None;
        for name in layouts {
            if name == "subsurface" && subcomp.is_none() {
                println!("subsurface: skipped, this compositor has no wl_subcompositor");
                continue;
            }
            // Two pictures, alternated, so the compositor may hold one while the next is
            // committed. The first one also finds a modifier and plane layout the compositor
            // imports: joint (one dma-buf, two planes) before split (one dma-buf per plane).
            let order: Vec<(u64, bool)> = match winner {
                Some(w) => vec![w],
                None => candidates
                    .iter()
                    .flat_map(|&m| plane_modes.iter().map(move |&s| (m, s)))
                    .collect(),
            };
            let mut pics: Vec<Picture> = Vec::new();
            let mut buffers: Vec<wl_buffer::WlBuffer> = Vec::new();
            let mut refused: Vec<String> = Vec::new();
            for &(m, split) in &order {
                let tag = format!("{m:#x}{}", if split { "/split" } else { "" });
                let p = match gpu.picture(fourcc, bw, bh, &[m], 0, split) {
                    Ok(p) => p,
                    Err(e) => {
                        refused.push(format!("{tag} (create: {e:#})"));
                        continue;
                    }
                };
                match import(&conn, &mut queue, &mut app, &dmabuf, &qh, &p, fourcc)? {
                    Some(b) => {
                        winner = Some((m, split));
                        pics.push(p);
                        buffers.push(b);
                        break;
                    }
                    None => {
                        refused.push(tag);
                        gpu.destroy(p);
                    }
                }
            }
            let Some((m, split)) = winner else {
                bail!(
                    "{name}: the compositor refused every modifier it offered: {}",
                    refused.join(" ")
                );
            };
            let second = gpu.picture(fourcc, bw, bh, &[m], 1, split)?;
            let Some(b) = import(&conn, &mut queue, &mut app, &dmabuf, &qh, &second, fourcc)?
            else {
                bail!("{name}: the second picture was refused with the layout the first took");
            };
            pics.push(second);
            buffers.push(b);
            println!(
                "{name}: modifier {m:#x} {} imported (refused: {}), planes (fd, offset, pitch) {:?}, allocation {} bytes",
                if split { "one dma-buf per plane" } else { "one dma-buf, two planes" },
                if refused.is_empty() { "none".to_string() } else { refused.join(" ") },
                pics[0].planes,
                pics[0].size
            );

            let layout = match name {
                "toplevel" => {
                    let viewport = viewporter.as_ref().map(|v| {
                        let vp = v.get_viewport(&parent, &qh, ());
                        vp.set_destination(win_w, win_h);
                        vp
                    });
                    let region = comp.create_region(&qh, ());
                    region.add(0, 0, win_w, win_h);
                    parent.set_opaque_region(Some(&region));
                    region.destroy();
                    Layout {
                        name: "toplevel",
                        video: parent.clone(),
                        _viewport: viewport,
                        _sub: None,
                        _repr: None,
                        parent: &parent,
                    }
                }
                _ => {
                    let Some(sc) = subcomp.as_ref() else {
                        bail!("no wl_subcompositor");
                    };
                    // Parent: an opaque black shm buffer at the window size.
                    let stride = win_w * 4;
                    let bytes = stride * win_h;
                    let memfd = rustix::fs::memfd_create(
                        "wl-scanout-probe",
                        rustix::fs::MemfdFlags::CLOEXEC,
                    )?;
                    rustix::fs::ftruncate(&memfd, bytes as u64)?;
                    let pool = shm.create_pool(memfd.as_fd(), bytes, &qh, ());
                    let black = pool.create_buffer(
                        0,
                        win_w,
                        win_h,
                        stride,
                        wl_shm::Format::Xrgb8888,
                        &qh,
                        (),
                    );
                    pool.destroy();
                    let region = comp.create_region(&qh, ());
                    region.add(0, 0, win_w, win_h);
                    parent.set_opaque_region(Some(&region));
                    parent.attach(Some(&black), 0, 0);
                    parent.damage_buffer(0, 0, i32::MAX, i32::MAX);
                    parent.commit();
                    let video = comp.create_surface(&qh, ());
                    let sub = sc.get_subsurface(&video, &parent, &qh, ());
                    sub.set_position(0, 0);
                    sub.set_desync();
                    video.set_opaque_region(Some(&region));
                    region.destroy();
                    let viewport = viewporter.as_ref().map(|v| {
                        let vp = v.get_viewport(&video, &qh, ());
                        vp.set_destination(win_w, win_h);
                        vp
                    });
                    // Coefficients belong to YCbCr buffers only; Mutter faults an RGB commit.
                    let repr = app
                        .color_repr
                        .as_ref()
                        .filter(|_| fourcc != DRM_FORMAT_XR24)
                        .map(|m| {
                            let s = m.get_surface(&video, &qh, ());
                            s.set_coefficients_and_range(
                                crs::Coefficients::Bt709,
                                crs::Range::Limited,
                            );
                            s
                        });
                    Layout {
                        name: "subsurface",
                        video,
                        _viewport: viewport,
                        _sub: Some(sub),
                        _repr: repr,
                        parent: &parent,
                    }
                }
            };
            let _top_repr = (layout.name == "toplevel" && fourcc != DRM_FORMAT_XR24)
                .then_some(app.color_repr.as_ref())
                .flatten()
                .map(|m| {
                    let s = m.get_surface(&parent, &qh, ());
                    s.set_coefficients_and_range(crs::Coefficients::Bt709, crs::Range::Limited);
                    s
                });

            app.samples.clear();
            app.releases = 0;
            let start = Instant::now();
            let mut frame = 0usize;
            while start.elapsed() < Duration::from_secs(seconds) && !app.closed {
                let buffer = &buffers[frame % 2];
                layout.video.attach(Some(buffer), 0, 0);
                layout.video.damage_buffer(0, 0, i32::MAX, i32::MAX);
                presentation.feedback(&layout.video, &qh, frame);
                app.samples.push(Sample {
                    commit_ns: now_ns(clock).unwrap_or(0),
                    presented_ns: None,
                    flags: 0,
                    refresh_ns: 0,
                    discarded: false,
                });
                layout.video.commit();
                if layout.name == "subsurface" && frame == 0 {
                    layout.parent.commit();
                }
                conn.flush()?;
                // Wait for this frame's verdict, then commit the next: one frame in flight.
                let wait_until = Instant::now() + Duration::from_millis(500);
                loop {
                    let s = &app.samples[frame];
                    if s.presented_ns.is_some() || s.discarded || Instant::now() >= wait_until {
                        break;
                    }
                    pump(&conn, &mut queue, &mut app, Duration::from_millis(20))?;
                }
                frame += 1;
            }
            let mut deltas: Vec<f64> = app
                .samples
                .iter()
                .filter_map(|s| {
                    s.presented_ns
                        .map(|p| (p as f64 - s.commit_ns as f64) / 1e6)
                })
                .collect();
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let pct = |q: f64| {
                deltas
                    .get(((deltas.len() as f64 - 1.0) * q) as usize)
                    .copied()
                    .unwrap_or(f64::NAN)
            };
            let presented = deltas.len();
            let zero_copy = app
                .samples
                .iter()
                .filter(|s| s.presented_ns.is_some() && s.flags & KIND_ZERO_COPY != 0)
                .count();
            let vsync = app
                .samples
                .iter()
                .filter(|s| s.presented_ns.is_some() && s.flags & KIND_VSYNC != 0)
                .count();
            let discarded = app.samples.iter().filter(|s| s.discarded).count();
            let refresh = app
                .samples
                .iter()
                .find_map(|s| (s.refresh_ns > 0).then_some(s.refresh_ns))
                .unwrap_or(0);
            println!(
                "{}: committed {} presented {presented} discarded {discarded} · zero-copy {zero_copy} vsync {vsync} · commit→glass p50 {:.2} p95 {:.2} ms · refresh {:.2} ms · releases {}",
                layout.name,
                app.samples.len(),
                pct(0.5),
                pct(0.95),
                refresh as f64 / 1e6,
                app.releases
            );

            // Unmap the video and let the compositor release the buffers before destroying them.
            layout.video.attach(None, 0, 0);
            layout.video.commit();
            if layout.name == "subsurface" {
                layout.parent.commit();
            }
            conn.flush()?;
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                pump(&conn, &mut queue, &mut app, Duration::from_millis(50))?;
            }
            for b in buffers {
                b.destroy();
            }
            if let Some(sub) = layout._sub {
                sub.destroy();
            }
            if layout.name == "subsurface" {
                layout.video.destroy();
            }
            queue.roundtrip(&mut app)?;
            for p in pics {
                gpu.destroy(p);
            }
        }
        Ok(())
    }
}
