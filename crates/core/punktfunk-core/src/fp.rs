//! Certificate fingerprint spelling shared by every Rust client.

/// The 32-byte fingerprint as 64 lowercase hex digits: the spelling every store, advert and
/// host log uses.
pub fn hex(fp: &[u8; 32]) -> String {
    use std::fmt::Write;
    fp.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A device's code: the first 16 hex digits of its fingerprint, upper case, in fours. A device
/// waiting for approval and the host's console both show it, for a person to compare.
pub fn device_code(fp: &[u8; 32]) -> String {
    let hex = hex(fp).to_uppercase();
    [&hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16]].join(" ")
}

/// A hex fingerprint in fours, eight groups to a line: two lines for 64 digits, to compare
/// against a host's log.
pub fn grouped(fp_hex: &str) -> String {
    let groups: Vec<&str> = fp_hex
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    groups
        .chunks(8)
        .map(|line| line.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 64 hex digits, either case, into the 32-byte SHA-256 fingerprint. `None` on any
/// other length or a non-hex byte, multibyte UTF-8 included: an mDNS `fp` is peer text.
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let digit = |c: u8| char::from(c).to_digit(16);
    let mut out = [0u8; 32];
    for (b, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
        *b = ((digit(pair[0])? << 4) | digit(pair[1])?) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{device_code, hex, parse_hex32};

    /// The vector every client's own spelling of the code is tested against.
    #[test]
    fn the_device_code_is_sixteen_digits_in_fours() {
        let mut fp = [0u8; 32];
        fp[..8].copy_from_slice(&[0x4f, 0x2a, 0x91, 0xc3, 0x07, 0xbe, 0xd5, 0x12]);
        assert_eq!(device_code(&fp), "4F2A 91C3 07BE D512");
    }

    #[test]
    fn parses_either_case_and_refuses_everything_else() {
        let want: [u8; 32] = std::array::from_fn(|i| i as u8);
        let lower = hex(&want);
        assert_eq!(&lower[..6], "000102");
        assert_eq!(parse_hex32(&lower), Some(want));
        assert_eq!(parse_hex32(&lower.to_uppercase()), Some(want));
        assert_eq!(parse_hex32(&lower[..62]), None);
        assert_eq!(parse_hex32(&format!("+{}", &lower[1..])), None);
        assert_eq!(parse_hex32(&format!("{}g", &lower[..63])), None);
    }

    /// 64 bytes with a 3-byte char straddling a digit pair: a `&s[i..i + 2]` slice panics here.
    #[test]
    fn a_multibyte_char_is_refused_not_a_panic() {
        let s = format!("a€{}", "a".repeat(60));
        assert_eq!(s.len(), 64);
        assert_eq!(parse_hex32(&s), None);
    }
}
