//! The VPS/SPS/PPS run at the head of an H.264 or HEVC access unit. Media Foundation caches
//! the first IDR's run and prepends it to a later IDR an MFT emits without one.

use crate::Codec;

/// H.264 SPS/PPS or HEVC VPS/SPS/PPS.
fn is_parameter_set(codec: Codec, nal: u8) -> bool {
    match codec {
        Codec::H264 => matches!(nal, 7 | 8),
        _ => matches!(nal, 32..=34),
    }
}

/// The AU's VPS/SPS/PPS run, or `None` when it carries none. An access-unit delimiter or
/// SEI ahead of the run is skipped rather than treated as its absence — that is the shape
/// Qualcomm's MFT emits, and it publishes the sequence header late and HEVC in-band only,
/// so an IDR without one is undecodable unless we prepend the cached run ourselves.
pub fn parameter_set_prefix(codec: Codec, au: &[u8]) -> Option<&[u8]> {
    let mut run: Option<usize> = None;
    let mut i = 2;
    while i + 1 < au.len() {
        if au[i - 2..i + 1] != [0, 0, 1] {
            i += 1;
            continue;
        }
        // `i` indexes the start code's last byte; the 4-byte form owns the zero before it.
        let header = i + 1;
        let code_start = if i >= 3 && au[i - 3] == 0 {
            i - 3
        } else {
            i - 2
        };
        let nal = match codec {
            Codec::H264 => au[header] & 0x1f,
            _ => (au[header] >> 1) & 0x3f,
        };
        match (is_parameter_set(codec, nal), run) {
            (true, None) => run = Some(code_start),
            (false, Some(start)) => return Some(&au[start..code_start]),
            _ => {}
        }
        i = header + 1;
    }
    run.map(|start| &au[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_classification_matches_the_two_codecs() {
        assert!(is_parameter_set(Codec::H264, 8));
        assert!(!is_parameter_set(Codec::H264, 5));
        assert!(is_parameter_set(Codec::H265, 34));
        assert!(!is_parameter_set(Codec::H265, 19));
    }

    /// The cached prefix must end exactly at the first slice NAL, start-code included.
    #[test]
    fn parameter_set_prefix_cuts_at_the_first_slice() {
        // SPS (0x67) + PPS (0x68) + IDR slice (0x65).
        let au = [
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC,
        ];
        let prefix = parameter_set_prefix(Codec::H264, &au).expect("SPS-led AU has a prefix");
        assert_eq!(prefix, &au[..12]);
        // An AU that opens with the slice has no prefix to cache.
        assert!(parameter_set_prefix(Codec::H264, &au[12..]).is_none());
        // Parameter sets with no slice after them are all prefix.
        assert_eq!(
            parameter_set_prefix(Codec::H264, &au[..12]).expect("all parameter sets"),
            &au[..12]
        );
    }

    /// An MFT that opens the AU with a delimiter still gets its header cached: the
    /// Qualcomm shape.
    #[test]
    fn parameter_set_prefix_skips_a_leading_delimiter() {
        // AUD (0x09) + SPS (0x67) + PPS (0x68) + IDR slice (0x65).
        let au = [
            0, 0, 0, 1, 0x09, 0x10, //
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC,
        ];
        let prefix =
            parameter_set_prefix(Codec::H264, &au).expect("delimiter-led AU still has a prefix");
        assert_eq!(prefix, &au[6..18]);
        // Delimiter-led, parameter sets running to the end of the AU.
        assert_eq!(
            parameter_set_prefix(Codec::H264, &au[..18]).expect("delimiter then parameter sets"),
            &au[6..18]
        );
    }
}
