//! C wrapper for [`H265Concealer`]: a platform decoder that reads the RPS itself (VideoToolbox)
//! gets every access unit through `conceal` first, in decode order, and decodes what it says.

use crate::*;
use pf_bitstream::h265::conceal::{Concealment, H265Concealer};

/// Outcome of [`punktfunk_h265_concealer_conceal`].
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PunktfunkConcealment {
    /// Decode the access unit as it came.
    Intact = 0,
    /// Decode the returned bytes instead: a missing current reference was moved to a picture
    /// the decoder holds.
    Rewritten = 1,
    /// Keep this and every following delta off the decoder and ask for an IDR.
    Unrecoverable = 2,
}

/// Opaque handle: one elementary stream's [`H265Concealer`]. The type lives in another crate,
/// so the header needs a core-owned name for it.
pub struct PunktfunkH265Concealer {
    inner: H265Concealer,
}

/// Create an HEVC reference concealer for one elementary stream (IDR first). Free with
/// [`punktfunk_h265_concealer_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_h265_concealer_new() -> *mut PunktfunkH265Concealer {
    Box::into_raw(Box::new(PunktfunkH265Concealer {
        inner: H265Concealer::new(),
    }))
}

/// Free a concealer created by [`punktfunk_h265_concealer_new`]. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_h265_concealer_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_free(c: *mut PunktfunkH265Concealer) {
    guard_void(|| {
        if !c.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(c) });
        }
    });
}

/// Fold one Annex-B access unit.
///
/// `out_kind` says what to decode. For `Rewritten`, `out_buf` and `out_len`
/// hold the bytes until [`punktfunk_h265_concealer_release`]. A length that
/// cannot fit a Rust slice returns [`PunktfunkStatus::InvalidArg`].
///
/// # Safety
/// `c` is a valid handle; `au` points to `len` readable bytes; the out pointers are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_conceal(
    c: *mut PunktfunkH265Concealer,
    au: *const u8,
    len: usize,
    out_kind: *mut PunktfunkConcealment,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut` never dereferences null.
        let Some(c) = (unsafe { c.as_mut() }) else {
            return PunktfunkStatus::NullPointer;
        };
        // Out-slots are written through the raw pointer: C may pass them uninitialised.
        if out_kind.is_null() || out_buf.is_null() || out_len.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: `au` is null or readable for `len` bytes (this fn's contract).
        let bytes = match unsafe { in_bytes(au, len) } {
            Ok(b) => b,
            Err(s) => return s,
        };
        let (kind, buf, n) = match c.inner.conceal(bytes) {
            Concealment::Intact => (PunktfunkConcealment::Intact, std::ptr::null_mut(), 0),
            Concealment::Rewritten(v) => {
                let boxed = v.into_boxed_slice();
                let n = boxed.len();
                (
                    PunktfunkConcealment::Rewritten,
                    Box::into_raw(boxed).cast::<u8>(),
                    n,
                )
            }
            Concealment::Unrecoverable => {
                (PunktfunkConcealment::Unrecoverable, std::ptr::null_mut(), 0)
            }
        };
        // SAFETY: the three out-pointers are non-null (checked above) and writable per contract.
        unsafe {
            out_kind.write(kind);
            out_buf.write(buf);
            out_len.write(n);
        }
        PunktfunkStatus::Ok
    })
}

/// Release a buffer [`punktfunk_h265_concealer_conceal`] returned. NULL is a no-op.
///
/// # Safety
/// `buf`/`len` are exactly what one `conceal` call returned, released once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_release(buf: *mut u8, len: usize) {
    guard_void(|| {
        if !buf.is_null() {
            // SAFETY: the pair came from `Box::<[u8]>::into_raw` above, released once.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buf, len)) });
        }
    });
}

/// [`punktfunk_av1_sequence_info`]'s answer: what an `av1C` record and a colour description
/// take from an AV1 sequence header. Colour codes are ITU-T H.273, 2 when none is coded.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PunktfunkAv1SequenceInfo {
    pub profile: u8,
    pub level_idx0: u8,
    pub tier0: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono_chrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// Parse the first sequence header in `data`, a low-overhead temporal unit or a run of sized
/// OBUs. `InvalidArg` when it carries none or the header does not parse.
///
/// # Safety
/// `data` points to `len` readable bytes; `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_av1_sequence_info(
    data: *const u8,
    len: usize,
    out: *mut PunktfunkAv1SequenceInfo,
) -> PunktfunkStatus {
    guard(|| {
        // `out` is written through the raw pointer: C may pass it uninitialised.
        if out.is_null() || data.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if ffi_slice_bytes::<u8>(len).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `data` is non-null and `ffi_slice_bytes` proved the extent fits a Rust slice.
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        let Some(i) = pf_bitstream::av1::sequence_info(bytes) else {
            return PunktfunkStatus::InvalidArg;
        };
        let info = PunktfunkAv1SequenceInfo {
            profile: i.profile,
            level_idx0: i.level_idx0,
            tier0: i.tier0,
            high_bitdepth: i.high_bitdepth,
            twelve_bit: i.twelve_bit,
            mono_chrome: i.mono_chrome,
            subsampling_x: i.subsampling_x,
            subsampling_y: i.subsampling_y,
            chroma_sample_position: i.chroma_sample_position,
            color_primaries: i.color_primaries,
            transfer_characteristics: i.transfer_characteristics,
            matrix_coefficients: i.matrix_coefficients,
            full_range: i.full_range,
            max_width: i.max_width,
            max_height: i.max_height,
        };
        // SAFETY: `out` is non-null (checked above) and writable per contract.
        unsafe { out.write(info) };
        PunktfunkStatus::Ok
    })
}
