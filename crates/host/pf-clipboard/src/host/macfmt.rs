//! NSPasteboard types ↔ wire MIMEs, and the change-count ledger that tells the user's copies
//! from ours. No AppKit, so its tests also run off macOS; [`super::macos`] is the only
//! production caller.

use super::{WIRE_GIF, WIRE_HTML, WIRE_JPEG, WIRE_PNG, WIRE_RTF, WIRE_TEXT};

/// The pasteboard type each wire MIME travels as. AppKit's legacy aliases (`NSStringPboardType`,
/// `Apple PNG pasteboard type`) are never listed: it adds them beside these.
pub const KINDS: [(&str, &str); 6] = [
    (WIRE_TEXT, "public.utf8-plain-text"),
    (WIRE_HTML, "public.html"),
    (WIRE_RTF, "public.rtf"),
    (WIRE_PNG, "public.png"),
    (WIRE_JPEG, "public.jpeg"),
    (WIRE_GIF, "com.compuserve.gif"),
];

/// What `NSImage` writes, and what AppKit adds beside every image type (JPEG, GIF, HEIC, BMP).
/// A source with a TIFF offers PNG by conversion.
pub const TIFF: &str = "public.tiff";

/// Password managers mark a secret copy with these (nspasteboard.org). The Apple client drops
/// such a copy too, so a secret never crosses the wire from either side.
const SECRET_MARKERS: [&str; 2] = [
    "org.nspasteboard.ConcealedType",
    "org.nspasteboard.TransientType",
];

pub fn type_for(wire: &str) -> Option<&'static str> {
    KINDS.iter().find(|k| k.0 == wire).map(|k| k.1)
}

/// The wire kinds `types` offers, in [`KINDS`] order. A secret copy offers none.
pub fn wires_for(types: &[String]) -> Vec<&'static str> {
    let has = |t: &str| types.iter().any(|x| x == t);
    if SECRET_MARKERS.iter().any(|m| has(m)) {
        return Vec::new();
    }
    KINDS
        .iter()
        .filter(|(wire, uti)| has(uti) || (*wire == WIRE_PNG && has(TIFF)))
        .map(|k| k.0)
        .collect()
}

/// Tells our writes from other apps' by the change count alone. Every write bumps the count
/// once and returns the new value, so a count that matches our last write is still ours.
#[derive(Debug)]
pub struct Ledger {
    seen: isize,
    ours: Option<isize>,
}

impl Ledger {
    pub fn new(count: isize) -> Self {
        Ledger {
            seen: count,
            ours: None,
        }
    }

    /// True once per write by anyone else.
    pub fn moved(&mut self, count: isize) -> bool {
        if count == self.seen {
            return false;
        }
        self.seen = count;
        self.ours = None;
        true
    }

    /// No write by anyone since the last [`Self::moved`].
    pub fn unchanged(&self, count: isize) -> bool {
        count == self.seen
    }

    /// Our write returned `count`.
    pub fn wrote(&mut self, count: isize) {
        self.seen = count;
        self.ours = Some(count);
    }

    /// Our clear returned `count`.
    pub fn cleared(&mut self, count: isize) {
        self.seen = count;
        self.ours = None;
    }

    /// The pasteboard still holds what we wrote.
    pub fn is_ours(&self, count: isize) -> bool {
        self.ours == Some(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(types: &[&str]) -> Vec<String> {
        types.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn every_kind_maps_both_ways() {
        for (wire, uti) in KINDS {
            assert_eq!(type_for(wire), Some(uti));
            assert_eq!(wires_for(&s(&[uti])), vec![wire]);
        }
        assert_eq!(type_for("image/webp"), None);
    }

    #[test]
    fn legacy_aliases_collapse_and_unknown_types_drop() {
        // A text + HTML copy as AppKit lists it, plus a type we do not carry.
        let types = s(&[
            "public.utf8-plain-text",
            "NSStringPboardType",
            "public.html",
            "Apple HTML pasteboard type",
            "com.apple.webarchive",
        ]);
        assert_eq!(wires_for(&types), vec![WIRE_TEXT, WIRE_HTML]);
        assert!(wires_for(&s(&["public.file-url"])).is_empty());
    }

    #[test]
    fn a_secret_copy_offers_nothing() {
        for marker in SECRET_MARKERS {
            assert!(wires_for(&s(&["public.utf8-plain-text", marker])).is_empty());
        }
    }

    #[test]
    fn a_tiff_alone_offers_png() {
        assert_eq!(wires_for(&s(&[TIFF])), vec![WIRE_PNG]);
        assert_eq!(wires_for(&s(&["public.png", TIFF])), vec![WIRE_PNG]);
    }

    #[test]
    fn our_write_is_not_a_move_and_theirs_is() {
        let mut l = Ledger::new(4);
        assert!(!l.moved(4));
        l.wrote(5);
        assert!(l.is_ours(5));
        assert!(!l.moved(5), "our own write must not echo");
        assert!(l.unchanged(5));
        assert!(l.moved(6), "another app's write is a move");
        assert!(!l.is_ours(6));
        assert!(!l.moved(6));
    }

    #[test]
    fn a_clear_gives_up_ownership_without_an_echo() {
        let mut l = Ledger::new(0);
        l.wrote(1);
        l.cleared(2);
        assert!(!l.is_ours(2));
        assert!(!l.moved(2));
    }

    #[test]
    fn a_write_after_ours_is_not_ours() {
        let mut l = Ledger::new(0);
        l.wrote(1);
        assert!(!l.unchanged(2), "a copy that landed before the next look");
        assert!(!l.is_ours(2));
    }
}
