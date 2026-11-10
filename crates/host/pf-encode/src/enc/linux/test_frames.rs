//! Capture frames the Linux encoder tests feed their encoders: CPU frames, and a LINEAR
//! dmabuf with an edge pattern whose decoded picture shows a misread row pitch.

use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// A BGRX frame of one `fill` colour.
pub fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&fill);
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

/// Packed 24-bpp CPU frame. `rgb` is (r, g, b) regardless of `fmt`'s byte order.
pub fn cpu_frame_24(w: u32, h: u32, pts_ns: u64, rgb: [u8; 3], fmt: PixelFormat) -> CapturedFrame {
    let px = match fmt {
        PixelFormat::Rgb => [rgb[0], rgb[1], rgb[2]],
        PixelFormat::Bgr => [rgb[2], rgb[1], rgb[0]],
        _ => unreachable!("24-bpp helper"),
    };
    let mut buf = vec![0u8; (w * h * 3) as usize];
    for p in buf.chunks_exact_mut(3) {
        p.copy_from_slice(&px);
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

/// A LINEAR dma-buf holding `bytes`, exported from its own Vulkan device the way gamescope
/// shares a screencast buffer. Lives until this drops.
pub(crate) struct LinearDmabuf {
    fd: std::os::fd::OwnedFd,
    _vk: (ash::Entry, ash::Instance, ash::Device),
}

impl LinearDmabuf {
    pub(crate) fn new(bytes: &[u8]) -> LinearDmabuf {
        use ash::vk;
        use pf_zerocopy::vkdev::{memory_type, MappedMemory};
        // SAFETY: test fixture over raw Vulkan handles it creates; the device outlives the fd's
        // users because the struct keeps it.
        unsafe {
            let entry = ash::Entry::load().expect("load libvulkan");
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .expect("instance");
            let phys = instance
                .enumerate_physical_devices()
                .expect("devices")
                .into_iter()
                .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
                .expect("an NVIDIA device");
            let mp = instance.get_physical_device_memory_properties(phys);
            let exts = [
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ];
            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(0)
                .queue_priorities(&prio)];
            let device = instance
                .create_device(
                    phys,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&qci)
                        .enabled_extension_names(&exts),
                    None,
                )
                .expect("device");
            let size = bytes.len() as u64;
            let buffer = |usage, export: bool| {
                let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let mut info = vk::BufferCreateInfo::default().size(size).usage(usage);
                if export {
                    info = info.push_next(&mut ext);
                }
                let buf = device.create_buffer(&info, None).expect("buffer");
                let req = device.get_buffer_memory_requirements(buf);
                let flags = if export {
                    vk::MemoryPropertyFlags::DEVICE_LOCAL
                } else {
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
                };
                let mut exp = vk::ExportMemoryAllocateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let mut alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(
                        memory_type(&mp, req.memory_type_bits, flags).expect("type"),
                    );
                if export {
                    alloc = alloc.push_next(&mut exp);
                }
                let mem = device.allocate_memory(&alloc, None).expect("memory");
                device.bind_buffer_memory(buf, mem, 0).expect("bind");
                (buf, mem)
            };
            let (dst, dst_mem) = buffer(vk::BufferUsageFlags::TRANSFER_DST, true);
            let (src, src_mem) = buffer(vk::BufferUsageFlags::TRANSFER_SRC, false);
            MappedMemory::map(&device, src_mem, bytes.len())
                .expect("map staging")
                .copy_from_slice(bytes);
            let pool = device
                .create_command_pool(&vk::CommandPoolCreateInfo::default(), None)
                .expect("pool");
            let cmd = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .command_buffer_count(1),
                )
                .expect("cmd")[0];
            device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .expect("begin");
            device.cmd_copy_buffer(cmd, src, dst, &[vk::BufferCopy::default().size(size)]);
            device.end_command_buffer(cmd).expect("end");
            let cmds = [cmd];
            let queue = device.get_device_queue(0, 0);
            device
                .queue_submit(
                    queue,
                    &[vk::SubmitInfo::default().command_buffers(&cmds)],
                    vk::Fence::null(),
                )
                .expect("submit");
            device.queue_wait_idle(queue).expect("wait");
            let fd = ash::khr::external_memory_fd::Device::new(&instance, &device)
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(dst_mem)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
                )
                .expect("export dma-buf");
            LinearDmabuf {
                fd: std::os::fd::FromRawFd::from_raw_fd(fd),
                _vk: (entry, instance, device),
            }
        }
    }

    /// One captured frame over this buffer, held the way the capture holds it.
    pub(crate) fn frame(
        &self,
        fmt: PixelFormat,
        fourcc: &[u8; 4],
        w: u32,
        h: u32,
        stride: u32,
        i: u32,
    ) -> CapturedFrame {
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: u64::from(i) * 16_666_667,
            format: fmt,
            payload: FramePayload::Dmabuf(pf_frame::DmabufFrame {
                fd: self.fd.try_clone().expect("dup"),
                fourcc: u32::from_le_bytes(*fourcc),
                modifier: 0,
                plane1: None,
                offset: 0,
                stride,
                hold: Some(std::sync::Arc::new(())),
                health: pf_zerocopy::zero_copy_health(0x5002),
                rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            cursor: None,
        }
    }
}

/// The first picture of `stream` as raw `pix_fmt` bytes, through ffmpeg.
pub(crate) fn decode_first(stream: &[u8], name: &str, pix_fmt: &str) -> Vec<u8> {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, stream).expect("write the stream");
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A `w`-wide BGRx frame at `stride`: dark left half, bright right half.
pub(crate) fn edge_frame(w: u32, h: u32, stride: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; (stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let v = if x < w / 2 { 40 } else { 235 };
            let i = (y * stride + x * 4) as usize;
            bytes[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    bytes
}

/// The edge of [`edge_frame`] sits in one column on every row of the decoded picture: a row
/// pitch misread anywhere on the way walks it sideways.
pub(crate) fn assert_edge_is_straight(stream: &[u8], name: &str, w: u32, h: u32) {
    let out = decode_first(stream, name, "gray");
    for y in [8, h - 8] {
        let edge = (0..w).find(|&x| out[(y * w + x) as usize] > 128);
        assert_eq!(edge, Some(w / 2), "row {y}");
    }
}

/// 1690 pixels: 6760-byte rows, off every 32-byte boundary.
pub(crate) const ODD_W: u32 = 1690;
pub(crate) const ODD_H: u32 = 128;
