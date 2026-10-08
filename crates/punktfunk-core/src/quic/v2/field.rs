//! Varints, fields and frames: the three encodings every v2 message is built from.
//!
//! A varint is QUIC's (RFC 9000 §16): two length bits, then 6, 14, 30 or 62 value bits,
//! big-endian. A frame is `type ‖ len ‖ body`, a body is `field*`, a field is `tag ‖ len ‖ value`.
//! Integers inside a value are little-endian. A reader takes an unsigned integer of any width
//! from one to eight bytes, so a field can widen later without a new tag.
//!
//! Every length is checked before it slices, and a frame's length against its type's bound
//! before anything allocates: these bytes arrive before the peer is trusted.

use crate::error::{PunktfunkError, Result};

/// Largest value a varint holds.
pub const VARINT_MAX: u64 = (1 << 62) - 1;

/// Append `v` as a varint in its shortest form. Values past [`VARINT_MAX`] are clamped, which
/// no caller reaches: tags, types and lengths are small.
pub fn put_varint(out: &mut Vec<u8>, v: u64) {
    let v = v.min(VARINT_MAX);
    match v {
        0..=0x3F => out.push(v as u8),
        0x40..=0x3FFF => out.extend_from_slice(&(v as u16 | 0x4000).to_be_bytes()),
        0x4000..=0x3FFF_FFFF => out.extend_from_slice(&(v as u32 | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(v | 0xC000_0000_0000_0000).to_be_bytes()),
    }
}

/// The varint at the front of `b` and the bytes it took; `None` while `b` is too short.
/// A longer-than-needed encoding is accepted, as QUIC accepts it.
pub fn get_varint(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let len = 1usize << (first >> 6);
    let bytes = b.get(..len)?;
    let v = bytes[1..]
        .iter()
        .fold(u64::from(first & 0x3F), |v, &x| (v << 8) | u64::from(x));
    Some((v, len))
}

/// Builds a message body field by field: `Fields::new().u32(MODE_W, w).str(NAME, n).frame(TY)`.
/// Tags go out in the order written; readers do not depend on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields(Vec<u8>);

impl Fields {
    pub fn new() -> Fields {
        Fields(Vec::new())
    }

    pub fn bytes(mut self, tag: u64, v: &[u8]) -> Fields {
        put_varint(&mut self.0, tag);
        put_varint(&mut self.0, v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    pub fn u8(self, tag: u64, v: u8) -> Fields {
        self.bytes(tag, &[v])
    }

    pub fn u16(self, tag: u64, v: u16) -> Fields {
        self.bytes(tag, &v.to_le_bytes())
    }

    pub fn u32(self, tag: u64, v: u32) -> Fields {
        self.bytes(tag, &v.to_le_bytes())
    }

    /// One field per value, all under `tag`: a repeated field.
    pub fn u16s(self, tag: u64, vs: &[u16]) -> Fields {
        vs.iter().fold(self, |f, &v| f.u16(tag, v))
    }

    pub fn u64(self, tag: u64, v: u64) -> Fields {
        self.bytes(tag, &v.to_le_bytes())
    }

    pub fn bool(self, tag: u64, v: bool) -> Fields {
        self.u8(tag, u8::from(v))
    }

    pub fn str(self, tag: u64, v: &str) -> Fields {
        self.bytes(tag, v.as_bytes())
    }

    /// A nested struct as one field.
    pub fn fields(self, tag: u64, inner: Fields) -> Fields {
        self.bytes(tag, &inner.0)
    }

    /// Apply `f` only when `cond` holds: the shape of every optional field.
    pub fn when(self, cond: bool, f: impl FnOnce(Fields) -> Fields) -> Fields {
        if cond {
            f(self)
        } else {
            self
        }
    }

    pub fn into_body(self) -> Vec<u8> {
        self.0
    }

    /// This body as a whole frame of type `ty`.
    pub fn frame(self, ty: u64) -> Vec<u8> {
        frame(ty, &self.0)
    }
}

/// `ty ‖ len ‖ body`.
pub fn frame(ty: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    put_varint(&mut out, ty);
    put_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// Type, body length and header length of the frame at the front of `b`; `None` while the
/// header is incomplete. A stream reader checks the length against the type's bound here,
/// before it buffers the body.
pub fn frame_header(b: &[u8]) -> Option<(u64, u64, usize)> {
    let (ty, a) = get_varint(b)?;
    let (len, c) = get_varint(&b[a..])?;
    Some((ty, len, a + c))
}

/// One frame off the front of `b`: `(type, body, bytes taken)`. `Ok(None)` while `b` holds less
/// than a frame. `Err` when the body is longer than `bound(type)` allows.
pub fn split_frame(b: &[u8], bound: impl Fn(u64) -> usize) -> Result<Option<(u64, &[u8], usize)>> {
    let Some((ty, len, hdr)) = frame_header(b) else {
        return Ok(None);
    };
    if len > bound(ty) as u64 {
        return Err(PunktfunkError::InvalidArg("frame over its type's bound"));
    }
    let end = hdr + len as usize;
    Ok(b.get(hdr..end).map(|body| (ty, body, end)))
}

/// Walks the fields of one body.
pub struct FieldReader<'a> {
    rest: &'a [u8],
}

impl<'a> FieldReader<'a> {
    pub fn new(body: &'a [u8]) -> FieldReader<'a> {
        FieldReader { rest: body }
    }

    /// The next `(tag, value)`, `None` at the end. A field that runs past the body is an error:
    /// half of what the peer meant is not acted on.
    pub fn next_field(&mut self) -> Result<Option<(u64, &'a [u8])>> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let bad = || PunktfunkError::InvalidArg("truncated field");
        let (tag, a) = get_varint(self.rest).ok_or_else(bad)?;
        let (len, c) = get_varint(&self.rest[a..]).ok_or_else(bad)?;
        let start = a + c;
        let end = start
            .checked_add(usize::try_from(len).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let value = self.rest.get(start..end).ok_or_else(bad)?;
        self.rest = &self.rest[end..];
        Ok(Some((tag, value)))
    }
}

/// An unsigned integer of one to eight little-endian bytes.
pub fn uint(v: &[u8]) -> Result<u64> {
    if v.is_empty() || v.len() > 8 {
        return Err(PunktfunkError::InvalidArg("bad integer field"));
    }
    Ok(v.iter().rev().fold(0u64, |n, &b| (n << 8) | u64::from(b)))
}

pub fn u8_of(v: &[u8]) -> Result<u8> {
    u8::try_from(uint(v)?).map_err(|_| PunktfunkError::InvalidArg("integer field out of range"))
}

pub fn u16_of(v: &[u8]) -> Result<u16> {
    u16::try_from(uint(v)?).map_err(|_| PunktfunkError::InvalidArg("integer field out of range"))
}

pub fn u32_of(v: &[u8]) -> Result<u32> {
    u32::try_from(uint(v)?).map_err(|_| PunktfunkError::InvalidArg("integer field out of range"))
}

pub fn u64_of(v: &[u8]) -> Result<u64> {
    uint(v)
}

pub fn bool_of(v: &[u8]) -> Result<bool> {
    Ok(uint(v)? != 0)
}

/// UTF-8 of at most `max` bytes.
pub fn str_of(v: &[u8], max: usize) -> Result<&str> {
    if v.len() > max {
        return Err(PunktfunkError::InvalidArg("string field too long"));
    }
    std::str::from_utf8(v).map_err(|_| PunktfunkError::InvalidArg("string field not UTF-8"))
}

/// Fill `slot` once. A tag the message declares single may not repeat.
pub fn set_once<T>(slot: &mut Option<T>, v: T) -> Result<()> {
    if slot.is_some() {
        return Err(PunktfunkError::InvalidArg("repeated field"));
    }
    *slot = Some(v);
    Ok(())
}

/// One more value of a repeated field; past `max` the message is refused.
pub fn push_bounded<T>(list: &mut Vec<T>, v: T, max: usize) -> Result<()> {
    if list.len() == max {
        return Err(PunktfunkError::InvalidArg("repeated field past its bound"));
    }
    list.push(v);
    Ok(())
}

/// A field the message cannot do without.
pub fn required<T>(slot: Option<T>, what: &'static str) -> Result<T> {
    slot.ok_or(PunktfunkError::InvalidArg(what))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn varint_boundaries_take_their_rfc_widths() {
        for (v, len) in [
            (0u64, 1),
            (63, 1),
            (64, 2),
            (16_383, 2),
            (16_384, 4),
            (1_073_741_823, 4),
            (1_073_741_824, 8),
            (VARINT_MAX, 8),
        ] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(b.len(), len, "{v}");
            assert_eq!(get_varint(&b), Some((v, len)), "{v}");
        }
        // RFC 9000 §A.1 sample vectors.
        assert_eq!(
            get_varint(&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c]),
            Some((151_288_809_941_952_652, 8))
        );
        assert_eq!(
            get_varint(&[0x9d, 0x7f, 0x3e, 0x7d]),
            Some((494_878_333, 4))
        );
        assert_eq!(get_varint(&[0x7b, 0xbd]), Some((15_293, 2)));
        assert_eq!(get_varint(&[0x40, 0x25]), Some((37, 2)));
        assert_eq!(get_varint(&[0x9d, 0x7f]), None);
    }

    #[test]
    fn fields_round_trip_and_unknown_tags_are_just_more_fields() {
        let body = Fields::new()
            .u8(1, 7)
            .u32(2, 0xDEAD_BEEF)
            .str(900, "unknown to this reader")
            .bool(3, true)
            .into_body();
        let mut r = FieldReader::new(&body);
        let mut seen = Vec::new();
        while let Some((tag, v)) = r.next_field().unwrap() {
            match tag {
                1 => seen.push(u8_of(v).unwrap() as u64),
                2 => seen.push(u32_of(v).unwrap() as u64),
                3 => seen.push(bool_of(v).unwrap() as u64),
                _ => {}
            }
        }
        assert_eq!(seen, [7, 0xDEAD_BEEF, 1]);
    }

    #[test]
    fn a_narrow_integer_widens_without_a_new_tag() {
        assert_eq!(u32_of(&[0x34, 0x12]).unwrap(), 0x1234);
        assert!(u8_of(&0x1234u16.to_le_bytes()).is_err());
        assert!(uint(&[]).is_err());
        assert!(uint(&[0; 9]).is_err());
    }

    #[test]
    fn split_frame_waits_for_the_body_and_enforces_the_bound() {
        let f = Fields::new().u16(1, 5).frame(0x22);
        assert_eq!(split_frame(&f[..f.len() - 1], |_| 64).unwrap(), None);
        let (ty, body, n) = split_frame(&f, |_| 64).unwrap().unwrap();
        assert_eq!((ty, n), (0x22, f.len()));
        assert_eq!(body, &f[2..]);
        assert!(split_frame(&f, |_| 3).is_err());
        assert!(split_frame(&[], |_| 64).unwrap().is_none());
    }

    #[test]
    fn set_once_refuses_a_repeat() {
        let mut slot = None;
        set_once(&mut slot, 1).unwrap();
        assert!(set_once(&mut slot, 2).is_err());
        assert_eq!(required(slot, "x").unwrap(), 1);
        assert!(required::<u8>(None, "x").is_err());
    }

    proptest! {
        #[test]
        fn varint_round_trips(v in 0..=VARINT_MAX) {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            prop_assert_eq!(get_varint(&b), Some((v, b.len())));
        }

        /// Hostile bytes never panic the reader and never yield a value past the body.
        #[test]
        fn field_reader_survives_any_bytes(body in proptest::collection::vec(any::<u8>(), 0..256)) {
            let mut r = FieldReader::new(&body);
            let mut taken = 0usize;
            while let Ok(Some((_, v))) = r.next_field() {
                taken += v.len();
                prop_assert!(taken <= body.len());
            }
            let _ = split_frame(&body, |_| 128);
        }
    }
}
