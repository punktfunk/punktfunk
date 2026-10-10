//! Sanitize a client-supplied device name before storage, logs, or UI.
//!
//! The name arrives on the wire from an unpaired device. Strip C0/C1 and Unicode
//! bidi/format controls, collapse whitespace, trim, and cap length so a name
//! cannot inject a terminal or spoof a trusted device. Empty/all-control names
//! fall back to a fingerprint-derived label. [`is_spoofy_char`] is the shared set.

/// Strip controls and bidi marks; empty input becomes `device {fp[:8]}`.
pub(crate) fn sanitize_device_name(name: &str, fp_hex: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '\t' || c == '\n' { ' ' } else { c })
        .filter(|&c| !c.is_control() && !is_spoofy_char(c))
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = collapsed.as_str();
    while trimmed.len() > NAME_MAX {
        let mut cut = NAME_MAX;
        while !trimmed.is_char_boundary(cut) {
            cut -= 1;
        }
        trimmed = &trimmed[..cut];
    }
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        format!("device {}", &fp_hex[..8.min(fp_hex.len())])
    } else {
        trimmed.to_string()
    }
}

/// Bidi/format marks that can reorder a displayed name. Shared with the stream
/// marker so the set cannot drift. Callers also drop `char::is_control`.
pub(crate) fn is_spoofy_char(c: char) -> bool {
    ('\u{202A}'..='\u{202E}').contains(&c) // LRE..RLO/PDF
        || ('\u{2066}'..='\u{2069}').contains(&c) // LRI..PDI
        || c == '\u{200E}' // LRM
        || c == '\u{200F}' // RLM
        || c == '\u{061C}' // ALM
        || c == '\u{FEFF}' // BOM / ZWNBSP
}

/// Matches `quic::HELLO_NAME_MAX` on the `Hello` wire cap.
const NAME_MAX: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_and_bidi() {
        let dirty = "\u{1b}]0;evil\u{07}Good\nDevice\u{202E}xfp";
        let clean = sanitize_device_name(dirty, "deadbeef00");
        assert!(!clean.contains('\u{1b}') && !clean.contains('\n') && !clean.contains('\u{202E}'));
        // ESC/BEL/RLO dropped; `]` after ESC survives; `\n` becomes a space.
        assert_eq!(clean, "]0;evilGood Devicexfp");
        assert_eq!(
            sanitize_device_name("\u{1b}\u{07}", "deadbeef00"),
            "device deadbeef"
        );
        assert_eq!(sanitize_device_name("   ", "abc"), "device abc");
        assert!(sanitize_device_name(&"x".repeat(200), "ab").len() <= 64);
    }
}
