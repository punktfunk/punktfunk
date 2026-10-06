//! The capture worker's control channel: the host and one `punktfunk-capture-worker` over a
//! pair of pipes, host → worker and worker → host.
//!
//! The worker is a second place an encode session runs, so the two encode messages carry the
//! driver's own structs ([`SetEncodeRequest`](crate::encode::SetEncodeRequest),
//! [`EncodeCtlRequest`](crate::encode::EncodeCtlRequest)) with nothing added: the host's proxy
//! does not know which one it drives. What is new is the source — which monitor the worker
//! captures and what it tells the host about it.
//!
//! Every message is a [`Frame`] and a body of `len` bytes. A request carries a `seq` its reply
//! echoes; an event carries `0`. Host and worker ship in one installer, so there is no
//! compatibility matrix: [`Hello`] is compared for equality and nothing is prefix-compatible.
//! The worker runs as the signed-in user and the host as SYSTEM, so the host treats every
//! byte here as input. Evidence: `design/windows-wgc-capture.md` §4.3.

use alloc::string::String;
use alloc::vec::Vec;
use bytemuck::{Pod, Zeroable};

/// `"PFCW"`, first in every [`Frame`]: a stray write on the pipe is refused, not parsed.
pub const MAGIC: u32 = u32::from_le_bytes(*b"PFCW");

/// Bumped on any change to a body here. Not a floor: [`Hello`] must match exactly.
pub const WORKER_PROTOCOL: u32 = 1;

/// Largest body either side accepts. The longest message is `SET_ENCODE` at 144 bytes; a
/// length past this is a corrupt or hostile stream and ends the channel.
pub const MAX_BODY: u32 = 1024;

/// A reply's kind is its request's with this bit set.
pub const REPLY: u32 = 0x8000_0000;

/// Message kinds. Requests are host → worker, events worker → host.
pub mod kind {
    /// [`Hello`](super::Hello), both ways, first on each pipe.
    pub const HELLO: u32 = 1;
    /// [`OpenSource`](super::OpenSource) → [`SourceReply`](super::SourceReply). Replaces any
    /// source the worker already has.
    pub const OPEN_SOURCE: u32 = 2;
    /// `SetEncodeRequest` → `SetEncodeReply`. The handles are values in the worker's table.
    pub const SET_ENCODE: u32 = 3;
    /// `EncodeCtlRequest` → [`CtlReply`](super::CtlReply).
    pub const ENCODE_CTL: u32 = 4;
    /// Event: [`SourceChanged`](super::SourceChanged), after the worker rebuilt its capture.
    pub const SOURCE_CHANGED: u32 = 0x100;
    /// Event: [`SourceGone`](super::SourceGone).
    pub const SOURCE_GONE: u32 = 0x101;
}

/// The header before every body.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct Frame {
    pub magic: u32,
    /// A [`kind`], with [`REPLY`] set on a reply.
    pub kind: u32,
    /// The request this answers; `0` on an event.
    pub seq: u32,
    /// Body bytes that follow, at most [`MAX_BODY`].
    pub len: u32,
}

/// Why a header was refused. Either ends the channel: there is no resynchronising a pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Magic(u32),
    TooLong(u32),
}

/// Both sides' first message: the protocol number and the build that speaks it.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    pub _pad: u32,
    /// The sender's version string, NUL-padded. Host and worker are one build.
    pub build: [u8; 32],
}

impl Hello {
    /// This build's hello. A `version` past 32 bytes is cut; both sides cut alike.
    pub fn new(version: &str) -> Self {
        let mut build = [0u8; 32];
        let n = version.len().min(32);
        build[..n].copy_from_slice(&version.as_bytes()[..n]);
        Self {
            protocol: WORKER_PROTOCOL,
            _pad: 0,
            build,
        }
    }
}

/// [`OpenSource::flags`]: capture `R16G16B16A16Float` (an HDR or wide-colour session).
pub const OPEN_FP16: u32 = 1 << 0;
/// [`OpenSource::flags`]: the capture draws the pointer into the picture.
pub const OPEN_CURSOR: u32 = 1 << 1;

/// Which monitor to capture, and how. The name is the host's, resolved from the target a moment
/// ago: a name that no longer matches an output is [`SOURCE_NOT_FOUND`], and the host resolves
/// again.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct OpenSource {
    /// `OPEN_*` bits; unknown bits are refused.
    pub flags: u32,
    /// The stream's frame period in 100 ns units. The worker asks the capture for half of it.
    pub frame_interval_100ns: u32,
    /// GDI device name (`\\.\DISPLAY1`), UTF-16, NUL-padded: `CCHDEVICENAME` is 32.
    pub gdi_name: [u16; 32],
}

