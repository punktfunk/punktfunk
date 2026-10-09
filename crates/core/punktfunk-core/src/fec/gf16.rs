//! GF(2¹⁶) Leopard-RS backend (`reed-solomon-simd`). SIMD, O(n log n), up to
//! 65535 shards/block. Shard length must be even.

use super::{
    validate_block_shape, validate_encode_shape, validate_into_shape, ErasureCoder, FecError,
};
use crate::config::FecScheme;
use reed_solomon_simd::ReedSolomonEncoder;
use std::sync::Mutex;

#[derive(Default)]
pub struct Gf16Coder {
    /// Cached encoder. `reset()` reshapes per block and reuses working space.
    /// `Mutex` keeps the `&self` trait surface; the session's one send thread
    /// holds it uncontended.
    enc: Mutex<Option<ReedSolomonEncoder>>,
}

impl ErasureCoder for Gf16Coder {
    fn scheme(&self) -> FecScheme {
        FecScheme::Gf16
    }

    fn encode(&self, data: &[&[u8]], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError> {
        let mut out = Vec::new();
        self.encode_into(data, recovery_count, &mut out)?;
        Ok(out)
    }

    fn encode_into(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), FecError> {
        if recovery_count == 0 {
            out.clear();
            return Ok(());
        }
        validate_encode_shape(data)?;
        let k = data.len();
        let shard_len = data[0].len();
        if shard_len % 2 != 0 {
            return Err(FecError::Config("GF(2^16) shard length must be even"));
        }
        let mut guard = self.enc.lock().unwrap_or_else(|p| p.into_inner());
        let enc = match guard.as_mut() {
            Some(enc) => {
                enc.reset(k, recovery_count, shard_len)
                    .map_err(|_| FecError::Backend("gf16 encoder reset"))?;
                enc
            }
            None => guard.insert(
                ReedSolomonEncoder::new(k, recovery_count, shard_len)
                    .map_err(|_| FecError::Backend("gf16 encoder init"))?,
            ),
        };
        for shard in data {
            enc.add_original_shard(shard)
                .map_err(|_| FecError::Backend("gf16 add shard"))?;
        }
        let result = enc.encode().map_err(|_| FecError::Backend("gf16 encode"))?;
        // Reuse pooled `Vec`s (`clear` keeps capacity); grow once to high-water M.
        out.truncate(recovery_count);
        let mut parity = result.recovery_iter();
        for buf in out.iter_mut() {
            let shard = parity
                .next()
                .ok_or(FecError::Backend("gf16 parity count"))?;
            buf.clear();
            buf.extend_from_slice(shard);
        }
        for shard in parity {
            out.push(shard.to_vec());
        }
        if out.len() != recovery_count {
            return Err(FecError::Backend("gf16 parity count"));
        }
        Ok(())
    }

    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, FecError> {
        validate_block_shape(received, data_count, recovery_count)?;
        let present = received.iter().filter(|s| s.is_some()).count();
        if present < data_count {
            return Err(FecError::TooFewShards {
                have: present,
                need: data_count,
            });
        }
        let originals_complete = received[..data_count].iter().all(|s| s.is_some());
        if recovery_count == 0 || originals_complete {
            let mut out = Vec::with_capacity(data_count);
            for slot in received.iter().take(data_count) {
                out.push(slot.clone().ok_or(FecError::TooFewShards {
                    have: present,
                    need: data_count,
                })?);
            }
            return Ok(out);
        }

        let original_in: Vec<(usize, &[u8])> = received[..data_count]
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_deref().map(|b| (i, b)))
            .collect();
        let recovery_in: Vec<(usize, &[u8])> = received[data_count..data_count + recovery_count]
            .iter()
            .enumerate()
            .filter_map(|(j, s)| s.as_deref().map(|b| (j, b)))
            .collect();

        let restored =
            reed_solomon_simd::decode(data_count, recovery_count, original_in, recovery_in)
                .map_err(|_| FecError::Backend("gf16 decode"))?;

        let mut out: Vec<Vec<u8>> = Vec::with_capacity(data_count);
        for (i, slot) in received[..data_count].iter().enumerate() {
            if let Some(s) = slot {
                out.push(s.clone());
            } else if let Some(s) = restored.get(&i) {
                out.push(s.clone());
            } else {
                return Err(FecError::Backend("gf16 decode left an original missing"));
            }
        }
        Ok(out)
    }

    fn reconstruct_into(
        &self,
        recovery_count: usize,
        data: &mut [&mut [u8]],
        have: &[bool],
        recovery: &[(usize, &[u8])],
    ) -> Result<(), FecError> {
        validate_into_shape(data, have, recovery, recovery_count)?;
        if have.iter().all(|h| *h) {
            return Ok(());
        }
        if data[0].len() % 2 != 0 {
            return Err(FecError::Config("GF(2^16) shard length must be even"));
        }
        let data_count = data.len();
        // Indexed refs into the caller's slots; decoder returns owned shards,
        // so these borrows end before the write-back below.
        let original_in: Vec<(usize, &[u8])> = data
            .iter()
            .zip(have)
            .enumerate()
            .filter(|&(_, (_, &h))| h)
            .map(|(i, (s, _))| (i, &**s))
            .collect();
        let restored = reed_solomon_simd::decode(
            data_count,
            recovery_count,
            original_in,
            recovery.iter().copied(),
        )
        .map_err(|_| FecError::Backend("gf16 decode"))?;
        for (i, h) in have.iter().enumerate() {
            if !*h {
                let shard = restored
                    .get(&i)
                    .ok_or(FecError::Backend("gf16 decode left an original missing"))?;
                data[i].copy_from_slice(shard);
            }
        }
        Ok(())
    }
}
