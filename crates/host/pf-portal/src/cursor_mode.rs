//! ScreenCast cursor mode against `AvailableCursorModes`.
//!
//! `SelectSources` with a bit the portal did not advertise is
//! `INVALID_ARGUMENT` from xdg-desktop-portal, not the backend. [`pick`]
//! always returns a subset of the advertised bits.
//!
//! One ladder for every portal cast: `negotiate_cursor_mode` (Linux) reads the
//! advertised bits and `PUNKTFUNK_PORTAL_CURSOR_MODE`, then calls [`pick`].
//! Pure and platform-neutral so the tests run without a compositor.

/// Portal wire bits. `pf_vdisplay::PortalCursorMode` re-exports this; the host
/// reads it to know whether `SPA_META_Cursor` can arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hidden = 1,
    /// Pointer burnt into the frames; no `SPA_META_Cursor`.
    Embedded = 2,
    /// `SPA_META_Cursor` beside the frames; the compositor keeps its hardware plane.
    Metadata = 4,
}

impl Mode {
    pub const fn bit(self) -> u32 {
        self as u32
    }

    /// Spelling for logs and `PUNKTFUNK_PORTAL_CURSOR_MODE`.
    pub const fn name(self) -> &'static str {
        match self {
            Mode::Hidden => "hidden",
            Mode::Embedded => "embedded",
            Mode::Metadata => "metadata",
        }
    }

    /// Under `Embedded` a missing overlay is not "pointer off the recorded view".
    pub const fn delivers_metadata(self) -> bool {
        matches!(self, Mode::Metadata)
    }

    const fn fallbacks(self) -> [Mode; 2] {
        match self {
            // Embedded still shows a pointer (burnt in). Hidden last: no pointer,
            // and no `SPA_META_Cursor` for a cursor-forward client to draw.
            Mode::Metadata => [Mode::Embedded, Mode::Hidden],
            // CPU capture composites `SPA_META_Cursor` inline, so Metadata still shows a pointer.
            Mode::Embedded => [Mode::Metadata, Mode::Hidden],
            // Either remaining mode shows a pointer. Prefer Embedded: this path is
            // not set up to draw `SPA_META_Cursor`.
            Mode::Hidden => [Mode::Embedded, Mode::Metadata],
        }
    }
}

/// Requested mode, plus the original want when that is a downgrade (the caller logs the gap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// `SelectSources` mode. In `advertised`, unless the backend advertised nothing we know.
    pub mode: Mode,
    pub wanted: Option<Mode>,
}

/// `Hidden` only when advertised. Nothing known falls to `Embedded` and sets `wanted`.
pub fn pick(advertised: u32, want: Mode) -> Choice {
    if advertised & want.bit() != 0 {
        return Choice {
            mode: want,
            wanted: None,
        };
    }
    for alt in want.fallbacks() {
        if advertised & alt.bit() != 0 {
            return Choice {
                mode: alt,
                wanted: Some(want),
            };
        }
    }
    // Advertised nothing this build knows (0, or bits from a newer spec). Every
    // backend implements Embedded, and it still shows a pointer. The caller warns.
    Choice {
        mode: Mode::Embedded,
        wanted: Some(want),
    }
}

/// Parsed `PUNKTFUNK_PORTAL_CURSOR_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    /// Unset or `auto`. The session's own negotiation decides.
    Auto,
    /// Prefer this mode. The ladder still runs; a pin cannot request an unadvertised mode.
    Mode(Mode),
    /// Unknown spelling. Treated as `Auto`; the caller logs it rather than swallowing a typo.
    Unrecognised,
}

