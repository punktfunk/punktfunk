//! C wrappers for [`H265Concealer`] and [`Av1Concealer`]: a platform decoder that reads its
//! references itself (VideoToolbox) gets every access unit it is fed through `conceal` first,
//! in decode order, and decodes what it says.

use crate::*;
use pf_bitstream::av1::conceal::Av1Concealer;
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
    /// Keep this access unit off the decoder and ask for recovery. The concealer forgets it,
    /// so the host's anchor on a picture the decoder holds comes back `Intact`.
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
        // SAFETY: this fn's contract is `conceal_out`'s.
        unsafe { conceal_out(|b| c.inner.conceal(b), au, len, out_kind, out_buf, out_len) }
    })
}

/// Run `conceal` over `au` and write its outcome to the out-slots, as the `conceal` calls
/// document.
///
/// # Safety
/// `au` points to `len` readable bytes; the out pointers are null or writable.
unsafe fn conceal_out(
    conceal: impl FnOnce(&[u8]) -> Concealment,
    au: *const u8,
    len: usize,
    out_kind: *mut PunktfunkConcealment,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
) -> PunktfunkStatus {
    // Out-slots are written through the raw pointer: C may pass them uninitialised.
    if out_kind.is_null() || out_buf.is_null() || out_len.is_null() {
        return PunktfunkStatus::NullPointer;
    }
    // SAFETY: `au` is null or readable for `len` bytes (this fn's contract).
    let bytes = match unsafe { in_bytes(au, len) } {
        Ok(b) => b,
        Err(s) => return s,
    };
    let (kind, buf, n) = match conceal(bytes) {
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
}

/// Opaque handle: one AV1 elementary stream's [`Av1Concealer`].
pub struct PunktfunkAv1Concealer {
    inner: Av1Concealer,
}

/// Create an AV1 reference concealer for one elementary stream (key frame first). Free with
/// [`punktfunk_av1_concealer_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_av1_concealer_new() -> *mut PunktfunkAv1Concealer {
    Box::into_raw(Box::new(PunktfunkAv1Concealer {
        inner: Av1Concealer::new(),
    }))
}

/// Free a concealer created by [`punktfunk_av1_concealer_new`]. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_av1_concealer_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_av1_concealer_free(c: *mut PunktfunkAv1Concealer) {
    guard_void(|| {
        if !c.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(c) });
        }
    });
}

/// The `concealed` verdict [`punktfunk_au_admission_note`] takes for one whole AV1 temporal
/// unit with wire `flags`, judged before the rule runs. `PUNKTFUNK_CONCEALED_DECODABLE` for an
/// error-resilient wave start: [`punktfunk_av1_concealer_conceal`] rewrites its stale
/// reference once the rule admits it. `PUNKTFUNK_CONCEALED_NONE` for every other AU. The
/// concealer is left as it was.
///
/// # Safety
/// `c` is a valid handle; `au` points to `len` readable bytes; `out_concealed` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_av1_concealer_verdict(
    c: *const PunktfunkAv1Concealer,
    au: *const u8,
    len: usize,
    flags: u32,
    out_concealed: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return PunktfunkStatus::NullPointer;
        };
        if out_concealed.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: `au` is null or readable for `len` bytes (this fn's contract).
        let bytes = match unsafe { in_bytes(au, len) } {
            Ok(b) => b,
            Err(s) => return s,
        };
        let wave_start = flags & punktfunk_core::packet::USER_FLAG_RECOVERY_POINT != 0;
        let verdict = if wave_start && c.inner.error_resilient(bytes) {
            crate::reanchor::PUNKTFUNK_CONCEALED_DECODABLE
        } else {
            crate::reanchor::PUNKTFUNK_CONCEALED_NONE
        };
        // SAFETY: non-null (checked above) and writable per contract.
        unsafe { out_concealed.write(verdict) };
        PunktfunkStatus::Ok
    })
}

