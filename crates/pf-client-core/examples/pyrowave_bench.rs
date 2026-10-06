//! Headless PyroWave decode bench: the client decoder over a `PUNKTFUNK_DUMP_VIDEO`
//! capture, with no window and no network.
//!
//! `cargo run --release -p pf-client-core --example pyrowave_bench -- <capture> [WxH] [bits] [secs]`
//!
//! Prints the frame rate, the per-frame split (parse, record, GPU) and the decoder's own
//! GPU stage times. `PYROWAVE_BENCH_GPU=<substring>` picks the adapter. A capture comes
//! from a real session, or from pf-encode's `pyrowave_dump_bench_capture`.
//!
//! `PYROWAVE_BENCH_LOAD=<ms>` adds what a presenter adds: a full-size pass on the
//! graphics queue every that many milliseconds, timed from submit to done.
//! `PYROWAVE_BENCH_QUEUE=compute` decodes on the compute-only family instead.

#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!("pyrowave_bench runs on Linux and Windows");
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> anyhow::Result<()> {
    bench::run()
}

#[cfg(any(target_os = "linux", windows))]
mod bench {
    use anyhow::{Context as _, Result};
    use ash::vk;
    use ash::vk::Handle as _;
    use pf_client_core::video_color::ColorDesc;
    use pf_client_core::video_pyrowave::PyroWaveDecoder;
    use pf_client_core::video_vk::{QueueLock, VulkanDecodeDevice};
    use std::time::{Duration, Instant};

    /// One capture AU: byte range, chunk-aligned, complete.
    type Au = (std::ops::Range<usize>, bool, bool);

