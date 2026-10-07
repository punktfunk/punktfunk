//! Raw FFI bindings to the vendored PyroWave C API (`pyrowave.h`).
//!
//! Empty outside Linux/Windows/64-bit Android — see build.rs. The safe wrapper
//! lives with its consumer (`punktfunk-host`'s encoder backend, and later the
//! Rust clients' decoder backend); this crate is bindings only.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
// Bindgen output for a C API: u128 layout warnings and the like are upstream's concern.
#![allow(improper_ctypes)]
// The workspace-wide undocumented_unsafe_blocks deny cannot apply to GENERATED code: bindgen
// emits `unsafe {}` in layout tests/accessors and nobody hand-writes proofs into OUT_DIR. This
// crate is bindings-only by charter (the safe wrapper lives with the consumer), so the allow is
// crate-wide; the hand-written link-sanity test below still carries its proof by convention.
#![allow(clippy::undocumented_unsafe_blocks)]

#[cfg(any(
    target_os = "linux",
    target_os = "windows",
    all(target_os = "android", target_pointer_width = "64")
))]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(all(
    test,
    any(
        target_os = "linux",
        target_os = "windows",
        all(target_os = "android", target_pointer_width = "64")
    )
))]
mod tests {
    use super::*;

    /// Link sanity: statically linked archives resolve and the vendored pin is
    /// the API version we designed against. No GPU or Vulkan loader required —
    /// the version query touches no device state.
    #[test]
    fn api_version_matches_vendored_pin() {
        let (mut major, mut minor, mut patch) = (0u32, 0u32, 0u32);
        // SAFETY: the version query writes three u32s through live local out-pointers and
        // touches no device or global state.
        unsafe { pyrowave_get_api_version(&mut major, &mut minor, &mut patch) };
        assert_eq!((major, minor, patch), (0, 6, 0), "vendored pyrowave API version moved — re-check the §4.2 protocol coupling before bumping");
    }
}
