//! The one bounds-checked view of a mapped shared-memory section.
//!
//! The host's pad and AU sections, the capture worker, `pf-umdf-util`'s mapped views and
//! `pf-vdisplay`'s encode section reach the bytes through [`SectionView`]; the vdisplay cursor
//! section and the seat audio sections use raw pointers of their own. The pointer arithmetic, the
//! bounds and alignment checks, the atomic casts and the memcpys live here, in a crate Miri
//! interprets natively and at the MSVC layout. Mapping, unmapping and handles stay with each
//! owner.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Alignment of the widest atomic a view hands out. [`SectionView::from_raw`] requires it of the
/// base, so an offset aligned for the atomic is an address aligned for it.
const BASE_ALIGN: usize = align_of::<AtomicU64>();

/// A read/write window of `len` bytes over a mapped section, borrowed from whatever keeps it
/// mapped.
///
/// Words both sides touch go through aligned atomics ([`Self::atomic_u32`] and its siblings).
/// Report bodies move as bulk bytes: relaxed atomics ([`Self::read_bytes`]) or one plain memcpy
/// ([`Self::copy_from_slice`]) where the caller's protocol keeps the peer off the range. Every
/// accessor panics when its range leaves the view or an atomic is misaligned.
///
/// A reference an accessor returns lasts only as long as the borrow of the view, so an owner
/// that builds a `'static` view ends every reference with its own borrow, and unmaps after.
///
/// The view is neither `Send` nor `Sync`: an owner that shares it across threads proves why
/// that is sound for its own protocol.
#[derive(Clone, Copy)]
pub struct SectionView<'a> {
    base: *mut u8,
    len: usize,
    _mapping: PhantomData<&'a [u8]>,
}

