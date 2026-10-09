//! GF(2⁸) Cauchy Reed–Solomon (`fec-rs`). Moonlight `nanors` parity.
//!
//! Generator `M[j][i] = inv[(m+i)^j]` over poly 0x1d. Vandermonde RS is not
//! interoperable — a stock client cannot recover that parity. Hard ceiling:
//! data + recovery ≤ 255 shards/block.
//!
//! Pin with `nanors_exact_parity_vectors`. The codec cache is keyed by `(k, m)`
//! because `ReedSolomon::new` rebuilds the generator on a miss.

use super::{
    validate_block_shape, validate_encode_shape, validate_into_shape, ErasureCoder, FecError,
};
use crate::config::FecScheme;
use fec_rs::ReedSolomon;
use std::sync::Mutex;

#[derive(Default)]
pub struct Gf8Coder {
    /// Last Cauchy codec, keyed by `(k, m)`. `ReedSolomon::new` rebuilds the
    /// generator; video holds one shape across frames. `Mutex` is the `&self`
    /// trait surface — uncontended on the one send thread.
    rs: Mutex<Option<(usize, usize, ReedSolomon)>>,
}

impl ErasureCoder for Gf8Coder {
    fn scheme(&self) -> FecScheme {
        FecScheme::Gf8
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
        let mut guard = self.rs.lock().unwrap_or_else(|p| p.into_inner());
        let cached = matches!(&*guard, Some((ck, cm, _)) if *ck == k && *cm == recovery_count);
        if !cached {
            let rs = ReedSolomon::new(k, recovery_count)
                .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
            *guard = Some((k, recovery_count, rs));
        }
        let rs = &guard.as_ref().expect("cache populated above").2;
        // `encode_sep` overwrites every parity row on the first pass; stale
        // bytes never survive, so skip a zero-fill.
        out.truncate(recovery_count);
        for buf in out.iter_mut() {
            buf.resize(shard_len, 0);
        }
        while out.len() < recovery_count {
            out.push(vec![0u8; shard_len]);
        }
        // `encode_sep` fills parity in place; `encode` would copy the data
        // shards into a scratch first.
        rs.encode_sep(data, out)
            .map_err(|_| FecError::Backend("gf8 encode"))?;
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
        if recovery_count == 0 {
            return collect_originals(received, data_count);
        }
        // Same `(k, m)` cache as `encode_into`. A miss rebuilds the generator
        // on the pump thread and drops the decode-matrix cache for a stable
        // loss pattern.
        let mut guard = self.rs.lock().unwrap_or_else(|p| p.into_inner());
        let cached =
            matches!(&*guard, Some((ck, cm, _)) if *ck == data_count && *cm == recovery_count);
        if !cached {
            let rs = ReedSolomon::new(data_count, recovery_count)
                .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
            *guard = Some((data_count, recovery_count, rs));
        }
        let rs = &guard.as_ref().expect("cache populated above").2;
        rs.reconstruct_data(received)
            .map_err(|_| FecError::Backend("gf8 reconstruct"))?;
        collect_originals(received, data_count)
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
        // fec-rs reconstructs through owned `Option<Vec<u8>>`. Copy present
        // shards in and recovered originals back into the caller's slots.
        let data_count = data.len();
        let mut received: Vec<Option<Vec<u8>>> = Vec::with_capacity(data_count + recovery_count);
        for (s, h) in data.iter().zip(have) {
            received.push(h.then(|| s.to_vec()));
        }
        received.resize(data_count + recovery_count, None);
        for &(j, bytes) in recovery {
            received[data_count + j] = Some(bytes.to_vec());
        }
        let mut guard = self.rs.lock().unwrap_or_else(|p| p.into_inner());
        let cached =
            matches!(&*guard, Some((ck, cm, _)) if *ck == data_count && *cm == recovery_count);
        if !cached {
            let rs = ReedSolomon::new(data_count, recovery_count)
                .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
            *guard = Some((data_count, recovery_count, rs));
        }
        let rs = &guard.as_ref().expect("cache populated above").2;
        rs.reconstruct_data(&mut received)
            .map_err(|_| FecError::Backend("gf8 reconstruct"))?;
        for (i, h) in have.iter().enumerate() {
            if !*h {
                let shard = received[i]
                    .as_ref()
                    .ok_or(FecError::Backend("reconstruction left an original missing"))?;
                data[i].copy_from_slice(shard);
            }
        }
        Ok(())
    }
}

fn collect_originals(
    received: &[Option<Vec<u8>>],
    data_count: usize,
) -> Result<Vec<Vec<u8>>, FecError> {
    let mut out = Vec::with_capacity(data_count);
    for slot in received.iter().take(data_count) {
        out.push(
            slot.clone()
                .ok_or(FecError::Backend("reconstruction left an original missing"))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-exact `nanors` Cauchy vectors (`M[j][i] = inv[(m+i)^j]`, poly 0x1d).
    /// A matrix switch would still encode, but Moonlight could not recover.
    #[test]
    fn nanors_exact_parity_vectors() {
        let coder = Gf8Coder::default();
        let data: [&[u8]; 4] = [&[10u8], &[20], &[30], &[40]];
        let parity = coder.encode(&data, 2).unwrap();
        assert_eq!(parity, vec![vec![136u8], vec![0u8]]);

        // Independent Cauchy rows: parity[j] = XOR_i M[j][i] · data[i].
        let rows = [[142u8, 244, 71, 167], [244, 142, 167, 71]];
        let din = [10u8, 20, 30, 40];
        for (j, row) in rows.iter().enumerate() {
            let expect = row
                .iter()
                .zip(din)
                .fold(0u8, |acc, (&m, d)| acc ^ gf_mul(m, d));
            assert_eq!(parity[j][0], expect, "parity row {j}");
        }
    }

    #[test]
    fn recovers_erased_data_shards() {
        let coder = Gf8Coder::default();
        let data: Vec<Vec<u8>> = (0..6).map(|i| vec![i as u8; 8]).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let parity = coder.encode(&refs, 3).unwrap();
        let mut received: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        // Three data erasures = the `m = 3` budget.
        received[1] = None;
        received[3] = None;
        received[5] = None;
        let recovered = coder.reconstruct(6, 3, &mut received).unwrap();
        assert_eq!(recovered, data);
    }

    /// GF(2⁸) multiply, reduction poly 0x1d. Independent of `fec-rs`.
    fn gf_mul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        for _ in 0..8 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= 0x1d;
            }
            b >>= 1;
        }
        p
    }
}
