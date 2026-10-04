//! The capability bitset: bit `n` is bit `n % 8` of byte `n / 8`, trailing zero bytes dropped.
//!
//! Bits 0–31 are the four `punktfunk/1` capability bytes in place
//! ([`FEATURE_V1_VIDEO_CAPS`](super::registry::FEATURE_V1_VIDEO_CAPS) and its neighbours), so
//! core maps between the wires exactly and embedders keep setting the bytes they set today.
//! A reader keeps the first 128 bits and ignores the rest: a bit it does not know is a feature it
//! does not have.

use super::registry::{
    FEATURE_V1_CLIENT_CAPS, FEATURE_V1_HOST_CAPS, FEATURE_V1_HOST_CAPS2, FEATURE_V1_VIDEO_CAPS,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureSet(u128);

impl FeatureSet {
    pub fn has(self, bit: u32) -> bool {
        bit < 128 && (self.0 >> bit) & 1 == 1
    }

    pub fn with(self, bit: u32) -> FeatureSet {
        FeatureSet(self.0 | 1u128.checked_shl(bit).unwrap_or(0))
    }

    /// The bits both sets hold.
    pub fn intersect(self, other: FeatureSet) -> FeatureSet {
        FeatureSet(self.0 & other.0)
    }

    pub fn union(self, other: FeatureSet) -> FeatureSet {
        FeatureSet(self.0 | other.0)
    }

    /// The bits from 32 up: what only this wire can say.
    pub fn native(self) -> FeatureSet {
        FeatureSet(self.0 & !0xFFFF_FFFF)
    }

    pub fn encode(self) -> Vec<u8> {
        let b = self.0.to_le_bytes();
        let used = b.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
        b[..used].to_vec()
    }

    /// Never fails: a short string is a peer with fewer features, a long one a newer peer.
    pub fn decode(b: &[u8]) -> FeatureSet {
        let mut w = [0u8; 16];
        let n = b.len().min(16);
        w[..n].copy_from_slice(&b[..n]);
        FeatureSet(u128::from_le_bytes(w))
    }

    /// A client's set from its `punktfunk/1` capability bytes.
    pub fn client(video_caps: u8, client_caps: u8) -> FeatureSet {
        FeatureSet(
            u128::from(video_caps) << FEATURE_V1_VIDEO_CAPS
                | u128::from(client_caps) << FEATURE_V1_CLIENT_CAPS,
        )
    }

    /// A host's set from its `punktfunk/1` capability bytes.
    pub fn host(host_caps: u8, host_caps2: u8) -> FeatureSet {
        FeatureSet(
            u128::from(host_caps) << FEATURE_V1_HOST_CAPS
                | u128::from(host_caps2) << FEATURE_V1_HOST_CAPS2,
        )
    }

    fn byte_at(self, first_bit: u32) -> u8 {
        (self.0 >> first_bit) as u8
    }

    pub fn video_caps(self) -> u8 {
        self.byte_at(FEATURE_V1_VIDEO_CAPS)
    }

    pub fn client_caps(self) -> u8 {
        self.byte_at(FEATURE_V1_CLIENT_CAPS)
    }

    pub fn host_caps(self) -> u8 {
        self.byte_at(FEATURE_V1_HOST_CAPS)
    }

    pub fn host_caps2(self) -> u8 {
        self.byte_at(FEATURE_V1_HOST_CAPS2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{
        CLIENT_CAP_EXT, HOST_CAP2_INPUT_EDGES, HOST_CAP_AUDIO_HIRES, VIDEO_CAP_MULTI_SLICE,
    };

    #[test]
    fn v1_bytes_round_trip_in_place() {
        let c = FeatureSet::client(VIDEO_CAP_MULTI_SLICE | 0x01, CLIENT_CAP_EXT);
        let h = FeatureSet::host(HOST_CAP_AUDIO_HIRES, HOST_CAP2_INPUT_EDGES);
        assert_eq!((c.video_caps(), c.client_caps()), (0x81, CLIENT_CAP_EXT));
        assert_eq!(
            (h.host_caps(), h.host_caps2()),
            (HOST_CAP_AUDIO_HIRES, HOST_CAP2_INPUT_EDGES)
        );
        assert_eq!((c.host_caps(), h.video_caps()), (0, 0));
        assert!(c.has(7) && c.has(14) && !c.has(16));
    }

    #[test]
    fn encoding_drops_trailing_zeros_and_decoding_ignores_unknown_bits() {
        assert_eq!(FeatureSet::default().encode(), Vec::<u8>::new());
        assert_eq!(FeatureSet::client(0x03, 0).encode(), [0x03]);
        let far = FeatureSet::default().with(127).with(40);
        assert_eq!(FeatureSet::decode(&far.encode()), far);
        let mut long = far.encode();
        long.extend_from_slice(&[0xFF; 8]);
        assert_eq!(FeatureSet::decode(&long), far);
        assert!(!far.with(500).has(500));
    }

    #[test]
    fn intersect_keeps_what_both_hold() {
        let a = FeatureSet::default().with(1).with(33);
        let b = FeatureSet::default().with(33).with(2);
        assert_eq!(a.intersect(b), FeatureSet::default().with(33));
    }
}