    fn load(path: &str) -> Result<(Vec<u8>, Vec<Au>)> {
        let data = std::fs::read(path).with_context(|| format!("read {path}"))?;
        let idx = std::fs::read_to_string(format!("{path}.idx"))
            .with_context(|| format!("read {path}.idx"))?;
        let mut aus = Vec::new();
        for line in idx.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            let [off, len, flags, complete] = f[..] else {
                anyhow::bail!("index line is not `offset len flags complete`: {line}");
            };
            let (off, len): (usize, usize) = (off.parse()?, len.parse()?);
            let flags = u32::from_str_radix(flags.trim_start_matches("0x"), 16)?;
            aus.push((off..off + len, flags & 0x40 != 0, complete == "1"));
        }
        // The bitstream's frame counter is three bits: a loop that is a multiple of
        // eight replays without a rewind.
        aus.truncate(aus.len() - aus.len() % 8);
        anyhow::ensure!(!aus.is_empty(), "capture holds fewer than eight AUs");
        Ok((data, aus))
    }

    /// One full-size blit on the graphics queue every `every_ms`: the wall time of each
    /// from submit to done, in microseconds.
    ///
    /// # Safety
    ///
    /// `device` is live for the whole call and `mem` are its physical device's. `lock` is
    /// the queue's lock when another thread submits to family `qf` too.
    #[allow(clippy::too_many_arguments)]
    unsafe fn present_load(
        device: &ash::Device,
        mem: &vk::PhysicalDeviceMemoryProperties,
        qf: u32,
        w: u32,
        h: u32,
        every_ms: u64,
        stop: &std::sync::atomic::AtomicBool,
        lock: Option<&QueueLock>,
    ) -> Result<Vec<u32>> {
        // SAFETY: fn contract; every create-info is a local that outlives its call, and
        // every handle made here is used on this thread only and freed before return.
        unsafe {
            let queue = device.get_device_queue(qf, 0);
            let image = || -> Result<(vk::Image, vk::DeviceMemory)> {
                let img = device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::A2B10G10R10_UNORM_PACK32)
                        .extent(vk::Extent3D {
                            width: w,
                            height: h,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .usage(
                            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                        ),
                    None,
                )?;
                let req = device.get_image_memory_requirements(img);
                let ti = (0..mem.memory_type_count)
                    .find(|&i| {
                        req.memory_type_bits & (1 << i) != 0
                            && mem.memory_types[i as usize]
                                .property_flags
                                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                    })
                    .context("no device-local memory type")?;
                let m = device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ti),
                    None,
                )?;
                device.bind_image_memory(img, m, 0)?;
                Ok((img, m))
            };
            let (src, src_mem) = image()?;
            let (dst, dst_mem) = image()?;
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qf)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1);
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let corner = vk::Offset3D {
                x: w as i32,
                y: h as i32,
                z: 1,
            };
            let blit = [vk::ImageBlit::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .src_offsets([vk::Offset3D::default(), corner])
                .dst_offsets([vk::Offset3D::default(), corner])];
            let mut times = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
                // The contents are never read back: UNDEFINED every pass is enough.
                let to = |img, layout, access| {
                    vk::ImageMemoryBarrier::default()
                        .image(img)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(layout)
                        .dst_access_mask(access)
                        .subresource_range(range)
                };
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[
                        to(
                            src,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            vk::AccessFlags::TRANSFER_READ,
                        ),
                        to(
                            dst,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                        ),
                    ],
                );
                device.cmd_blit_image(
                    cmd,
                    src,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &blit,
                    vk::Filter::NEAREST,
                );
                device.end_command_buffer(cmd)?;
                device.reset_fences(&[fence])?;
                let t = Instant::now();
                let cmds = [cmd];
                {
                    let _held = lock.map(QueueLock::guard);
                    device.queue_submit(
                        queue,
                        &[vk::SubmitInfo::default().command_buffers(&cmds)],
                        fence,
                    )?;
                }
                device.wait_for_fences(&[fence], true, u64::MAX)?;
                times.push(t.elapsed().as_micros() as u32);
                std::thread::sleep(Duration::from_millis(every_ms));
            }
            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
            for (img, m) in [(src, src_mem), (dst, dst_mem)] {
                device.destroy_image(img, None);
                device.free_memory(m, None);
            }
            Ok(times)
        }
    }

    fn pct(sorted: &[u32], p: usize) -> f64 {
        f64::from(sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]) / 1000.0
    }

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let path = args
            .first()
            .context("usage: pyrowave_bench <capture> [WxH] [bits] [secs]")?;
        let (w, h) = args
            .get(1)
            .and_then(|s| s.split_once('x'))
            .map(|(w, h)| Ok::<_, std::num::ParseIntError>((w.parse()?, h.parse()?)))
            .transpose()?
            .unwrap_or((3840u32, 2160u32));
        let bits: u8 = args.get(2).map_or(Ok(10), |s| s.parse())?;
        let secs: u64 = args.get(3).map_or(Ok(5), |s| s.parse())?;
        let (data, aus) = load(path)?;

        // SAFETY: loads the system Vulkan loader; nothing else holds it.
        let entry = unsafe { ash::Entry::load() }?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        // SAFETY: the create-info is a local that outlives the call.
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }?;
        let want = std::env::var("PYROWAVE_BENCH_GPU").ok();
        // SAFETY: `instance` is live; the same holds for every query below.
        let (pdev, props) = unsafe { instance.enumerate_physical_devices() }?
            .into_iter()
            .map(|p| (p, unsafe { instance.get_physical_device_properties(p) }))
            .find(|(_, props)| {
                let name = props.device_name_as_c_str().unwrap_or_default();
                match &want {
                    Some(w) => name.to_string_lossy().contains(w.as_str()),
                    None => props.device_type != vk::PhysicalDeviceType::CPU,
                }
            })
            .context("no matching Vulkan adapter")?;
        let name = props
            .device_name_as_c_str()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // SAFETY: as above.
        let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        let family = |want: vk::QueueFlags, not: vk::QueueFlags| {
            families
                .iter()
                .position(|q| q.queue_flags.contains(want) && !q.queue_flags.intersects(not))
                .map(|i| i as u32)
        };
        let gfx_qf = family(
            vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE,
            vk::QueueFlags::empty(),
        )
        .context("no graphics+compute queue family")?;
        let compute_qf = family(vk::QueueFlags::COMPUTE, vk::QueueFlags::GRAPHICS);
        let qf = match std::env::var("PYROWAVE_BENCH_QUEUE").as_deref() {
            Ok("compute") => compute_qf.context("no compute-only queue family")?,
            _ => gfx_qf,
        };
        let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut have = vk::PhysicalDeviceFeatures2::default().push_next(&mut have12);
        // SAFETY: as above; the chain is two locals.
        unsafe { instance.get_physical_device_features2(pdev, &mut have) };
        let float16 = have12.shader_float16 == vk::TRUE;

        // The feature set the presenter enables for the wavelet lane (`vk/setup.rs`).
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .storage_buffer8_bit_access(true)
            .shader_float16(float16);
        let mut v13 = vk::PhysicalDeviceVulkan13Features::default()
            .synchronization2(true)
            .subgroup_size_control(true)
            .compute_full_subgroups(true);
        let mut feat = vk::PhysicalDeviceFeatures2::default()
            .features(vk::PhysicalDeviceFeatures::default().shader_int16(true))
            .push_next(&mut v12)
            .push_next(&mut v13);
        let prio = [1.0f32];
        let mut queue_families = vec![gfx_qf];
        queue_families.extend(compute_qf);
        let queues: Vec<_> = queue_families
            .iter()
            .map(|&f| {
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(f)
                    .queue_priorities(&prio)
            })
            .collect();
        // SAFETY: the create-info and its chain are locals that outlive the call.
        let device = unsafe {
            instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .push_next(&mut feat),
                None,
            )
        }?;

        let vkd = VulkanDecodeDevice {
            get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
            instance: instance.handle().as_raw() as usize,
            physical_device: pdev.as_raw() as usize,
            device: device.handle().as_raw() as usize,
            vendor_id: props.vendor_id,
            device_name: name.clone(),
            graphics_qf: qf,
            decode_qf: qf,
            decode_video_caps: 0,
            instance_extensions: Vec::new(),
            device_extensions: Vec::new(),
            f_sampler_ycbcr: false,
            f_timeline_semaphore: true,
            f_synchronization2: true,
            video_decode: false,
            present_timing: false,
            pyrowave_decode: true,
            f_shader_int16: true,
            f_storage_buffer8: true,
            f_subgroup_size_control: true,
            f_compute_full_subgroups: true,
            f_shader_float16: float16,
            api_version: props.api_version,
            queue_families,
            d3d11_import: false,
            dmabuf_import: false,
            vaapi_av1_decode: false,
            vaapi_hevc_decode: false,
            d3d11_hdr10: false,
            d3d11_nv12: false,
            d3d11_p010: false,
            adapter_luid: None,
            queue_lock: std::sync::Arc::new(QueueLock::new()),
        };
        let color = ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        let mut dec = PyroWaveDecoder::new(&vkd, w, h, 1408, false, color, bits >= 10)?;

        let mean = aus.iter().map(|a| a.0.len()).sum::<usize>() / aus.len();
        println!(
            "{name}: {w}x{h} {bits}-bit, {} AUs, mean {mean} bytes, float16={float16}, \
             decode on family {qf} (graphics {gfx_qf}, compute {compute_qf:?})",
            aus.len()
        );
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let load = std::env::var("PYROWAVE_BENCH_LOAD")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            .map(|ms| {
                let (device, stop) = (device.clone(), stop.clone());
                // One queue, two threads: the decoder's lock, as a presenter takes it.
                let lock = (qf == gfx_qf).then(|| vkd.queue_lock.clone());
                // SAFETY: as above.
                let mem = unsafe { instance.get_physical_device_memory_properties(pdev) };
                std::thread::spawn(move || {
                    // SAFETY: `device` outlives the thread: it is joined before the destroy.
                    unsafe { present_load(&device, &mem, gfx_qf, w, h, ms, &stop, lock.as_deref()) }
                })
            });
        // One pass to warm pipelines and the plane ring, then the timed run.
        let (mut wall, mut parse, mut record, mut gpu) = (vec![], vec![], vec![], vec![]);
        let mut skipped = 0u32;
        for timed in [false, true] {
            let started = Instant::now();
            let limit = Duration::from_secs(if timed { secs } else { 1 });
            'run: loop {
                for (range, aligned, complete) in &aus {
                    let t = Instant::now();
                    let out = dec.decode_frame(&data[range.clone()], *aligned, *complete)?;
                    if timed {
                        if out.is_none() {
                            skipped += 1;
                        }
                        let s = dec.last_split();
                        wall.push(t.elapsed().as_micros() as u32);
                        parse.push(s.parse_us);
                        record.push(s.record_us);
                        gpu.push(s.gpu_us);
                    }
                }
                if started.elapsed() >= limit {
                    if timed {
                        let fps = wall.len() as f64 / started.elapsed().as_secs_f64();
                        println!(
                            "{fps:.1} frames/s over {} frames, {skipped} skipped",
                            wall.len()
                        );
                    } else {
                        dec.gpu_stage_report(true);
                    }
                    break 'run;
                }
            }
        }
        for (label, v) in [
            ("frame ", &mut wall),
            ("parse ", &mut parse),
            ("record", &mut record),
            ("gpu   ", &mut gpu),
        ] {
            v.sort_unstable();
            println!(
                "{label} p50 {:6.2} ms  p95 {:6.2}  max {:6.2}",
                pct(v, 50),
                pct(v, 95),
                pct(v, 100)
            );
        }
        for line in dec.gpu_stage_report(false) {
            println!("  {line}");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut v) = load.map(|t| t.join().expect("load thread")).transpose()? {
            v.sort_unstable();
            println!(
                "pass   p50 {:6.2} ms  p95 {:6.2}  max {:6.2}  ({} passes on the graphics queue)",
                pct(&v, 50),
                pct(&v, 95),
                pct(&v, 100),
                v.len()
            );
        }
        drop(dec);
        // SAFETY: the decoder is gone and idled the queue on drop; nothing else uses these.
        unsafe {
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
        Ok(())
    }
}
