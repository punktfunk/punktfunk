//! Little-endian field framing for the tagged datagram planes. [`Wr`] appends fields in wire
//! order; [`Rd`] checks the tag and minimum length once, then takes fields in the same order.

/// Builds one datagram: `Wr::tag(TAG, len).u32(a).u64(b).bytes(c).done()`.
pub(super) struct Wr(Vec<u8>);

impl Wr {
    /// One tag byte, with room for a `cap`-byte datagram.
    pub(super) fn tag(tag: u8, cap: usize) -> Wr {
        let mut b = Vec::with_capacity(cap);
        b.push(tag);
        Wr(b)
    }

    pub(super) fn u32(self, v: u32) -> Wr {
        self.bytes(&v.to_le_bytes())
    }

    pub(super) fn u64(self, v: u64) -> Wr {
        self.bytes(&v.to_le_bytes())
    }

    pub(super) fn bytes(mut self, v: &[u8]) -> Wr {
        self.0.extend_from_slice(v);
        self
    }

    pub(super) fn done(self) -> Vec<u8> {
        self.0
    }
}

/// Reads one datagram whose tag and length [`Rd::tag`] already checked. A fixed field read past
/// the checked length panics: that is a codec bug, not bad input.
pub(super) struct Rd<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Rd<'a> {
    /// A datagram tagged `tag` of at least `min_len` bytes.
    pub(super) fn tag(b: &'a [u8], tag: u8, min_len: usize) -> Option<Rd<'a>> {
        (b.len() >= min_len.max(1) && b[0] == tag).then_some(Rd { b, off: 1 })
    }

    fn take<const N: usize>(&mut self) -> [u8; N] {
        let v = self.b[self.off..self.off + N].try_into().unwrap();
        self.off += N;
        v
    }

    pub(super) fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }

    pub(super) fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }

    /// Everything after the fields read so far.
    pub(super) fn rest(self) -> &'a [u8] {
        &self.b[self.off..]
    }
}
