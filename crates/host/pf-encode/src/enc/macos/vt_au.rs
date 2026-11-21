//! The access unit VideoToolbox hands back, as the host sends it: VideoToolbox writes each NAL
//! unit behind a big-endian length and keeps the parameter sets in the format description; the
//! wire carries Annex-B with the parameter sets in-band on every keyframe.
//!
//! Pure, so its table runs on every target.

use anyhow::{ensure, Result};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Annex-B for one access unit: each of `head` (parameter sets, then SEI, as bare NAL units),
/// then each NAL unit of `sample`, whose units sit behind `len_size`-byte lengths.
pub(crate) fn annex_b(head: &[&[u8]], sample: &[u8], len_size: usize) -> Result<Vec<u8>> {
    ensure!(
        (1..=4).contains(&len_size),
        "NAL length field of {len_size} bytes"
    );
    let head_len: usize = head.iter().map(|n| n.len() + START_CODE.len()).sum();
    let mut out = Vec::with_capacity(head_len + sample.len() + 16);
    for nal in head {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    let mut rest = sample;
    while !rest.is_empty() {
        ensure!(rest.len() >= len_size, "NAL length cut short");
        let (len, body) = rest.split_at(len_size);
        let len = len.iter().fold(0usize, |n, &b| n << 8 | usize::from(b));
        ensure!(
            len > 0 && len <= body.len(),
            "NAL of {len} bytes past the sample end"
        );
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&body[..len]);
        rest = &body[len..];
    }
    ensure!(out.len() > head_len, "sample holds no NAL unit");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_table() {
        let sps = [0x67u8, 0xAA];
        let pps = [0x68u8, 0xBB];
        type Row<'a> = (&'a str, Vec<&'a [u8]>, Vec<u8>, usize, Option<Vec<u8>>);
        let rows: Vec<Row> = vec![
            (
                "a keyframe gains its parameter sets ahead of the slice",
                vec![&sps, &pps],
                vec![0, 0, 0, 2, 0x65, 0xCC],
                4,
                Some(vec![
                    0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC,
                ]),
            ),
            (
                "two units behind two-byte lengths",
                vec![],
                vec![0, 1, 0x41, 0, 2, 0x06, 0x05],
                2,
                Some(vec![0, 0, 0, 1, 0x41, 0, 0, 0, 1, 0x06, 0x05]),
            ),
            (
                "a length past the end is refused",
                vec![],
                vec![0, 0, 0, 9, 0x41],
                4,
                None,
            ),
            (
                "a zero length is refused",
                vec![],
                vec![0, 0, 0, 0],
                4,
                None,
            ),
            ("a cut length is refused", vec![], vec![0, 0], 4, None),
            (
                "parameter sets alone are no access unit",
                vec![&sps],
                vec![],
                4,
                None,
            ),
            (
                "a length field wider than four bytes is refused",
                vec![],
                vec![0; 6],
                5,
                None,
            ),
        ];
        for (name, head, sample, len_size, want) in rows {
            assert_eq!(annex_b(&head, &sample, len_size).ok(), want, "{name}");
        }
    }
}