/// [`SourceReply::status`]: the capture is running.
pub const SOURCE_OK: u32 = 0;
/// No output carries that name or key.
pub const SOURCE_NOT_FOUND: u32 = 1;
/// The capture did not open; `error` and `stage` say where.
pub const SOURCE_OPEN_FAILED: u32 = 2;
/// The request itself was malformed.
pub const SOURCE_BAD_REQUEST: u32 = 3;

/// [`SourceReply::format`] and [`SourceChanged::format`].
pub const FORMAT_BGRA8: u32 = 1;
pub const FORMAT_FP16: u32 = 2;

/// What the worker opened. Everything past `stage` is zero unless `status` is [`SOURCE_OK`].
/// Refresh and HDR state are not here: the host's own display inventory has them.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SourceReply {
    pub status: u32,
    /// The failing call's HRESULT.
    pub error: i32,
    /// NUL-padded tag of the stage that failed.
    pub stage: [u8; 16],
    /// The size of the frames, which is the size an encoder must open at.
    pub width: u32,
    pub height: u32,
    /// `FORMAT_*`: what the frames are, which is what was asked for.
    pub format: u32,
}

/// The answer to `ENCODE_CTL`: `0`, or the reason nothing was done.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct CtlReply {
    pub status: u32,
}

/// [`CtlReply::status`]: done.
pub const CTL_OK: u32 = 0;
/// There is no session, or no source, for this to act on.
pub const CTL_NOT_FOUND: u32 = 1;
/// The op is unknown or its arguments are out of range.
pub const CTL_INVALID: u32 = 2;
/// The op ran and failed.
pub const CTL_FAILED: u32 = 3;

/// The source's size or format moved and the capture follows it. The session opened for the
/// old one is stale: the host answers with a new `SET_ENCODE`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SourceChanged {
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

/// [`SourceGone::reason`]: the monitor the capture was opened on no longer exists. One that
/// re-arrives is a new monitor, even under its old name.
pub const GONE_MONITOR: u32 = 1;

/// The source is no longer what was opened. The capture API does not say so (its frames just
/// thin out or stop), so the worker watches the monitor itself. The host answers by resolving
/// the display again and opening a new capture.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SourceGone {
    pub reason: u32,
}

// Both ends read these as raw bytes off a pipe.
const _: () = {
    use core::mem::size_of;

    assert!(size_of::<Frame>() == 16);
    assert!(size_of::<Hello>() == 40);
    assert!(size_of::<OpenSource>() == 72);
    assert!(size_of::<SourceReply>() == 36);
    assert!(size_of::<CtlReply>() == 4);
    assert!(size_of::<SourceChanged>() == 12);
    assert!(size_of::<SourceGone>() == 4);
    assert!(size_of::<crate::encode::SetEncodeRequest>() <= MAX_BODY as usize);
};

/// One message as the bytes to write: the header, then `body`.
pub fn encode<T: Pod>(kind: u32, seq: u32, body: &T) -> Vec<u8> {
    let body = bytemuck::bytes_of(body);
    let frame = Frame {
        magic: MAGIC,
        kind,
        seq,
        len: body.len() as u32,
    };
    let mut out = Vec::with_capacity(size_of::<Frame>() + body.len());
    out.extend_from_slice(bytemuck::bytes_of(&frame));
    out.extend_from_slice(body);
    out
}

/// The header in `bytes`, checked: the reader then knows exactly how many body bytes follow.
pub fn header(bytes: &[u8; 16]) -> Result<Frame, FrameError> {
    let frame: Frame = bytemuck::pod_read_unaligned(bytes);
    if frame.magic != MAGIC {
        return Err(FrameError::Magic(frame.magic));
    }
    if frame.len > MAX_BODY {
        return Err(FrameError::TooLong(frame.len));
    }
    Ok(frame)
}

/// `bytes` as a `T`. `None` unless it is exactly a `T`: a short or long body is never padded
/// or cut into one.
pub fn body<T: Pod>(bytes: &[u8]) -> Option<T> {
    (bytes.len() == size_of::<T>()).then(|| bytemuck::pod_read_unaligned(bytes))
}

/// `name` as [`OpenSource::gdi_name`]. `None` when it is empty or does not fit with its NUL.
pub fn gdi_name(name: &str) -> Option<[u16; 32]> {
    let mut out = [0u16; 32];
    let mut n = 0;
    for unit in name.encode_utf16() {
        if n == out.len() - 1 {
            return None;
        }
        out[n] = unit;
        n += 1;
    }
    (n > 0).then_some(out)
}

