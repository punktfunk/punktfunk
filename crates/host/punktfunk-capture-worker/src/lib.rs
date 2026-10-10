//! The Windows capture worker: Windows Graphics Capture and the encoder for one session that
//! streams a monitor the host did not create.
//!
//! Windows Graphics Capture does not activate for SYSTEM, so the host starts this process with
//! the signed-in user's token and drives it over two pipes ([`pf_driver_proto::worker`]). It
//! captures one monitor ([`source`]), encodes with the driver's own session loop
//! (`pf-encode-session`) fed from [`pool`], and publishes into the AU section the host created,
//! so the host reads a worker's session exactly as it reads the driver's.
//!
//! The process is the unit of recovery. It owns no display, changes no topology and keeps
//! nothing the host cannot rebuild by starting another one; it exits when the host closes its
//! pipe, and ends itself rather than run on with an encode thread that will not stop.
//! Evidence: `design/windows-wgc-capture.md` §4.

#![cfg(target_os = "windows")]

mod control;
mod pool;
mod session;
mod source;
mod view;

use std::fs::File;
use std::os::windows::io::{FromRawHandle, RawHandle};

use anyhow::{Context, Result};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Exit code of a worker that ended itself over an encode thread that would not stop.
pub const EXIT_WEDGED: i32 = 3;

/// `--control <read>,<write>`: the two pipe ends the host made inheritable for this process,
/// as handle values.
fn pipes(args: &[String]) -> Result<(File, File)> {
    let values = args
        .iter()
        .position(|a| a == "--control")
        .and_then(|i| args.get(i + 1))
        .context("usage: punktfunk-capture-worker --control <read>,<write>")?;
    let (rx, tx) = values
        .split_once(',')
        .context("--control takes two handles")?;
    let handle = |v: &str| -> Result<File> {
        let value: usize = v.parse().context("--control handle value")?;
        // SAFETY: the host created this pipe end for this process and passed its value here;
        // nothing else in this process knows it, so the `File` is its only owner.
        Ok(unsafe { File::from_raw_handle(value as RawHandle) })
    };
    Ok((handle(rx)?, handle(tx)?))
}

/// Serve one host until it closes the control pipe.
pub fn run(args: &[String]) -> Result<()> {
    let (rx, tx) = pipes(args)?;
    // SAFETY: both calls take scalars and report only through their return value. The
    // awareness makes the capture item and the output agree on pixels.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    control::serve(rx, tx)
}
