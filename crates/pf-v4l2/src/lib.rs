//! V4L2 decoder nodes: the `unsafe` half of the Linux client's V4L2 rungs.
//!
//! Its own crate for the reason `pf-libva` is one. [`pf_v4l2dec`] forbids
//! `unsafe` so its flows and layouts test on any OS; this is the ioctls, the
//! buffer mappings and the fds those flows drive. It links nothing but libc,
//! so its one hardware-shaped test runs on a bare machine against the
//! kernel's `visl` virtual decoder.
//!
//! [`Node`] is a stateful decoder ([`pf_v4l2dec::stateful::Device`]).
//! [`RequestNode`] is a stateless one ([`pf_v4l2dec::stateless::Device`]):
//! the video node, the media device that owns its requests, and one request.
//! Both export their CAPTURE buffers as dma-bufs; a `RequestNode` also maps
//! them, for picture formats nothing imports.
//!
//! Empty off Linux. The layouts are the 64-bit little-endian ABI; callers
//! gate on that before they open a node.

#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;

use pf_v4l2dec::stateful::CaptureFormat;
use pf_v4l2dec::stateful::Dequeued;
use pf_v4l2dec::stateful::Event;
use pf_v4l2dec::stateful::Interest;
use pf_v4l2dec::stateful::Queue;
use pf_v4l2dec::stateless::Control;
use pf_v4l2dec::stateless::ControlRange;
use pf_v4l2dec::uapi;
use pf_v4l2dec::uapi_stateless as req;

