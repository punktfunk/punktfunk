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
    use super::{hex, parse_hex32};

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
