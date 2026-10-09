//! V4L2 video decode for the Linux client, the half that needs no device.
//!
//! [`uapi`] and [`uapi_stateless`] hand-declare the kernel structs, ioctl
//! numbers, and fourccs the decode rungs use; sizes are pinned by compile-time
//! assertions and the request codes by tests against the kernel's published
//! values. The layouts are the 64-bit little-endian ABI, which is every target
//! the rungs build for.
//!
//! [`stateful`] is the flow of a decoder that parses the stream itself —
//! formats, queues, the source-change renegotiation. [`stateless`] is the flow
//! of one that does not: [`hevc`] turns a `pf-bitstream` plan into its
//! controls, and the client keeps the decoded picture buffer. [`sand`] unpacks
//! the Raspberry Pi's column-tiled pictures. Each flow is written against a
//! `Device` trait whose ioctl implementation lives in `pf-v4l2`; the fakes in
//! `testing` (feature `testing`) are the only decoders most machines have.
//!
//! No `unsafe` here: nothing in this crate opens, maps, or calls a device.

#![forbid(unsafe_code)]

pub mod hevc;
pub mod sand;
pub mod stateful;
pub mod stateless;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod uapi;
pub mod uapi_stateless;