pub fn parse_pin(raw: &str) -> Pin {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Pin::Auto,
        "hidden" | "none" => Pin::Mode(Mode::Hidden),
        "embedded" | "composited" => Pin::Mode(Mode::Embedded),
        "metadata" | "meta" => Pin::Mode(Mode::Metadata),
        _ => Pin::Unrecognised,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Portal wire bits. A rejecting backend prints this number.
    #[test]
    fn mode_bits_are_the_portal_wire_values() {
        assert_eq!(Mode::Hidden.bit(), 1);
        assert_eq!(Mode::Embedded.bit(), 2);
        assert_eq!(Mode::Metadata.bit(), 4);
    }

    /// xdph advertises `3` (`Hidden|Embedded`). Metadata (`4`) is
    /// `INVALID_ARGUMENT` from xdg-desktop-portal, not the backend.
    #[test]
    fn metadata_wanted_but_unadvertised_downgrades_to_embedded() {
        assert_eq!(Mode::Hidden.bit() | Mode::Embedded.bit(), 3);
        let c = pick(3, Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// Under Embedded no `SPA_META_Cursor` arrives. A missing overlay is not "pointer off view".
    #[test]
    fn only_metadata_can_deliver_a_cursor_overlay() {
        assert!(Mode::Metadata.delivers_metadata());
        assert!(!Mode::Embedded.delivers_metadata());
        assert!(!Mode::Hidden.delivers_metadata());
        // The negotiated mode governs, not the wanted one.
        assert!(!pick(3, Mode::Metadata).mode.delivers_metadata());
    }

    #[test]
    fn embedded_wanted_and_advertised_is_untouched() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, None);
    }

    #[test]
    fn metadata_is_used_where_advertised() {
        let all = Mode::Hidden.bit() | Mode::Embedded.bit() | Mode::Metadata.bit();
        let c = pick(all, Mode::Metadata);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, None);
    }

    /// CPU capture composites `SPA_META_Cursor`, so Metadata still shows a pointer.
    #[test]
    fn embedded_unadvertised_falls_to_metadata_not_hidden() {
        let c = pick(Mode::Hidden.bit() | Mode::Metadata.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, Some(Mode::Embedded));
    }

    /// Hidden-only backend: requesting Metadata would close the session.
    #[test]
    fn hidden_only_backend_yields_hidden() {
        let c = pick(Mode::Hidden.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Hidden);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// Nothing known (a portal still starting reads 0): Embedded, flagged so the caller warns.
    #[test]
    fn unknown_advertisement_guesses_embedded_and_reports_a_downgrade() {
        for advertised in [0, 0b1000_0000] {
            let c = pick(advertised, Mode::Metadata);
            assert_eq!(c.mode, Mode::Embedded);
            assert_eq!(c.wanted, Some(Mode::Metadata));
        }
    }

    #[test]
    fn never_requests_an_unadvertised_mode() {
        let modes = [Mode::Hidden, Mode::Embedded, Mode::Metadata];
        for advertised in 1u32..=0b111 {
            for want in modes {
                let c = pick(advertised, want);
                assert!(
                    advertised & c.mode.bit() != 0,
                    "picked {} from advertised {advertised:#05b} (want {})",
                    c.mode.name(),
                    want.name()
                );
                assert_eq!(c.wanted.is_some(), c.mode != want);
            }
        }
    }

    #[test]
    fn pin_parses_the_spellings_we_document() {
        assert_eq!(parse_pin(""), Pin::Auto);
        assert_eq!(parse_pin("auto"), Pin::Auto);
        assert_eq!(parse_pin(" AUTO "), Pin::Auto);
        assert_eq!(parse_pin("embedded"), Pin::Mode(Mode::Embedded));
        assert_eq!(parse_pin("Embedded"), Pin::Mode(Mode::Embedded));
        assert_eq!(parse_pin("metadata"), Pin::Mode(Mode::Metadata));
        assert_eq!(parse_pin("hidden"), Pin::Mode(Mode::Hidden));
        assert_eq!(parse_pin("2"), Pin::Unrecognised);
        assert_eq!(parse_pin("yes"), Pin::Unrecognised);
    }

    /// A pin is a preference: unadvertised Metadata still becomes Embedded.
    #[test]
    fn a_pin_still_runs_the_ladder() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
    }
}
