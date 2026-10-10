//! C wrapper for [`ReanchorGate`]. Time stays inside (`Instant::now`).
//! `arm` on loss, `on_decoded` per frame, `on_no_output` per empty AU, `poll` each tick.

use crate::*;
use punktfunk_core::reanchor::{AuAdmission, DecoderClass, GateVerdict, ReanchorGate};

/// Create a re-anchor gate seeded with the session's current `frames_dropped` (so
/// the first [`punktfunk_reanchor_gate_poll`] doesn't read the baseline as a loss).
/// Free with [`punktfunk_reanchor_gate_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_reanchor_gate_new(frames_dropped: u64) -> *mut ReanchorGate {
    Box::into_raw(Box::new(ReanchorGate::new(frames_dropped)))
}

/// Free a gate created by [`punktfunk_reanchor_gate_new`]. NULL is a no-op.
///
/// # Safety
/// `g` was returned by [`punktfunk_reanchor_gate_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_free(g: *mut ReanchorGate) {
    guard_void(|| {
        if !g.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(g) });
        }
    });
}

/// Arm the freeze: a loss was detected (frame-index gap, or decoder wedge/demotion).
/// Zeroes the recovery-mark count and (re-)sets the backstop deadline. NULL is a no-op.
///
/// # Safety
/// `g` is a valid gate handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_arm(g: *mut ReanchorGate) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(g) = unsafe { g.as_mut() } {
            g.arm(std::time::Instant::now());
        }
    });
}

/// Arm for a frame-index gap, pre-crediting `expected_drops` so a later
/// `frames_dropped` climb is not a second loss (double-arm race). Plain `arm`
/// for decoder wedge/demotion. NULL is a no-op.
///
/// # Safety
/// `g` is a valid gate handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_arm_expecting_drops(
    g: *mut ReanchorGate,
    expected_drops: u64,
) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(g) = unsafe { g.as_mut() } {
            g.arm_expecting_drops(std::time::Instant::now(), expected_drops);
        }
    });
}

/// Fold one decoded frame; `out_present` is whether to display it. Reads `FLAG_SOF`,
/// `USER_FLAG_RECOVERY_ANCHOR`, `USER_FLAG_RECOVERY_POINT`. Platform decoders that
/// do not flag IDRs pass `decoder_keyframe = false` — wire `FLAG_SOF` covers it.
/// Uncorroborated: C callers cannot supply bitstream evidence.
///
/// # Safety
/// `g` is a valid gate handle; `out_present` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_on_decoded(
    g: *mut ReanchorGate,
    flags: u32,
    decoder_keyframe: bool,
    out_present: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let present = g.on_decoded(flags, decoder_keyframe, std::time::Instant::now())
            == GateVerdict::Present;
        // SAFETY: the caller passes `out_present` null or writable for one value.
        unsafe { put(out_present, present) };
        PunktfunkStatus::Ok
    })
}

/// A received AU produced no decoded frame. Writes to `out_request_kf` whether the
/// no-output streak has tripped and the client should (throttled) request a keyframe
/// — the gate arms the freeze at the same time.
///
/// # Safety
/// `g` is a valid gate handle; `out_request_kf` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_on_no_output(
    g: *mut ReanchorGate,
    out_request_kf: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let request = g.on_no_output(std::time::Instant::now());
        // SAFETY: the caller passes `out_request_kf` null or writable for one value.
        unsafe { put(out_request_kf, request) };
        PunktfunkStatus::Ok
    })
}

/// Periodic fold of `frames_dropped` plus the overdue backstop. Writes to
/// `out_request_kf` whether the client should (throttled) request a keyframe
/// (a drop-count climb armed a freeze, or the freeze is overdue and re-asks).
///
/// # Safety
/// `g` is a valid gate handle; `out_request_kf` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_poll(
    g: *mut ReanchorGate,
    frames_dropped: u64,
    out_request_kf: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let request = g.poll(frames_dropped, std::time::Instant::now());
        // SAFETY: the caller passes `out_request_kf` null or writable for one value.
        unsafe { put(out_request_kf, request) };
        PunktfunkStatus::Ok
    })
}

/// Whether the gate is currently withholding concealed frames (frozen on the last
/// good picture). Writes `false` on a NULL gate.
///
/// # Safety
/// `g` is a valid gate handle; `out_holding` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_is_holding(
    g: *const ReanchorGate,
    out_holding: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let holding = unsafe { g.as_ref() }.is_some_and(ReanchorGate::is_holding);
        // SAFETY: the caller passes `out_holding` null or writable for one value.
        unsafe { put(out_holding, holding) };
        PunktfunkStatus::Ok
    })
}

// C wrapper for [`AuAdmission`]: one per elementary stream, every AU in receive order.

/// [`punktfunk_au_admission_note`]'s `concealed`: the lane has no concealer.
pub const PUNKTFUNK_CONCEALED_NONE: u32 = 0;
/// The concealer left every reference on a picture the decoder holds.
pub const PUNKTFUNK_CONCEALED_DECODABLE: u32 = 1;
/// Nothing can stand in for the lost reference.
pub const PUNKTFUNK_CONCEALED_UNRECOVERABLE: u32 = 2;

/// Create an admission rule. Free with [`punktfunk_au_admission_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_au_admission_new() -> *mut AuAdmission {
    Box::into_raw(Box::default())
}

/// Free a rule created by [`punktfunk_au_admission_new`]. NULL is a no-op.
///
/// # Safety
/// `a` was returned by [`punktfunk_au_admission_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_au_admission_free(a: *mut AuAdmission) {
    guard_void(|| {
        if !a.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(a) });
        }
    });
}

/// Fold one AU: its frame index, the index gap ahead of it (0 for none), its wire flags,
/// whether the decoder is strict, and a `PUNKTFUNK_CONCEALED_*`. Writes whether to keep the
/// AU off the decoder and whether to ask for a keyframe. An unknown `concealed` returns
/// [`PunktfunkStatus::InvalidArg`].
///
/// # Safety
/// `a` is a valid handle; the out pointers are writable or NULL.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_au_admission_note(
    a: *mut AuAdmission,
    index: u32,
    gap: u32,
    flags: u32,
    strict: bool,
    concealed: u32,
    out_withhold: *mut bool,
    out_ask_keyframe: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut` never dereferences null.
        let Some(a) = (unsafe { a.as_mut() }) else {
            return PunktfunkStatus::NullPointer;
        };
        let verdict = match concealed {
            PUNKTFUNK_CONCEALED_NONE => None,
            PUNKTFUNK_CONCEALED_DECODABLE => Some(punktfunk_core::reanchor::Concealment::Decodable),
            PUNKTFUNK_CONCEALED_UNRECOVERABLE => {
                Some(punktfunk_core::reanchor::Concealment::Unrecoverable)
            }
            _ => return PunktfunkStatus::InvalidArg,
        };
        let class = if strict {
            DecoderClass::Strict
        } else {
            DecoderClass::Lenient
        };
        let step = a.note(index, gap, flags, class, verdict);
        // SAFETY: caller out-params, null or writable. `put` writes without reading, so C may
        // pass them uninitialised.
        unsafe {
            put(out_withhold, step.withhold);
            put(out_ask_keyframe, step.ask_keyframe);
        }
        PunktfunkStatus::Ok
    })
}
