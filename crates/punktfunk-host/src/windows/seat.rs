//! What this host reads of the seat contract, in one place.
//!
//! The multi-seat add-on (`unom/punktfunk-seats`) supervises one ordinary host
//! per Windows session and marks each with `PUNKTFUNK_SEAT_SESSION=1` plus a
//! `PUNKTFUNK_SEAT_ID`. The host never learns how those sessions come to exist.
//! An unset marker is the console host, which behaves exactly as before.
//!
//! The id is untrusted process input, so it is validated once here rather than
//! at each use: it reaches a device-parameter marker and log lines.
//! `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.

/// Whether an add-on-managed seat owns this host rather than the console.
pub(crate) fn is_seat_host() -> bool {
    std::env::var("PUNKTFUNK_SEAT_SESSION").as_deref() == Ok("1")
}

/// 32 lowercase hexadecimal characters, so the id is safe as a device-parameter
/// marker and in a log line. Any other value is a supervisor bug, not a seat.
pub(crate) fn validate_seat_id(raw: &str) -> Result<&str, &'static str> {
    let valid = raw.len() == SEAT_ID_LEN
        && raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    valid
        .then_some(raw)
        .ok_or("PUNKTFUNK_SEAT_ID must be 32 lowercase hexadecimal characters")
}

const SEAT_ID_LEN: usize = 32;

/// This process's validated seat id, or `None` for the console host.
/// The error is a rejected id, which callers surface rather than ignore.
pub(crate) fn seat_id() -> Result<Option<String>, &'static str> {
    let Some(raw) = std::env::var_os("PUNKTFUNK_SEAT_ID") else {
        // A seat without an id would mint audio devnodes the console host also matches.
        if is_seat_host() {
            return Err("PUNKTFUNK_SEAT_SESSION=1 without a PUNKTFUNK_SEAT_ID");
        }
        return Ok(None);
    };
    let text = raw
        .to_str()
        .ok_or("PUNKTFUNK_SEAT_ID must be valid Unicode")?;
    validate_seat_id(text).map(|id| Some(id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_ids_reject_unsafe_marker_and_log_text() {
        for raw in [
            "",
            "550E8400E29B41D4A716446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "seat 1",
            "seat/1",
            "seat\n1",
            "séat-1",
            "0123456789abcdefg123456789abcdef",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_seat_id(raw).is_err(), "{raw:?}");
        }
        assert_eq!(
            validate_seat_id("0123456789abcdef0123456789abcdef"),
            Ok("0123456789abcdef0123456789abcdef")
        );
    }
}
