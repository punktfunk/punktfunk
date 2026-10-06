//! What this host reads of the seat contract, in one place.
//!
//! The seat supervisor (`pf-seats`, inside the Windows service) runs one ordinary
//! host per Windows session and marks each with `PUNKTFUNK_SEAT_SESSION=1` plus a
//! `PUNKTFUNK_SEAT_ID`. The host never learns how those sessions come to exist.
//! An unset marker is the console host, which behaves exactly as before.
//!
//! The id is untrusted process input, so it is validated once here rather than
//! at each use: it reaches a device-parameter marker and log lines.
//! `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.

/// Whether a supervisor-managed seat owns this host rather than the console.
pub fn is_seat_host() -> bool {
    std::env::var("PUNKTFUNK_SEAT_SESSION").as_deref() == Ok("1")
}

/// 32 lowercase hexadecimal characters, so the id is safe as a device-parameter
/// marker and in a log line. Any other value is a supervisor bug, not a seat.
pub fn validate_seat_id(raw: &str) -> Result<&str, &'static str> {
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
pub fn seat_id() -> Result<Option<String>, &'static str> {
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

/// The box's config dir a seat host reads its trust from (`PUNKTFUNK_TRUST_DIR`): the
/// identity, the pairing store, `profiles.json` and the device display overlays. Read only;
/// the box host is their one writer. `None` on the box host, and for a relative path.
pub fn trust_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_TRUST_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// The box's config dir a Windows seat host reads the library from (`PUNKTFUNK_LIBRARY_DIR`):
/// `library*.json`, `library-metadata/`, and the plugin manifests and grants an `exec` entry
/// resolves against. Read only; the box host is their one writer. `None` on every other host,
/// and for a relative path.
pub fn box_library_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_LIBRARY_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// Where this host reads its library: [`box_library_dir`], else its own config dir. Play stats
/// are never here; they stay in [`crate::config_dir`].
pub fn library_dir() -> std::path::PathBuf {
    box_library_dir().unwrap_or_else(crate::config_dir)
}

/// `PUNKTFUNK_PAIRING=refused`: devices pair with the box, never with this host. A knock is
/// refused, a PIN window never opens.
pub fn pairing_refused() -> bool {
    std::env::var("PUNKTFUNK_PAIRING").as_deref() == Ok("refused")
}

/// `PUNKTFUNK_SEAT_OWNER=1`: this seat host is the box owner's own, behind the door. It serves
/// the owner profile and the light-seat profiles that play inside the owner's host.
pub fn is_owner_seat() -> bool {
    is_seat_host() && std::env::var("PUNKTFUNK_SEAT_OWNER").as_deref() == Ok("1")
}

static DOOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as the door (`serve --door`).
pub fn set_door() {
    DOOR.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this host is the door: it advertises, pairs, serves the console and places every
/// connect on a seat, and never streams itself. `serve --door` or `PUNKTFUNK_DOOR=1`.
pub fn is_door() -> bool {
    DOOR.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("PUNKTFUNK_DOOR").as_deref() == Ok("1")
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