/// [`OpenSource::gdi_name`] as text, up to its first NUL.
pub fn gdi_name_text(name: &[u16; 32]) -> String {
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{EncodeCtlRequest, SetEncodeRequest};

    fn split(bytes: &[u8]) -> (Frame, &[u8]) {
        let head: &[u8; 16] = bytes[..16].try_into().unwrap();
        (header(head).expect("header"), &bytes[16..])
    }

    #[test]
    fn a_message_round_trips_through_its_bytes() {
        let open = OpenSource {
            flags: OPEN_FP16 | OPEN_CURSOR,
            frame_interval_100ns: 83_333,
            gdi_name: gdi_name(r"\\.\DISPLAY10").unwrap(),
        };
        let bytes = encode(kind::OPEN_SOURCE, 7, &open);
        let (frame, rest) = split(&bytes);
        assert_eq!(
            (frame.kind, frame.seq, frame.len),
            (kind::OPEN_SOURCE, 7, 72)
        );
        assert_eq!(body::<OpenSource>(rest), Some(open));
        assert_eq!(gdi_name_text(&open.gdi_name), r"\\.\DISPLAY10");
    }

    /// The two encode messages are the driver's structs, byte for byte: that is what lets one
    /// host proxy drive either.
    #[test]
    fn the_encode_messages_are_the_drivers_own() {
        let req: SetEncodeRequest = Zeroable::zeroed();
        let bytes = encode(kind::SET_ENCODE, 1, &req);
        let (frame, rest) = split(&bytes);
        assert_eq!(frame.len as usize, size_of::<SetEncodeRequest>());
        assert_eq!(body::<SetEncodeRequest>(rest), Some(req));
        let ctl: EncodeCtlRequest = Zeroable::zeroed();
        assert_eq!(
            split(&encode(kind::ENCODE_CTL, 2, &ctl)).0.len as usize,
            size_of::<EncodeCtlRequest>()
        );
    }

    #[test]
    fn a_reply_is_its_request_with_the_reply_bit() {
        let bytes = encode(kind::ENCODE_CTL | REPLY, 9, &CtlReply { status: CTL_OK });
        let (frame, rest) = split(&bytes);
        assert_eq!(frame.kind & !REPLY, kind::ENCODE_CTL);
        assert_ne!(frame.kind & REPLY, 0);
        assert_eq!(body::<CtlReply>(rest), Some(CtlReply { status: CTL_OK }));
    }

    #[test]
    fn a_foreign_or_oversized_header_ends_the_channel() {
        let mut bytes = encode(kind::HELLO, 0, &Hello::new("0.43.1"));
        bytes[0] ^= 0xFF;
        let head: [u8; 16] = bytes[..16].try_into().unwrap();
        assert!(matches!(header(&head), Err(FrameError::Magic(_))));

        let long = Frame {
            magic: MAGIC,
            kind: kind::HELLO,
            seq: 0,
            len: MAX_BODY + 1,
        };
        let head: [u8; 16] = bytemuck::bytes_of(&long).try_into().unwrap();
        assert_eq!(header(&head), Err(FrameError::TooLong(MAX_BODY + 1)));
    }

    #[test]
    fn a_body_of_the_wrong_size_is_not_a_message() {
        let bytes = encode(
            kind::SOURCE_GONE,
            0,
            &SourceGone {
                reason: GONE_MONITOR,
            },
        );
        assert_eq!(body::<SourceChanged>(&bytes[16..]), None);
        assert_eq!(
            body::<SourceGone>(&bytes[16..20]).map(|g| g.reason),
            Some(GONE_MONITOR)
        );
        assert_eq!(body::<SourceGone>(&bytes[16..19]), None);
    }

    #[test]
    fn hellos_match_only_for_the_same_build() {
        assert_eq!(Hello::new("0.43.1"), Hello::new("0.43.1"));
        assert_ne!(Hello::new("0.43.1"), Hello::new("0.43.2"));
        let long = "x".repeat(40);
        assert_eq!(Hello::new(&long).build, [b'x'; 32]);
    }

    #[test]
    fn a_device_name_needs_room_for_its_terminator() {
        assert!(gdi_name("").is_none());
        assert!(gdi_name(&"D".repeat(31)).is_some());
        assert!(gdi_name(&"D".repeat(32)).is_none());
    }
}
