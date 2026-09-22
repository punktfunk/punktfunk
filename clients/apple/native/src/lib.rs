//! The Apple client's one static library: punktfunk-core's C ABI, linked whole, plus the
//! console's (`punktfunk_console_*`). Swift imports both as `PunktfunkCore`.

pub use punktfunk_core;

#[cfg(target_vendor = "apple")]
mod console;
