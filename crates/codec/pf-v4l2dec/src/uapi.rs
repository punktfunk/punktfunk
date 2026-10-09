//! `videodev2.h`, the part a decoder client touches, for the 64-bit
//! little-endian ABI (x86_64, aarch64).
//!
//! Unions are declared as their widest integer member: `m` in [`V4l2Plane`]
//! and [`V4l2Buffer`] is an `unsigned long`, so an offset or an fd is its low
//! 32 bits and a pointer is the whole word. Struct sizes are asserted below;
//! a wrong size changes the ioctl number, and the kernel answers `ENOTTY`.

/// `_IOC` direction bits.
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// `_IOC(dir, 'V', nr, size)`.
const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

pub const V4L2_PIX_FMT_H264: u32 = fourcc(b'H', b'2', b'6', b'4');
pub const V4L2_PIX_FMT_HEVC: u32 = fourcc(b'H', b'E', b'V', b'C');
pub const V4L2_PIX_FMT_AV1: u32 = fourcc(b'A', b'V', b'0', b'1');
/// Linear two-plane 4:2:0 in one memory plane. Same code as `DRM_FORMAT_NV12`.
pub const V4L2_PIX_FMT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
/// Linear 10-bit in 16-bit words. Same code as `DRM_FORMAT_P010`.
pub const V4L2_PIX_FMT_P010: u32 = fourcc(b'P', b'0', b'1', b'0');

pub const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub const V4L2_MEMORY_MMAP: u32 = 1;
pub const V4L2_FIELD_NONE: u32 = 1;

pub const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
pub const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
pub const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

pub const V4L2_FMT_FLAG_COMPRESSED: u32 = 0x0001;

pub const V4L2_BUF_FLAG_ERROR: u32 = 0x0000_0040;
pub const V4L2_BUF_FLAG_LAST: u32 = 0x0010_0000;

pub const V4L2_EVENT_SOURCE_CHANGE: u32 = 5;
pub const V4L2_EVENT_SRC_CH_RESOLUTION: u32 = 1;

pub const V4L2_DEC_CMD_START: u32 = 0;

