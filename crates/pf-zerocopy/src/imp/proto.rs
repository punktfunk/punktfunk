//! Request/reply vocabulary between the PipeWire capture thread and the isolated GPU-import
//! worker (`punktfunk-host zerocopy-worker`; `design/zerocopy-worker-isolation.md`).
//!
//! Transport is [`super::ipc`] (SEQPACKET + `SCM_RIGHTS`); this module versions the body
//! independently of other workers. Pixels never ride the socket — they move GPU-side via
//! CUDA IPC ([`super::cuda::ipc_export`]).
//!
//! Pin: `round_trip_both_directions`.

use serde::{Deserialize, Serialize};

/// Bumped on any wire change. Echoed in [`Reply::Ready`]; the host refuses a mismatch.
/// Same binary (`/proc/self/exe`) — trips only a stale re-exec.
pub const PROTO_VERSION: u32 = 4;

/// Mirrors the `EglImporter` entry points. Append-only: a worker can outlive a replaced host,
/// so an unknown variant must fail decode, not remap.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// Tiled dmabuf → EGL/GL de-tile blit → BGRx CUDA buffer.
    Tiled,
    /// Tiled dmabuf → EGL/GL NV12 convert → two-plane CUDA buffer (`PUNKTFUNK_NV12`).
    TiledNv12,
    /// LINEAR dmabuf → Vulkan bridge → BGRx CUDA buffer (gamescope's only offer).
    Linear,
    /// Tiled dmabuf → EGL/GL planar-YUV444 convert → one stacked 3-plane CUDA buffer.
    Tiled444,
    /// LINEAR dmabuf → Vulkan-bridge compute CSC → two-plane NV12 CUDA buffer (gamescope analogue
    /// of [`TiledNv12`](Self::TiledNv12)).
    LinearNv12,
}

impl ImportKind {
    /// The CUDA planes this import delivers.
    pub fn layout(self) -> super::cuda::PlaneLayout {
        use super::cuda::PlaneLayout;
        match self {
            ImportKind::Tiled | ImportKind::Linear => PlaneLayout::Packed32,
            ImportKind::TiledNv12 | ImportKind::LinearNv12 => PlaneLayout::Nv12,
            ImportKind::Tiled444 => PlaneLayout::Yuv444,
        }
    }

    /// Tiled imports de-tile through EGL/GL; LINEAR ones go through the Vulkan bridge.
    pub fn is_tiled(self) -> bool {
        matches!(
            self,
            ImportKind::Tiled | ImportKind::TiledNv12 | ImportKind::Tiled444
        )
    }
}

/// host → worker.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Request {
    /// EGL-importable DRM modifiers for `fourcc` (startup; advertised to PipeWire).
    Modifiers { fourcc: u32 },
    /// `key` is the dmabuf's `st_ino` (stable per object). The fd rides `SCM_RIGHTS` only on
    /// first sight of `key` (`has_fd`); the worker keeps the dup.
    Import {
        key: u64,
        kind: ImportKind,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        offset: u32,
        stride: u32,
        has_fd: bool,
    },
    /// Recycle buffer `id` into the worker pool. Fire-and-forget; any host thread.
    Release { id: u32 },
    /// Format renegotiation: drop cached per-`key` fds and Vulkan imports. Fire-and-forget.
    ClearCache,
    /// The host's NVENC input slot `id`, an OPAQUE_FD export of `size` bytes (the fd rides along).
    RegisterSlot { id: u32, size: u64 },
    /// The host rebuilt its ring: drop every slot import. No reply.
    ForgetSlots,
    /// A straight-alpha RGBA8 cursor bitmap of `len` bytes in the memfd that rides along.
    SetCursor {
        serial: u64,
        width: u32,
        height: u32,
        len: u32,
    },
    /// One fused pass of the dmabuf `key` into slot `slot` (fd rides along when `has_fd`).
    Convert {
        key: u64,
        has_fd: bool,
        src: ConvertSrc,
        slot: u32,
        out: ConvertOut,
        cursor: Option<CursorRect>,
    },
    /// The convert timeline as OPAQUE_FD; it rides back on [`Reply::Timeline`].
    ConvertTimeline,
}

