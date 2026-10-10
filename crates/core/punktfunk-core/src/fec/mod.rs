//! Erasure coding. [`ErasureCoder`] fronts two backends: GF(2⁸) Cauchy
//! Reed–Solomon (Moonlight `nanors`) and GF(2¹⁶) Leopard-RS.
//!
//! GF(2⁸) caps a block at 255 shards. GF(2¹⁶) raises that to 65535 and
//! runs in O(n log n). Shard length is equal within a block.
//!
//! Pin GF(2⁸) with `nanors_exact_parity_vectors`. Round-trips for both
//! backends live in this module.

mod gf16;
mod gf8;

pub use gf16::Gf16Coder;
pub use gf8::Gf8Coder;

use crate::config::FecScheme;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FecError {
    #[error("invalid shard configuration: {0}")]
    Config(&'static str),
    #[error("too few shards to reconstruct (have {have}, need {need})")]
    TooFewShards { have: usize, need: usize },
    #[error("backend error: {0}")]
    Backend(&'static str),
}

/// All shards in a block are equal length.
pub trait ErasureCoder: Send + Sync {
    fn scheme(&self) -> FecScheme;

    /// `recovery_count == 0` returns empty. `data` is borrowed so the
    /// packetizer can point into the frame buffer without a per-shard copy.
    fn encode(&self, data: &[&[u8]], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError>;

    /// Pooled [`encode`](Self::encode): on success `out` holds exactly
    /// `recovery_count` shards, reusing existing `Vec`s. Default delegates
    /// to `encode` (unpooled). On error `out` is unspecified — do not send.
    fn encode_into(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), FecError> {
        *out = self.encode(data, recovery_count)?;
        Ok(())
    }

    /// `received` is length K+M: `0..K` originals, `K..` recovery;
    /// `Some` present, `None` lost. Returns the K originals in order.
    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, FecError>;

    /// Fill missing data shards in the caller's slots. A missing slot's
    /// bytes are unspecified on entry. `recovery` is `(index, bytes)` with
    /// `index < recovery_count` (declared M; the math needs M even when
    /// not every parity shard arrived). On error, discard the block.
    fn reconstruct_into(
        &self,
        recovery_count: usize,
        data: &mut [&mut [u8]],
        have: &[bool],
        recovery: &[(usize, &[u8])],
    ) -> Result<(), FecError>;
}

pub fn coder_for(scheme: FecScheme) -> Box<dyn ErasureCoder> {
    match scheme {
        FecScheme::Gf8 => Box::new(Gf8Coder::default()),
        FecScheme::Gf16 => Box::new(Gf16Coder::default()),
    }
}

/// Both backends call this first; their fast paths skip their own checks.
pub(crate) fn validate_block_shape(
    received: &[Option<Vec<u8>>],
    data_count: usize,
    recovery_count: usize,
) -> Result<(), FecError> {
    if received.len() != data_count + recovery_count {
        return Err(FecError::Config(
            "received length must equal data + recovery",
        ));
    }
    let mut len = None;
    for s in received.iter().flatten() {
        match len {
            None => len = Some(s.len()),
            Some(l) if l != s.len() => {
                return Err(FecError::Config("shards in a block must be equal length"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Both backends call this first.
pub(crate) fn validate_into_shape(
    data: &[&mut [u8]],
    have: &[bool],
    recovery: &[(usize, &[u8])],
    recovery_count: usize,
) -> Result<(), FecError> {
    if data.is_empty() {
        return Err(FecError::Config("no data shards"));
    }
    if have.len() != data.len() {
        return Err(FecError::Config("have length must equal data length"));
    }
    let len = data[0].len();
    if data.iter().any(|s| s.len() != len) {
        return Err(FecError::Config("shards in a block must be equal length"));
    }
    for &(j, bytes) in recovery {
        if j >= recovery_count {
            return Err(FecError::Config("recovery index out of range"));
        }
        if bytes.len() != len {
            return Err(FecError::Config("shards in a block must be equal length"));
        }
    }
    let present = have.iter().filter(|h| **h).count();
    if present + recovery.len() < data.len() {
        return Err(FecError::TooFewShards {
            have: present + recovery.len(),
            need: data.len(),
        });
    }
    Ok(())
}

pub(crate) fn validate_encode_shape(data: &[&[u8]]) -> Result<(), FecError> {
    let first = data
        .first()
        .ok_or(FecError::Config("no data shards"))?
        .len();
    if data.iter().any(|s| s.len() != first) {
        return Err(FecError::Config("data shards must be equal length"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(coder: &dyn ErasureCoder, k: usize, m: usize, shard_len: usize, lose: &[usize]) {
        let data: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..shard_len).map(|b| (i * 31 + b * 7) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let recovery = coder.encode(&refs, m).unwrap();
        assert_eq!(recovery.len(), m);

        let mut received: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
        received.extend(data.iter().cloned().map(Some));
        received.extend(recovery.iter().cloned().map(Some));
        for &idx in lose {
            received[idx] = None;
        }

        let restored = coder.reconstruct(k, m, &mut received).unwrap();
        assert_eq!(restored, data);
    }

    /// `reconstruct_into` against a contiguous buffer (reassembler frame
    /// layout). Present slots must stay untouched.
    fn roundtrip_into(
        coder: &dyn ErasureCoder,
        k: usize,
        m: usize,
        shard_len: usize,
        lose_data: &[usize],
        lose_recovery: &[usize],
    ) {
        let src: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..shard_len).map(|b| (i * 31 + b * 7) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = src.iter().map(|s| s.as_slice()).collect();
        let parity = coder.encode(&refs, m).unwrap();

        let mut buf = vec![0u8; k * shard_len];
        let mut have = vec![true; k];
        for (i, s) in src.iter().enumerate() {
            if lose_data.contains(&i) {
                have[i] = false; // codec must fill this hole
            } else {
                buf[i * shard_len..(i + 1) * shard_len].copy_from_slice(s);
            }
        }
        let recovery: Vec<(usize, &[u8])> = parity
            .iter()
            .enumerate()
            .filter(|(j, _)| !lose_recovery.contains(j))
            .map(|(j, p)| (j, p.as_slice()))
            .collect();

        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(shard_len).collect();
        coder
            .reconstruct_into(m, &mut slots, &have, &recovery)
            .unwrap();
        for (i, s) in src.iter().enumerate() {
            assert_eq!(
                &buf[i * shard_len..(i + 1) * shard_len],
                s.as_slice(),
                "shard {i}"
            );
        }
    }

    #[test]
    fn gf16_reconstruct_into_fills_only_the_holes() {
        roundtrip_into(&Gf16Coder::default(), 16, 4, 256, &[1, 9], &[3]);
        roundtrip_into(&Gf16Coder::default(), 4, 2, 16, &[0, 3], &[]);
        roundtrip_into(&Gf16Coder::default(), 4, 2, 16, &[], &[0, 1]);
    }

    #[test]
    fn gf8_reconstruct_into_fills_only_the_holes() {
        roundtrip_into(&Gf8Coder::default(), 16, 4, 256, &[0, 7], &[1]);
        roundtrip_into(&Gf8Coder::default(), 4, 2, 16, &[2], &[1]);
    }

    #[test]
    fn reconstruct_into_rejects_bad_shapes() {
        let mut buf = [0u8; 4 * 8];
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        let have = [true, true, false, false];
        assert!(Gf16Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[])
            .is_err());
        // Indices 2 and 3 are outside declared `M = 2`.
        let parity = [0u8; 8];
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf16Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[(2, &parity), (3, &parity)])
            .is_err());
        let short = [0u8; 6];
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf8Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[(0, &short), (1, &parity)])
            .is_err());
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf8Coder::default()
            .reconstruct_into(2, &mut slots, &[true; 3], &[(0, &parity)])
            .is_err());
    }

    #[test]
    fn gf8_recovers_within_budget() {
        // Lose 2 data + 2 recovery: exactly the `m = 4` budget.
        roundtrip(&Gf8Coder::default(), 16, 4, 256, &[0, 7, 16, 19]);
    }

    #[test]
    fn gf16_recovers_within_budget() {
        roundtrip(&Gf16Coder::default(), 16, 4, 256, &[1, 9, 17, 18]);
    }

    #[test]
    fn gf8_too_much_loss_errors() {
        let data: Vec<Vec<u8>> = (0..8).map(|_| vec![0u8; 64]).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let recovery = Gf8Coder::default().encode(&refs, 2).unwrap();
        let mut received: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(recovery.into_iter().map(Some))
            .collect();
        // Three losses, `m = 2` — unrecoverable.
        received[0] = None;
        received[1] = None;
        received[2] = None;
        assert!(Gf16Coder::default().scheme() == FecScheme::Gf16);
        let err = Gf8Coder::default().reconstruct(8, 2, &mut received);
        assert!(err.is_err());
    }

    #[test]
    fn reconstruct_rejects_wrong_received_length() {
        // Length 3 vs K+M=4 must error, not panic on a recovery-slice index.
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf16Coder::default().reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf8Coder::default().reconstruct(2, 2, &mut recv).is_err());
    }

    #[test]
    fn reconstruct_rejects_mismatched_shard_lengths() {
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf16Coder::default().reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf8Coder::default().reconstruct(2, 2, &mut recv).is_err());
    }
}
