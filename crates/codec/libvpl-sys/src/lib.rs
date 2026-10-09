//! Raw FFI bindings to the vendored Intel VPL C API (`vpl/mfx.h`) plus the
//! statically linked MIT dispatcher.
//!
//! Empty on targets other than Windows — see build.rs. The safe wrapper lives
//! with its consumer (`pf-encode`'s `enc/windows/qsv.rs`); this crate is
//! bindings only.

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
// Generated code — clippy findings in it (missing safety docs on generated unsafe fns, style
// nits across 14k lines) are bindgen's shape, not ours; the safe wrapper in pf-encode is the
// linted surface.
#![allow(clippy::all)]

#[cfg(target_os = "windows")]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// Link sanity: the static dispatcher resolves and its loader spins up
    /// without an Intel driver present (enumeration may find zero
    /// implementations — that's fine, MFXLoad itself must still succeed).
    #[test]
    fn dispatcher_links_and_loads() {
        // SAFETY: MFXLoad allocates the dispatcher's loader context (documented to work with no
        // driver present) and MFXUnload frees that same non-null handle; nothing else is touched.
        unsafe {
            let loader = MFXLoad();
            assert!(!loader.is_null(), "MFXLoad returned NULL");
            MFXUnload(loader);
        }
    }
}