/// A dmabuf as the fused convert reads it. `fd` is process-local and never on the wire: the
/// worker fills it from its cache. `plane1` is a producer NV12's chroma `(offset, stride)` in
/// the same fd; the worker copies such a frame into the slot instead of converting it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertSrc {
    #[serde(skip)]
    pub fd: i32,
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
    pub plane1: Option<(u32, u32)>,
}

/// The slot layout the pass writes: `mode` as `convert_img.comp` numbers it (0 ARGB, 1 NV12,
/// 2 YUV444 planar, 3 ARGB10, 4 ABGR10), row pitch in words, luma rows.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertOut {
    pub mode: u32,
    pub width: u32,
    pub height: u32,
    pub pitch_w: u32,
    pub plane_rows: u32,
}

/// Where the uploaded cursor lands, in source pixels.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// worker → host.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Reply {
    /// Once, after EGL + CUDA init.
    Ready {
        version: u32,
    },
    /// Init failed. Host falls back to CPU, as for in-process `EglImporter::new()`.
    InitErr {
        message: String,
    },
    Modifiers {
        modifiers: Vec<u64>,
    },
    /// Import done; GPU copy already synced worker-side. `desc` only the first time `id` is
    /// delivered — the host opens CUDA IPC handles then and caches the mapping.
    Frame {
        id: u32,
        desc: Option<BufferDesc>,
    },
    /// No cached fd for this `key` (evicted or caches diverged). Host forgets "already sent"
    /// and retries once with the fd.
    NeedFd,
    /// This import failed; the worker is still alive (e.g. `EGL_BAD_MATCH`).
    Err {
        message: String,
    },
    /// The request completed (slot registered, cursor stored).
    Done,
    /// The pass is submitted; it signals `value` on the convert timeline.
    Converted {
        value: u64,
    },
    /// The convert timeline's fd rides along.
    Timeline,
}

/// Sent once per pooled buffer; later frames cite it by `Frame.id`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BufferDesc {
    pub width: u32,
    pub height: u32,
    /// `cuIpcGetMemHandle` blob (Y or BGRx). Always 64 bytes.
    pub y_handle: Vec<u8>,
    pub y_pitch: usize,
    /// NV12 only: interleaved chroma `(handle, pitch)`.
    pub uv: Option<(Vec<u8>, usize)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imp::ipc;
    use std::os::fd::AsFd;

    /// Pins the message types. Framing (fds, EOF, timeouts, descriptor cap) lives in [`ipc`].
    #[test]
    fn round_trip_both_directions() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let req = Request::Import {
            key: 0xdead_beef_u64,
            kind: ImportKind::TiledNv12,
            width: 5120,
            height: 1440,
            fourcc: 0x3432_5258,
            modifier: Some(0x0300_0000_0000_1234),
            offset: 0,
            stride: 5120 * 4,
            has_fd: false,
        };
        ipc::send(a.as_fd(), &req, None).unwrap();
        let (got, fd) = ipc::recv::<Request>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, req);
        assert!(fd.is_none());

        // A producer NV12 rides with its chroma plane; the fd stays process-local.
        let planar = Request::Convert {
            key: 9,
            has_fd: false,
            src: ConvertSrc {
                fd: 0,
                fourcc: u32::from_le_bytes(*b"NV12"),
                modifier: 0,
                offset: 0,
                stride: 3840,
                width: 3840,
                height: 2160,
                plane1: Some((3840 * 2160, 3840)),
            },
            slot: 2,
            out: ConvertOut {
                mode: 1,
                width: 3840,
                height: 2160,
                pitch_w: 960,
                plane_rows: 2160,
            },
            cursor: None,
        };
        ipc::send(a.as_fd(), &planar, None).unwrap();
        let (got, _) = ipc::recv::<Request>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, planar);

        let reply = Reply::Frame {
            id: 7,
            desc: Some(BufferDesc {
                width: 5120,
                height: 1440,
                y_handle: vec![1u8; 64],
                y_pitch: 5632,
                uv: Some((vec![2u8; 64], 5632)),
            }),
        };
        ipc::send(b.as_fd(), &reply, None).unwrap();
        let (got, fd) = ipc::recv::<Reply>(a.as_fd(), &mut buf).unwrap();
        assert_eq!(got, reply);
        assert!(fd.is_none());
    }
}
