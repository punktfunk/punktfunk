//! Poll helpers over `/proc/bus/input/devices` and `/sys/bus/hid/devices` for the pad
//! backends' `#[ignore]`d device tests. Kernel bind and unbind are asynchronous, so a
//! test reads the same surface it waits on.

use std::fs::DirEntry;
use std::time::{Duration, Instant};

pub fn input_devices() -> String {
    std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default()
}

/// The named evdev is gone, or `timeout` elapsed with it still listed.
pub fn wait_input_gone(needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !input_devices().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !input_devices().contains(needle)
}

/// First `/sys/bus/hid/devices` entry whose name carries `needle`, e.g. `:28DE:1302`.
pub fn hid_entry(needle: &str) -> Option<DirEntry> {
    std::fs::read_dir("/sys/bus/hid/devices")
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains(needle))
}

/// The HID device is gone, or `timeout` elapsed with it still bound.
pub fn wait_hid_gone(needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if hid_entry(needle).is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    hid_entry(needle).is_none()
}