/// `ioctl` with `EINTR` retried. `T` must be the struct `request` encodes.
fn ioctl<T>(fd: RawFd, request: u32, arg: &mut T) -> std::io::Result<()> {
    loop {
        // SAFETY: `fd` is an open device and `arg` is a live, exclusively
        // borrowed `T` whose size is the one encoded in `request`, so the
        // kernel reads and writes only inside it.
        let r = unsafe { libc::ioctl(fd, request as _, std::ptr::from_mut(arg)) };
        if r >= 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// An `_IO` request: no argument.
fn ioctl_none(fd: RawFd, request: u32) -> std::io::Result<()> {
    // SAFETY: `fd` is an open request fd and the request code takes no
    // argument, so the kernel touches no memory of ours.
    let r = unsafe { libc::ioctl(fd, request as _, 0) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Wait on one fd. True when any of `events` is ready. A signal resumes the wait for what is
/// left of `timeout`: read as a timeout, it would fail a request that is still running.
fn poll(fd: RawFd, events: libc::c_short, timeout: Duration) -> std::io::Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let r = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `pfd` is one live `pollfd` and the count passed is 1.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r >= 0 {
            break r;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
        if left.is_zero() {
            return Ok(false);
        }
    };
    // A queue that is not streaming polls as an error at once; without this
    // pause a caller's deadline loop would spin.
    if r > 0 && pfd.revents & events == 0 {
        std::thread::sleep(timeout.min(Duration::from_millis(2)));
    }
    Ok(pfd.revents & events != 0)
}

fn queue_type(queue: Queue) -> u32 {
    match queue {
        Queue::Output => uapi::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        Queue::Capture => uapi::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    }
}

/// `EAGAIN` and, on a drained CAPTURE queue, `EPIPE`: nothing to take.
fn nothing_ready(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EPIPE))
}

fn capture_format_of(fmt: &uapi::V4l2Format) -> CaptureFormat {
    // Copied out: the mplane struct is packed.
    let pix = fmt.pix_mp;
    let planes = pix.plane_fmt;
    CaptureFormat {
        fourcc: pix.pixelformat,
        width: pix.width,
        height: pix.height,
        stride: planes[0].bytesperline,
        planes: pix.num_planes,
    }
}

/// One mapped buffer.
struct Mapping {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is owned by exactly one node, which is used from one
// thread at a time; nothing else holds the pointer.
unsafe impl Send for Mapping {}

impl Mapping {
    fn bytes(&self) -> &[u8] {
        // SAFETY: a live mapping of `len` readable bytes owned by `self`,
        // borrowed for no longer than `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are a live `mmap` result this value owns; it is
        // unmapped exactly once, here.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// One dequeued buffer, as the kernel filled it in.
struct Done {
    index: u32,
    stamp: u64,
    flags: u32,
    bytesused: u32,
}

/// An open multi-planar memory-to-memory video node.
struct Video {
    fd: OwnedFd,
}

impl Video {
    fn open(path: &Path) -> std::io::Result<Video> {
        let fd: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?
            .into();
        let mut cap = uapi::V4l2Capability::default();
        ioctl(fd.as_raw_fd(), uapi::VIDIOC_QUERYCAP, &mut cap)?;
        let device = if cap.capabilities & uapi::V4L2_CAP_DEVICE_CAPS != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        let needed = uapi::V4L2_CAP_VIDEO_M2M_MPLANE | uapi::V4L2_CAP_STREAMING;
        if device & needed != needed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "not a multi-planar memory-to-memory node",
            ));
        }
        Ok(Video { fd })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn formats(&self, queue: Queue) -> std::io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut desc = uapi::V4l2Fmtdesc {
                index,
                type_: queue_type(queue),
                ..Default::default()
            };
            match ioctl(self.raw(), uapi::VIDIOC_ENUM_FMT, &mut desc) {
                Ok(()) => out.push(desc.pixelformat),
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn read_format(&self, queue: Queue) -> std::io::Result<uapi::V4l2Format> {
        let mut fmt = uapi::V4l2Format {
            type_: queue_type(queue),
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_G_FMT, &mut fmt)?;
        Ok(fmt)
    }

    fn set_output_format(
        &self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()> {
        let mut fmt = uapi::V4l2Format {
            type_: uapi::V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            ..Default::default()
        };
        fmt.pix_mp.width = width;
        fmt.pix_mp.height = height;
        fmt.pix_mp.pixelformat = fourcc;
        fmt.pix_mp.field = uapi::V4L2_FIELD_NONE;
        fmt.pix_mp.num_planes = 1;
        fmt.pix_mp.plane_fmt[0].sizeimage = buffer_size;
        ioctl(self.raw(), uapi::VIDIOC_S_FMT, &mut fmt)
    }

    fn capture_format(&self) -> std::io::Result<CaptureFormat> {
        Ok(capture_format_of(&self.read_format(Queue::Capture)?))
    }

    fn set_capture_format(&self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        let mut fmt = self.read_format(Queue::Capture)?;
        fmt.pix_mp.pixelformat = fourcc;
        ioctl(self.raw(), uapi::VIDIOC_S_FMT, &mut fmt)?;
        Ok(capture_format_of(&fmt))
    }

    fn request_buffers(&self, queue: Queue, count: u32) -> std::io::Result<u32> {
        let mut reqbufs = uapi::V4l2Requestbuffers {
            count,
            type_: queue_type(queue),
            memory: uapi::V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_REQBUFS, &mut reqbufs)?;
        Ok(reqbufs.count)
    }

    fn stream(&self, queue: Queue, on: bool) -> std::io::Result<()> {
        let mut kind = queue_type(queue) as i32;
        let request = if on {
            uapi::VIDIOC_STREAMON
        } else {
            uapi::VIDIOC_STREAMOFF
        };
        ioctl(self.raw(), request, &mut kind)
    }

    /// A `v4l2_buffer` for `queue` over `plane`, which must outlive the ioctl.
    fn buffer(queue: Queue, index: u32, plane: &mut uapi::V4l2Plane) -> uapi::V4l2Buffer {
        uapi::V4l2Buffer {
            index,
            type_: queue_type(queue),
            memory: uapi::V4L2_MEMORY_MMAP,
            m: std::ptr::from_mut(plane) as u64,
            length: 1,
            ..Default::default()
        }
    }

    /// Map buffer `index` of `queue`, writable for bitstream input.
    fn map(&self, queue: Queue, index: u32) -> std::io::Result<Mapping> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Video::buffer(queue, index, &mut plane);
        ioctl(self.raw(), uapi::VIDIOC_QUERYBUF, &mut buf)?;
        let len = plane.length as usize;
        let prot = match queue {
            Queue::Output => libc::PROT_READ | libc::PROT_WRITE,
            Queue::Capture => libc::PROT_READ,
        };
        // SAFETY: a fresh shared mapping of this fd at the offset and length
        // the driver just reported for the buffer; no existing memory is
        // named, and the result is checked before use.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                self.raw(),
                (plane.m as u32).into(),
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let ptr = std::ptr::NonNull::new(ptr.cast::<u8>())
            .ok_or_else(|| std::io::Error::other("mmap returned null"))?;
        Ok(Mapping { ptr, len })
    }

    fn export(&self, index: u32) -> std::io::Result<OwnedFd> {
        let mut exp = uapi::V4l2Exportbuffer {
            type_: uapi::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            index,
            flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
            ..Default::default()
        };
        ioctl(self.raw(), uapi::VIDIOC_EXPBUF, &mut exp)?;
        // SAFETY: a successful EXPBUF returns a new fd this process owns and
        // nothing else has seen.
        Ok(unsafe { OwnedFd::from_raw_fd(exp.fd) })
    }

    /// Copy `data` into the mapped OUTPUT buffer and queue it, in `request`
    /// when one is given.
    fn queue_input(
        &self,
        mapping: &Mapping,
        index: u32,
        data: &[u8],
        stamp: u64,
        request: Option<RawFd>,
    ) -> std::io::Result<()> {
        if data.len() > mapping.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "a {} byte access unit does not fit the {} byte input buffer",
                    data.len(),
                    mapping.len
                ),
            ));
        }
        // SAFETY: `mapping` is a live writable mapping of `mapping.len` bytes
        // and `data.len()` was just checked against it. The buffer is not
        // queued (callers pass only free indices), so the driver is not
        // reading it, and `data` cannot overlap a private mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapping.ptr.as_ptr(), data.len());
        }
        let mut plane = uapi::V4l2Plane {
            bytesused: data.len() as u32,
            ..Default::default()
        };
        let mut buf = Video::buffer(Queue::Output, index, &mut plane);
        buf.timestamp_sec = (stamp / 1_000_000) as i64;
        buf.timestamp_usec = (stamp % 1_000_000) as i64;
        if let Some(fd) = request {
            buf.flags = req::V4L2_BUF_FLAG_REQUEST_FD;
            buf.request_fd = fd;
        }
        ioctl(self.raw(), uapi::VIDIOC_QBUF, &mut buf)
    }

    fn queue_capture(&self, index: u32) -> std::io::Result<()> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Video::buffer(Queue::Capture, index, &mut plane);
        ioctl(self.raw(), uapi::VIDIOC_QBUF, &mut buf)
    }

    fn dequeue(&self, queue: Queue) -> std::io::Result<Option<Done>> {
        let mut plane = uapi::V4l2Plane::default();
        let mut buf = Video::buffer(queue, 0, &mut plane);
        match ioctl(self.raw(), uapi::VIDIOC_DQBUF, &mut buf) {
            Ok(()) => Ok(Some(Done {
                index: buf.index,
                stamp: (buf.timestamp_sec as u64) * 1_000_000 + buf.timestamp_usec as u64,
                flags: buf.flags,
                bytesused: plane.bytesused,
            })),
            Err(e) if nothing_ready(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// [`Self::dequeue`] for a buffer that is due: a completed request has
    /// both of its buffers done, give or take a wakeup.
    fn dequeue_due(&self, queue: Queue) -> std::io::Result<Option<Done>> {
        if let Some(done) = self.dequeue(queue)? {
            return Ok(Some(done));
        }
        let events = match queue {
            Queue::Output => libc::POLLOUT,
            Queue::Capture => libc::POLLIN,
        };
        poll(self.raw(), events, Duration::from_millis(50))?;
        self.dequeue(queue)
    }

    fn dequeue_capture(&self) -> std::io::Result<Option<Dequeued>> {
        Ok(self.dequeue(Queue::Capture)?.map(|d| Dequeued {
            index: d.index,
            stamp: d.stamp,
            error: d.flags & uapi::V4L2_BUF_FLAG_ERROR != 0,
            empty: d.bytesused == 0,
        }))
    }
}

/// A stateful decoder node.
pub struct Node {
    video: Video,
    inputs: Vec<Mapping>,
}

impl Node {
    /// A multi-planar memory-to-memory streaming node, or a refusal.
    pub fn open(path: &Path) -> std::io::Result<Node> {
        Ok(Node {
            video: Video::open(path)?,
            inputs: Vec::new(),
        })
    }

    /// The pixel formats `queue` takes, in the driver's order.
    pub fn formats(&self, queue: Queue) -> std::io::Result<Vec<u32>> {
        self.video.formats(queue)
    }

    /// Whether the driver has the control `id`.
    pub fn has_control(&self, id: u32) -> bool {
        query_control(&self.video, id).is_ok_and(|r| r.is_some())
    }

    /// Export CAPTURE buffer `index` as a dma-buf.
    pub fn export(&mut self, index: u32) -> std::io::Result<OwnedFd> {
        self.video.export(index)
    }
}

impl pf_v4l2dec::stateful::Device for Node {
    fn set_output_format(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()> {
        self.video
            .set_output_format(fourcc, width, height, buffer_size)
    }

    fn subscribe_source_change(&mut self) -> std::io::Result<()> {
        let mut sub = uapi::V4l2EventSubscription {
            type_: uapi::V4L2_EVENT_SOURCE_CHANGE,
            ..Default::default()
        };
        ioctl(self.video.raw(), uapi::VIDIOC_SUBSCRIBE_EVENT, &mut sub)
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        if queue == Queue::Output {
            self.inputs.clear();
        }
        let granted = self.video.request_buffers(queue, count)?;
        if queue == Queue::Output {
            for index in 0..granted {
                self.inputs.push(self.video.map(Queue::Output, index)?);
            }
        }
        Ok(granted)
    }

    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()> {
        self.video.stream(queue, on)
    }

    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()> {
        let mapping = self
            .inputs
            .get(index as usize)
            .ok_or_else(|| std::io::Error::other("no such input buffer"))?;
        self.video.queue_input(mapping, index, data, stamp, None)
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        Ok(self.video.dequeue(Queue::Output)?.map(|d| d.index))
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        self.video.queue_capture(index)
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        self.video.dequeue_capture()
    }

    fn dequeue_event(&mut self) -> std::io::Result<Option<Event>> {
        let mut ev = uapi::V4l2Event::default();
        match ioctl(self.video.raw(), uapi::VIDIOC_DQEVENT, &mut ev) {
            Ok(()) => {
                let resolution = ev.u[0] as u32 & uapi::V4L2_EVENT_SRC_CH_RESOLUTION != 0;
                Ok(Some(
                    if ev.type_ == uapi::V4L2_EVENT_SOURCE_CHANGE && resolution {
                        Event::SourceChange
                    } else {
                        Event::Other
                    },
                ))
            }
            // No event pending.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        self.video.capture_format()
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        self.video.formats(Queue::Capture)
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        self.video.set_capture_format(fourcc)
    }

    fn min_capture_buffers(&mut self) -> std::io::Result<u32> {
        let mut ctrl = uapi::V4l2Control {
            id: uapi::V4L2_CID_MIN_BUFFERS_FOR_CAPTURE,
            value: 0,
        };
        ioctl(self.video.raw(), uapi::VIDIOC_G_CTRL, &mut ctrl)?;
        Ok(ctrl.value.max(0) as u32)
    }

    fn wait(&mut self, interest: Interest, timeout: Duration) -> std::io::Result<()> {
        let events = match interest {
            Interest::Capture => libc::POLLIN | libc::POLLPRI,
            Interest::Output => libc::POLLOUT | libc::POLLPRI,
        };
        poll(self.video.raw(), events, timeout).map(|_| ())
    }
}

fn query_control(video: &Video, id: u32) -> std::io::Result<Option<ControlRange>> {
    let mut q = req::V4l2QueryExtCtrl {
        id,
        ..Default::default()
    };
    match ioctl(video.raw(), req::VIDIOC_QUERY_EXT_CTRL, &mut q) {
        Ok(()) => Ok(Some(ControlRange {
            minimum: q.minimum,
            maximum: q.maximum,
        })),
        // The driver has no such control.
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The media device that owns `video`'s requests: the one whose topology
/// lists the node's `major:minor` as a video interface.
fn media_device_of(video: &Video) -> std::io::Result<OwnedFd> {
    let rdev = std::fs::File::from(video.fd.try_clone()?)
        .metadata()?
        .rdev();
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    for n in 0..64 {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(format!("/dev/media{n}"))
        else {
            continue;
        };
        let media: OwnedFd = file.into();
        let mut topology = req::MediaV2Topology::default();
        if ioctl(media.as_raw_fd(), req::MEDIA_IOC_G_TOPOLOGY, &mut topology).is_err() {
            continue;
        }
        let count = topology.num_interfaces as usize;
        let mut interfaces = vec![req::MediaV2Interface::default(); count];
        let mut topology = req::MediaV2Topology {
            num_interfaces: count as u32,
            ptr_interfaces: interfaces.as_mut_ptr() as u64,
            ..Default::default()
        };
        if ioctl(media.as_raw_fd(), req::MEDIA_IOC_G_TOPOLOGY, &mut topology).is_err() {
            continue;
        }
        let ours = interfaces.iter().any(|i| {
            // Copied out: the struct is packed.
            let (kind, imajor, iminor) = (i.intf_type, i.major, i.minor);
            kind == req::MEDIA_INTF_T_V4L_VIDEO && (imajor, iminor) == (major, minor)
        });
        if ours {
            return Ok(media);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no media device lists this video node",
    ))
}

/// A stateless decoder: the video node, its media device, and one request.
pub struct RequestNode {
    video: Video,
    request: OwnedFd,
    /// Kept open: the request belongs to it.
    _media: OwnedFd,
    input: Option<Mapping>,
    pictures: Vec<Mapping>,
}

impl RequestNode {
    /// Open the node and allocate its request. Refuses a node no media
    /// device claims: without one there is nothing to attach controls to.
    pub fn open(path: &Path) -> std::io::Result<RequestNode> {
        let video = Video::open(path)?;
        let media = media_device_of(&video)?;
        let mut fd: i32 = -1;
        ioctl(media.as_raw_fd(), req::MEDIA_IOC_REQUEST_ALLOC, &mut fd)?;
        // SAFETY: a successful REQUEST_ALLOC returns a new fd this process
        // owns and nothing else has seen.
        let request = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(RequestNode {
            video,
            request,
            _media: media,
            input: None,
            pictures: Vec::new(),
        })
    }

    /// The pixel formats `queue` takes, in the driver's order.
    pub fn formats(&self, queue: Queue) -> std::io::Result<Vec<u32>> {
        self.video.formats(queue)
    }

    /// Export CAPTURE buffer `index` as a dma-buf.
    pub fn export(&mut self, index: u32) -> std::io::Result<OwnedFd> {
        self.video.export(index)
    }

    /// The bytes of CAPTURE buffer `index`. Meaningful for a buffer the
    /// decoder has returned: until it is queued again the driver only reads it.
    pub fn picture(&self, index: u32) -> Option<&[u8]> {
        Some(self.pictures.get(index as usize)?.bytes())
    }
}

impl pf_v4l2dec::stateless::Device for RequestNode {
    fn set_output_format(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()> {
        self.video
            .set_output_format(fourcc, width, height, buffer_size)
    }

    fn control(&mut self, id: u32) -> std::io::Result<Option<ControlRange>> {
        query_control(&self.video, id)
    }

    fn set_controls(&mut self, in_request: bool, controls: &[Control<'_>]) -> std::io::Result<()> {
        if controls.is_empty() {
            return Ok(());
        }
        fn payload<T: ?Sized>(id: u32, value: &T) -> req::V4l2ExtControl {
            req::V4l2ExtControl {
                id,
                size: size_of_val(value) as u32,
                reserved2: 0,
                value: std::ptr::from_ref(value).cast::<u8>() as u64,
            }
        }
        let mut list: Vec<req::V4l2ExtControl> = controls
            .iter()
            .map(|c| match *c {
                Control::Value { id, value } => req::V4l2ExtControl {
                    id,
                    size: 0,
                    reserved2: 0,
                    value: u64::from(value as u32),
                },
                Control::HevcSps(v) => payload(c.id(), v),
                Control::HevcPps(v) => payload(c.id(), v),
                Control::HevcDecodeParams(v) => payload(c.id(), v),
                Control::HevcSliceParams(v) => payload(c.id(), v),
                Control::HevcScalingMatrix(v) => payload(c.id(), v),
                Control::HevcEntryPoints(v) => payload(c.id(), v),
                Control::HevcStRps(v) => payload(c.id(), v),
                Control::HevcLtRps(v) => payload(c.id(), v),
            })
            .collect();
        let mut ext = req::V4l2ExtControls {
            which: if in_request {
                req::V4L2_CTRL_WHICH_REQUEST_VAL
            } else {
                0
            },
            count: list.len() as u32,
            request_fd: if in_request {
                self.request.as_raw_fd()
            } else {
                0
            },
            controls: list.as_mut_ptr() as u64,
            ..Default::default()
        };
        // The payload pointers name `controls`, which outlive this call; the
        // kernel copies them in before the ioctl returns.
        ioctl(self.video.raw(), req::VIDIOC_S_EXT_CTRLS, &mut ext).map_err(|e| {
            let failed = list.get(ext.error_idx as usize).map(|c| c.id);
            std::io::Error::new(e.kind(), format!("control {failed:#x?}: {e}"))
        })
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        self.video.capture_format()
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        self.video.formats(Queue::Capture)
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        self.video.set_capture_format(fourcc)
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        match queue {
            Queue::Output => self.input = None,
            Queue::Capture => self.pictures.clear(),
        }
        let granted = self.video.request_buffers(queue, count)?;
        match queue {
            Queue::Output if granted > 0 => {
                self.input = Some(self.video.map(Queue::Output, 0)?);
            }
            Queue::Output => {}
            Queue::Capture => {
                for index in 0..granted {
                    self.pictures.push(self.video.map(Queue::Capture, index)?);
                }
            }
        }
        Ok(granted)
    }

    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()> {
        self.video.stream(queue, on)
    }

    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()> {
        let mapping = self
            .input
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no input buffer"))?;
        self.video
            .queue_input(mapping, index, data, stamp, Some(self.request.as_raw_fd()))
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        self.video.queue_capture(index)
    }

    fn run_request(&mut self, timeout: Duration) -> std::io::Result<bool> {
        let fd = self.request.as_raw_fd();
        ioctl_none(fd, req::MEDIA_REQUEST_IOC_QUEUE)?;
        // A request signals completion as an exceptional condition.
        if !poll(fd, libc::POLLPRI, timeout)? {
            return Ok(false);
        }
        ioctl_none(fd, req::MEDIA_REQUEST_IOC_REINIT)?;
        Ok(true)
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        Ok(self.video.dequeue_due(Queue::Output)?.map(|d| d.index))
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        Ok(self.video.dequeue_due(Queue::Capture)?.map(|d| Dequeued {
            index: d.index,
            stamp: d.stamp,
            error: d.flags & uapi::V4L2_BUF_FLAG_ERROR != 0,
            empty: d.bytesused == 0,
        }))
    }
}

/// Against the kernel's reference stateful codec, `vicodec`:
/// `sudo modprobe vicodec multiplanar=1`, then
/// `PF_V4L2_VICODEC_ENC=/dev/videoA PF_V4L2_VICODEC_DEC=/dev/videoB cargo test
/// -p pf-v4l2 --lib -- --ignored`. The encoder node makes the stream the
/// decoder node is then driven with.
#[cfg(test)]
mod tests {
    use pf_v4l2dec::stateful::Stateful;

    use super::*;

    const FWHT: u32 = u32::from_le_bytes(*b"FWHT");

    /// Raw NV12 picture `n`: a moving gradient, so pictures differ.
    fn raw_frame(width: usize, height: usize, n: usize) -> Vec<u8> {
        let mut frame = vec![128u8; width * height * 3 / 2];
        for y in 0..height {
            for x in 0..width {
                frame[y * width + x] = (x / 4 + y / 4 + n * 8) as u8;
            }
        }
        frame
    }

    /// Wait for and take the next buffer of `queue`.
    fn take(video: &Video, queue: Queue) -> Done {
        for _ in 0..100 {
            if let Some(done) = video.dequeue_due(queue).expect("dequeue") {
                return done;
            }
        }
        panic!("the encoder returned no buffer");
    }

    /// Encode `frames` pictures with the vicodec encoder node.
    fn encode(path: &Path, width: u32, height: u32, frames: usize) -> Vec<Vec<u8>> {
        let enc = Video::open(path).expect("open the encoder");
        enc.set_output_format(uapi::V4L2_PIX_FMT_NV12, width, height, 0)
            .expect("set the raw format");
        let mut fmt = enc
            .read_format(Queue::Capture)
            .expect("read the coded format");
        fmt.pix_mp.pixelformat = FWHT;
        fmt.pix_mp.width = width;
        fmt.pix_mp.height = height;
        ioctl(enc.raw(), uapi::VIDIOC_S_FMT, &mut fmt).expect("set the coded format");
        assert!(enc.request_buffers(Queue::Output, 1).expect("raw buffers") >= 1);
        let coded_count = enc
            .request_buffers(Queue::Capture, 2)
            .expect("coded buffers");
        let input = enc.map(Queue::Output, 0).expect("map the raw buffer");
        let coded: Vec<Mapping> = (0..coded_count)
            .map(|i| enc.map(Queue::Capture, i).expect("map a coded buffer"))
            .collect();
        for index in 0..coded_count {
            enc.queue_capture(index).expect("queue a coded buffer");
        }
        enc.stream(Queue::Output, true).expect("start raw");
        enc.stream(Queue::Capture, true).expect("start coded");
        let mut out = Vec::with_capacity(frames);
        for n in 0..frames {
            let raw = raw_frame(width as usize, height as usize, n);
            enc.queue_input(&input, 0, &raw, n as u64, None)
                .expect("queue a raw picture");
            let done = take(&enc, Queue::Capture);
            out.push(coded[done.index as usize].bytes()[..done.bytesused as usize].to_vec());
            enc.queue_capture(done.index).expect("requeue");
            take(&enc, Queue::Output);
        }
        out
    }

    /// The bytes behind a dma-buf, read through its own mapping.
    fn read_dmabuf(fd: &OwnedFd, len: usize) -> Vec<u8> {
        // SAFETY: a fresh read-only shared mapping of `len` bytes of a dma-buf
        // this test owns; checked before use and unmapped below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "map the exported picture");
        // SAFETY: `ptr` is the live mapping of `len` bytes made above.
        let copy = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec();
        // SAFETY: the same mapping, unmapped once.
        unsafe { libc::munmap(ptr, len) };
        copy
    }

    #[test]
    #[ignore = "needs the kernel's vicodec codec: see the module comment"]
    fn a_kernel_stateful_decoder_runs_the_whole_flow() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let encoder = var("PF_V4L2_VICODEC_ENC");
        let decoder = var("PF_V4L2_VICODEC_DEC");
        let streams = [(640u32, 360u32, 12usize), (1280, 720, 6)];
        let coded: Vec<Vec<Vec<u8>>> = streams
            .iter()
            .map(|(w, h, n)| encode(Path::new(&encoder), *w, *h, *n))
            .collect();

        let node = Node::open(Path::new(&decoder)).expect("open the decoder");
        assert!(node
            .formats(Queue::Output)
            .expect("enumerate")
            .contains(&FWHT));
        let nv12 = uapi::V4L2_PIX_FMT_NV12;
        let mut d = Stateful::open(node, FWHT, 640, 360, &[nv12]).expect("start the decoder");
        let mut stamp = 0u64;
        for ((width, height, _), frames) in streams.iter().zip(&coded) {
            for (n, frame) in frames.iter().enumerate() {
                stamp += 1;
                d.submit(frame, stamp).expect("queue an access unit");
                let picture = d
                    .pump(Duration::from_millis(1000))
                    .expect("pump")
                    .unwrap_or_else(|| panic!("no picture for unit {stamp}"));
                assert_eq!(picture.stamp, stamp, "the stamp follows its picture");
                assert!(!picture.corrupt);
                let format = d.capture().expect("configured").format;
                // The coded size: the driver pads the visible one to its blocks.
                assert!(format.width >= *width && format.height >= *height);
                assert!(format.width < *width + 64 && format.height < *height + 64);
                assert_eq!((format.fourcc, format.planes), (nv12, 1));

                // The picture, through the dma-buf the presenter would import:
                // FWHT is lossy, so the luma is near the source, not equal.
                let fd = d.device().export(picture.index).expect("export");
                let luma = (format.stride * format.height) as usize;
                let pixels = read_dmabuf(&fd, luma * 3 / 2);
                let source = raw_frame(*width as usize, *height as usize, n);
                let mut error = 0u64;
                for y in 0..*height as usize {
                    for x in 0..*width as usize {
                        let got = pixels[y * format.stride as usize + x];
                        error += u64::from(got.abs_diff(source[y * *width as usize + x]));
                    }
                }
                let mean = error / u64::from(width * height);
                assert!(mean < 24, "unit {stamp}: mean luma error {mean}");
                d.release(picture.index, picture.generation)
                    .expect("return the buffer");
            }
        }
        // The second stream's size replaced the pool once.
        assert_eq!(d.capture().expect("configured").generation, 2);
    }
}