impl<'a> SectionView<'a> {
    /// A view of the `len` bytes at `base`.
    ///
    /// # Safety
    /// `base` must be 8-aligned (checked), and `base..base + len` must stay mapped, readable
    /// and writable for `'a`. No Rust reference may overlap it except the atomics this view
    /// hands out. A peer may write it concurrently only by the protocol the caller follows:
    /// aligned atomic words, and bulk copies only where the peer is excluded or detected.
    // unsafe-fn-no-op-ok: the contract is the mapping's lifetime, checked by every accessor.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> Self {
        assert!(
            base.addr() % BASE_ALIGN == 0,
            "section base {:p} is not {BASE_ALIGN}-aligned",
            base
        );
        Self {
            base,
            len,
            _mapping: PhantomData,
        }
    }

    /// A view of a borrowed buffer: tests, and Miri. The words make the base 8-aligned by type.
    pub fn over(buf: &'a mut [u64]) -> Self {
        Self {
            base: buf.as_mut_ptr().cast(),
            len: size_of_val(buf),
            _mapping: PhantomData,
        }
    }

    /// Bytes in the view.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Panics unless `off..off + n` is inside the view and `off` is `align`-aligned.
    fn check(&self, off: usize, n: usize, align: usize) {
        assert!(
            off.checked_add(n).is_some_and(|end| end <= self.len),
            "section access out of bounds (off={off}, n={n}, len={})",
            self.len
        );
        assert!(
            off % align == 0,
            "section access misaligned (off={off}, align={align})"
        );
    }

    /// The atomic `T` at `off`. Private: `T` is one of the four atomic integers, for which every
    /// bit pattern is valid.
    fn atomic<T>(&self, off: usize) -> &T {
        const { assert!(align_of::<T>() <= BASE_ALIGN) };
        self.check(off, size_of::<T>(), align_of::<T>());
        // SAFETY: `check` proved the `T` inside the mapping `from_raw` keeps live, and its address
        // aligned: an 8-aligned base plus an offset aligned for `T`, which is at most 8-aligned.
        // Every bit pattern is a valid atomic integer, and a peer touches the word only
        // atomically, so a shared reference to it is sound for the borrow of `self`.
        unsafe { &*self.base.add(off).cast::<T>() }
    }

    /// The byte at `off` as an atomic.
    pub fn atomic_u8(&self, off: usize) -> &AtomicU8 {
        self.atomic(off)
    }

    /// The 2-aligned word at `off` as an atomic.
    pub fn atomic_u16(&self, off: usize) -> &AtomicU16 {
        self.atomic(off)
    }

    /// The 4-aligned word at `off` as an atomic.
    pub fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        self.atomic(off)
    }

    /// The 8-aligned word at `off` as an atomic.
    pub fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        self.atomic(off)
    }

    /// Load `dst.len()` bytes from `off`, each a relaxed atomic load. The peer may be writing
    /// them, so the copy can be torn; the caller's sequence check decides whether it is whole.
    pub fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.check(off, dst.len(), 1);
        for (i, byte) in dst.iter_mut().enumerate() {
            *byte = self.atomic_u8(off + i).load(Ordering::Relaxed);
        }
    }

    /// Store `src` at `off`, each byte a relaxed atomic store.
    pub fn write_bytes(&self, off: usize, src: &[u8]) {
        self.check(off, src.len(), 1);
        for (i, &byte) in src.iter().enumerate() {
            self.atomic_u8(off + i).store(byte, Ordering::Relaxed);
        }
    }

    /// One plain memcpy of `src` into `off..`, for a range the caller's protocol keeps every
    /// reader off until a later Release store publishes it.
    pub fn copy_from_slice(&self, off: usize, src: &[u8]) {
        self.check(off, src.len(), 1);
        // SAFETY: `check` proved the range inside the live mapping. `src` is a Rust slice, so it
        // cannot overlap the mapping, which no Rust reference covers. The caller's protocol keeps
        // a peer from reading the range.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(off), src.len()) }
    }

    /// One plain memcpy of `off..` into `dst`, for a range the caller's protocol keeps every
    /// writer off while it runs. A peer write racing it is a data race: use
    /// [`Self::read_bytes`] where one can.
    pub fn copy_to_slice(&self, off: usize, dst: &mut [u8]) {
        self.check(off, dst.len(), 1);
        // SAFETY: as `copy_from_slice`, copying the other way; `dst` is an exclusive Rust slice.
        unsafe { core::ptr::copy_nonoverlapping(self.base.add(off), dst.as_mut_ptr(), dst.len()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four words, 32 bytes: wide enough for every width twice over.
    const WORDS: usize = 4;
    const LEN: usize = WORDS * 8;

    fn bytes_of(buf: &[u64; WORDS]) -> [u8; LEN] {
        let mut out = [0u8; LEN];
        for (chunk, word) in out.chunks_exact_mut(8).zip(buf) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        out
    }

    #[test]
    fn atomics_round_trip_at_every_width() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        view.atomic_u8(3).store(0xA5, Ordering::Relaxed);
        view.atomic_u16(6).store(0xBEEF, Ordering::Relaxed);
        view.atomic_u32(12).store(0xDEAD_BEEF, Ordering::Release);
        view.atomic_u64(16)
            .store(0x0123_4567_89AB_CDEF, Ordering::SeqCst);
        assert_eq!(view.atomic_u8(3).load(Ordering::Relaxed), 0xA5);
        assert_eq!(view.atomic_u16(6).load(Ordering::Relaxed), 0xBEEF);
        assert_eq!(view.atomic_u32(12).load(Ordering::Acquire), 0xDEAD_BEEF);
        assert_eq!(
            view.atomic_u64(16).load(Ordering::SeqCst),
            0x0123_4567_89AB_CDEF
        );
        let b = bytes_of(&buf);
        assert_eq!(b[3], 0xA5);
        assert_eq!(&b[6..8], &0xBEEFu16.to_ne_bytes());
        assert_eq!(&b[12..16], &0xDEAD_BEEFu32.to_ne_bytes());
        assert_eq!(&b[16..24], &0x0123_4567_89AB_CDEFu64.to_ne_bytes());
    }

    /// Widths overlap in memory: a u64 is its two u32 halves and its eight bytes.
    #[test]
    fn widths_see_the_same_bytes() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        let word = 0x1122_3344_5566_7788u64;
        view.atomic_u64(8).store(word, Ordering::Relaxed);
        let halves = [
            view.atomic_u32(8).load(Ordering::Relaxed).to_ne_bytes(),
            view.atomic_u32(12).load(Ordering::Relaxed).to_ne_bytes(),
        ];
        assert_eq!(halves.concat(), word.to_ne_bytes());
        for (i, expect) in word.to_ne_bytes().into_iter().enumerate() {
            assert_eq!(view.atomic_u8(8 + i).load(Ordering::Relaxed), expect);
        }
    }

    #[test]
    fn the_last_word_of_each_width_is_reachable() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        view.atomic_u8(LEN - 1).store(1, Ordering::Relaxed);
        view.atomic_u16(LEN - 2).store(2, Ordering::Relaxed);
        view.atomic_u32(LEN - 4).store(4, Ordering::Relaxed);
        view.atomic_u64(LEN - 8).store(8, Ordering::Relaxed);
        assert_eq!(view.atomic_u64(LEN - 8).load(Ordering::Relaxed), 8);
    }

    #[test]
    fn relaxed_bytes_round_trip() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        view.write_bytes(5, &[1, 2, 3, 4, 5]);
        let mut got = [0u8; 5];
        view.read_bytes(5, &mut got);
        assert_eq!(got, [1, 2, 3, 4, 5]);
        assert_eq!(view.atomic_u8(4).load(Ordering::Relaxed), 0);
        assert_eq!(view.atomic_u8(10).load(Ordering::Relaxed), 0);
    }

    #[test]
    fn plain_copies_round_trip_and_agree_with_the_atomic_bytes() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        view.copy_from_slice(LEN - 3, &[7, 8, 9]);
        let mut plain = [0u8; 3];
        view.copy_to_slice(LEN - 3, &mut plain);
        assert_eq!(plain, [7, 8, 9]);
        let mut relaxed = [0u8; 3];
        view.read_bytes(LEN - 3, &mut relaxed);
        assert_eq!(relaxed, plain);
        view.write_bytes(0, &[3, 2, 1]);
        view.copy_to_slice(0, &mut plain);
        assert_eq!(plain, [3, 2, 1]);
    }

    #[test]
    fn zero_length_access_is_legal_up_to_the_end() {
        let mut buf = [0u64; WORDS];
        let view = SectionView::over(&mut buf);
        for off in [0, 1, LEN - 1, LEN] {
            view.read_bytes(off, &mut []);
            view.write_bytes(off, &[]);
            view.copy_from_slice(off, &[]);
            view.copy_to_slice(off, &mut []);
        }
    }

    #[test]
    fn an_empty_view_allows_only_empty_access() {
        let view = SectionView::over(&mut []);
        assert!(view.is_empty());
        view.read_bytes(0, &mut []);
        view.copy_from_slice(0, &[]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn an_empty_view_has_no_first_byte() {
        SectionView::over(&mut []).atomic_u8(0);
    }

    #[test]
    fn copies_of_a_view_share_the_bytes() {
        let mut buf = [0u64; WORDS];
        let a = SectionView::over(&mut buf);
        let b = a;
        a.atomic_u32(4).store(77, Ordering::Relaxed);
        assert_eq!(b.atomic_u32(4).load(Ordering::Relaxed), 77);
        assert_eq!((a.len(), b.len()), (LEN, LEN));
    }

    #[test]
    fn a_raw_view_reaches_the_same_bytes_as_the_buffer() {
        let mut buf = vec![0u64; WORDS];
        // SAFETY: `buf` is 8-aligned by type, `LEN` bytes long, and outlives `view`; no Rust
        // reference to it is used while `view` is.
        let view = unsafe { SectionView::from_raw(buf.as_mut_ptr().cast(), LEN) };
        view.atomic_u64(24).store(0xFEED, Ordering::Relaxed);
        assert_eq!(buf[3], 0xFEED);
    }

    #[test]
    #[should_panic(expected = "not 8-aligned")]
    fn a_misaligned_base_is_refused_at_construction() {
        let mut buf = [0u64; WORDS];
        // SAFETY: deliberately breaks the alignment clause of the contract, which `from_raw`
        // checks and refuses before any access. The pointer stays inside `buf`.
        let _ = unsafe { SectionView::from_raw(buf.as_mut_ptr().cast::<u8>().add(1), LEN - 1) };
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_byte_at_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u8(LEN);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_word_straddling_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u32(LEN - 2);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn an_aligned_u64_past_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u64(LEN);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn an_offset_that_overflows_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).read_bytes(usize::MAX, &mut [0; 2]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_read_past_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).read_bytes(LEN - 1, &mut [0; 2]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_write_past_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).write_bytes(LEN - 1, &[0; 2]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_copy_in_past_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).copy_from_slice(LEN - 1, &[0; 2]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_copy_out_past_the_end_is_out_of_bounds() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).copy_to_slice(LEN + 1, &mut []);
    }

    #[test]
    #[should_panic(expected = "misaligned")]
    fn a_u16_at_an_odd_offset_is_misaligned() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u16(1);
    }

    #[test]
    #[should_panic(expected = "misaligned")]
    fn a_u32_at_a_2_aligned_offset_is_misaligned() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u32(2);
    }

    #[test]
    #[should_panic(expected = "misaligned")]
    fn a_u64_at_a_4_aligned_offset_is_misaligned() {
        let mut buf = [0u64; WORDS];
        SectionView::over(&mut buf).atomic_u64(4);
    }
}