/// `V4L2_CID_BASE + 39`: capture buffers the decoder needs before it can run.
pub const V4L2_CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0927;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Fmtdesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2PlanePixFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [V4l2PlanePixFormat; 8],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// `struct v4l2_format` with the multi-planar arm of its 200-byte union. The
/// union holds a pointer in another arm, so it starts at offset 8.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Format {
    pub type_: u32,
    pub pad: u32,
    pub pix_mp: V4l2PixFormatMplane,
    pub tail: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Requestbuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Plane {
    pub bytesused: u32,
    pub length: u32,
    /// `mem_offset` for MMAP buffers, in the low 32 bits.
    pub m: u64,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub pad: u32,
    pub timestamp_sec: i64,
    pub timestamp_usec: i64,
    pub timecode: V4l2Timecode,
    pub sequence: u32,
    pub memory: u32,
    /// Multi-planar: the address of a [`V4l2Plane`] array of `length` entries.
    pub m: u64,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Exportbuffer {
    pub type_: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2EventSubscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Event {
    pub type_: u32,
    pub pad: u32,
    /// The payload union. `src_change.changes` is the low 32 bits of `u[0]`.
    pub u: [u64; 8],
    pub pending: u32,
    pub sequence: u32,
    pub timestamp_sec: i64,
    pub timestamp_nsec: i64,
    pub id: u32,
    pub reserved: [u32; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2DecoderCmd {
    pub cmd: u32,
    pub flags: u32,
    pub raw: [u64; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2Control {
    pub id: u32,
    pub value: i32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<V4l2Capability>() == 104);
    assert!(size_of::<V4l2Fmtdesc>() == 64);
    assert!(size_of::<V4l2PlanePixFormat>() == 20);
    assert!(size_of::<V4l2PixFormatMplane>() == 192);
    assert!(size_of::<V4l2Format>() == 208);
    assert!(size_of::<V4l2Requestbuffers>() == 20);
    assert!(size_of::<V4l2Plane>() == 64);
    assert!(size_of::<V4l2Buffer>() == 88);
    assert!(size_of::<V4l2Exportbuffer>() == 64);
    assert!(size_of::<V4l2EventSubscription>() == 32);
    assert!(size_of::<V4l2Event>() == 136);
    assert!(size_of::<V4l2DecoderCmd>() == 72);
    assert!(size_of::<V4l2Control>() == 8);
    // Offsets the unions and the padding decide, as `videodev2.h` lays them out.
    assert!(std::mem::offset_of!(V4l2Format, pix_mp) == 8);
    assert!(std::mem::offset_of!(V4l2PixFormatMplane, num_planes) == 180);
    assert!(std::mem::offset_of!(V4l2Plane, m) == 8);
    assert!(std::mem::offset_of!(V4l2Buffer, timestamp_sec) == 24);
    assert!(std::mem::offset_of!(V4l2Buffer, m) == 64);
    assert!(std::mem::offset_of!(V4l2Buffer, length) == 72);
    assert!(std::mem::offset_of!(V4l2Event, u) == 8);
    assert!(std::mem::offset_of!(V4l2Event, pending) == 72);
};

pub const VIDIOC_QUERYCAP: u32 = ioc(IOC_READ, 0, size_of::<V4l2Capability>());
pub const VIDIOC_ENUM_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<V4l2Fmtdesc>());
pub const VIDIOC_G_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 4, size_of::<V4l2Format>());
pub const VIDIOC_S_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 5, size_of::<V4l2Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(IOC_READ | IOC_WRITE, 8, size_of::<V4l2Requestbuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(IOC_READ | IOC_WRITE, 9, size_of::<V4l2Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(IOC_READ | IOC_WRITE, 15, size_of::<V4l2Buffer>());
pub const VIDIOC_EXPBUF: u32 = ioc(IOC_READ | IOC_WRITE, 16, size_of::<V4l2Exportbuffer>());
pub const VIDIOC_DQBUF: u32 = ioc(IOC_READ | IOC_WRITE, 17, size_of::<V4l2Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(IOC_WRITE, 18, size_of::<i32>());
pub const VIDIOC_STREAMOFF: u32 = ioc(IOC_WRITE, 19, size_of::<i32>());
pub const VIDIOC_G_CTRL: u32 = ioc(IOC_READ | IOC_WRITE, 27, size_of::<V4l2Control>());
pub const VIDIOC_DQEVENT: u32 = ioc(IOC_READ, 89, size_of::<V4l2Event>());
pub const VIDIOC_SUBSCRIBE_EVENT: u32 = ioc(IOC_WRITE, 90, size_of::<V4l2EventSubscription>());
pub const VIDIOC_DECODER_CMD: u32 = ioc(IOC_READ | IOC_WRITE, 96, size_of::<V4l2DecoderCmd>());

/// Unused direction, kept so the three `_IOC` directions read as one set.
const _: u32 = IOC_NONE;

#[cfg(test)]
mod tests {
    use super::*;

    /// The values `strace` and the kernel headers print on x86_64 and aarch64.
    #[test]
    fn request_codes_match_the_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xC040_5602);
        assert_eq!(VIDIOC_G_FMT, 0xC0D0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xC014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xC058_5609);
        assert_eq!(VIDIOC_QBUF, 0xC058_560F);
        assert_eq!(VIDIOC_EXPBUF, 0xC040_5610);
        assert_eq!(VIDIOC_DQBUF, 0xC058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_G_CTRL, 0xC008_561B);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565A);
        assert_eq!(VIDIOC_DECODER_CMD, 0xC048_5660);
    }

    #[test]
    fn fourccs_are_the_drm_codes_the_presenter_imports() {
        assert_eq!(V4L2_PIX_FMT_NV12, 0x3231_564e);
        assert_eq!(V4L2_PIX_FMT_P010, 0x3031_3050);
    }
}
