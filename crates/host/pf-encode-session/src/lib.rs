//! One Windows encode session: frames from a [`drive::FrameSource`] through an encoder backend
//! into the host's AU section. The pf-vdisplay driver runs it under its swap chain and the
//! capture worker under Windows Graphics Capture, so the loop, its back-pressure and its pacing
//! exist once.
//!
//! [`targets`] holds the per-slot encoder inputs and the one GPU pass that fills them, [`open`]
//! walks a `SET_ENCODE` backend list, [`section`] writes the AU section and [`drive`] is the
//! loop. Each side supplies what differs: where frames come from, the thread the loop runs on,
//! and how the section got mapped.
//! Evidence: `design/windows-wgc-capture.md` §4.5, `design/windows-video-plane-overhaul.md` §2.

#![cfg(target_os = "windows")]

pub mod drive;
pub mod open;
pub mod section;
pub mod targets;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// A session failure: a small code for the `SET_ENCODE` reply and a stage tag. The log has
/// the rest.
pub type Fail = (i32, &'static str);

/// Lock `m` whether or not a holder panicked: every value guarded here stays consistent across
/// an unwind, and an encode thread must not die of someone else's panic.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