/// Fold one temporal unit the decoder is fed, in decode order. Outs as
/// [`punktfunk_h265_concealer_conceal`]; release a `Rewritten` buffer with
/// [`punktfunk_h265_concealer_release`]. An AU the rule withholds never reaches this: the
/// concealer mirrors the decoder's slots.
///
/// # Safety
/// `c` is a valid handle; `au` points to `len` readable bytes; the out pointers are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_av1_concealer_conceal(
    c: *mut PunktfunkAv1Concealer,
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
        // SAFETY: this fn's contract is `conceal_out`'s.
        unsafe { conceal_out(|b| c.inner.conceal(b), au, len, out_kind, out_buf, out_len) }
    })
}

/// Release a buffer [`punktfunk_h265_concealer_conceal`] or
/// [`punktfunk_av1_concealer_conceal`] returned. NULL is a no-op.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reanchor::{PUNKTFUNK_CONCEALED_DECODABLE, PUNKTFUNK_CONCEALED_NONE};
    use punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;

    /// pf-bitstream's one-slot libaom chain: frame 14 alone is error-resilient.
    fn chain() -> Vec<&'static [u8]> {
        let ivf: &'static [u8] =
            include_bytes!("../../../codec/pf-bitstream/tests/vectors/aom-one-slot-er14.ivf");
        let mut aus = Vec::new();
        let mut at = 32;
        while at + 12 <= ivf.len() {
            let n = u32::from_le_bytes(ivf[at..at + 4].try_into().unwrap()) as usize;
            aus.push(&ivf[at + 12..at + 12 + n]);
            at += 12 + n;
        }
        aus
    }

    fn verdict(c: *mut PunktfunkAv1Concealer, au: &[u8], flags: u32) -> u32 {
        let mut v = u32::MAX;
        // SAFETY: a live handle, a borrowed slice, a local out-slot.
        let s = unsafe { punktfunk_av1_concealer_verdict(c, au.as_ptr(), au.len(), flags, &mut v) };
        assert_eq!(s, PunktfunkStatus::Ok);
        v
    }

    fn conceal(c: *mut PunktfunkAv1Concealer, au: &[u8]) -> PunktfunkConcealment {
        let (mut kind, mut buf, mut len) = (PunktfunkConcealment::Intact, std::ptr::null_mut(), 0);
        // SAFETY: a live handle, a borrowed slice, local out-slots; the buffer is released once.
        unsafe {
            let s = punktfunk_av1_concealer_conceal(
                c,
                au.as_ptr(),
                au.len(),
                &mut kind,
                &mut buf,
                &mut len,
            );
            assert_eq!(s, PunktfunkStatus::Ok);
            punktfunk_h265_concealer_release(buf, len);
        }
        kind
    }

    /// Frames 0 to 3 fed, 4 lost, 5 to 13 withheld: only the resilient wave start passes the
    /// strict rule, and the feed rewrites it.
    #[test]
    fn only_a_resilient_av1_wave_start_is_judged_decodable() {
        let aus = chain();
        let c = punktfunk_av1_concealer_new();
        for au in &aus[..4] {
            assert_eq!(conceal(c, au), PunktfunkConcealment::Intact);
        }
        assert_eq!(
            verdict(c, aus[5], USER_FLAG_RECOVERY_POINT),
            PUNKTFUNK_CONCEALED_NONE
        );
        assert_eq!(verdict(c, aus[14], 0), PUNKTFUNK_CONCEALED_NONE);
        assert_eq!(
            verdict(c, aus[14], USER_FLAG_RECOVERY_POINT),
            PUNKTFUNK_CONCEALED_DECODABLE
        );
        assert_eq!(conceal(c, aus[14]), PunktfunkConcealment::Rewritten);
        assert_eq!(conceal(c, aus[15]), PunktfunkConcealment::Intact);
        // SAFETY: the handle from `new`, freed once.
        unsafe { punktfunk_av1_concealer_free(c) };
    }
}
