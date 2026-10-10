//! Stdout line for xdg-desktop-portal-hyprland's custom picker.
//!
//! xdph has no headless source-selection API. It runs
//! `screencopy:custom_picker_binary` and parses one stdout line. The Hyprland
//! backend points that binary at a shim that cats a per-session file; this
//! module is the format of that file.
//!
//! No schema, no D-Bus error: a malformed line is a portal log line. Tests
//! transcribe xdph's parser (`ScreencopyShared.cpp` `[SELECTION]`). A missing
//! `/` or missing newline still yields a plausible output name; string-asserting
//! the line would miss that. Built on every platform so those tests run without
//! a compositor.

/// xdph splits on the first `/` into flags and selection. No `/` makes both
/// halves the whole payload (`npos + 1` wraps to 0): the output name still
/// parses while the flag loop walks `screen:<name>` one character at a time.
/// `screen` contains `r` (allow-restore-token); this line never sets it.
/// Empty flags are a leading `/`.
///
/// xdph `pop_back()`s the selection unconditionally. Without the trailing
/// newline the last character of the output name is eaten.
pub(crate) fn selection_line(name: &str) -> String {
    format!("[SELECTION]/screen:{name}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xdph picker parser (`ScreencopyShared.cpp`). `(flags, output)`, or
    /// `None` where xdph treats the stdout as no selection.
    ///
    /// Transcribed, not string-asserted: a missing `/` still yields the right
    /// output name while the flag loop walks the whole selection.
    fn xdph_parse(picker_stdout: &str) -> Option<(String, String)> {
        let marker = picker_stdout.find("[SELECTION]")?;
        let selection = &picker_stdout[marker + "[SELECTION]".len()..];
        // No `/`: C++ `substr(npos + 1)` unsigned-wraps; both halves are the whole string.
        let (flags, sel) = match selection.find('/') {
            Some(i) => (&selection[..i], &selection[i + 1..]),
            None => (selection, selection),
        };
        let name = sel.strip_prefix("screen:")?;
        // Unconditional `pop_back()`; the trailing newline is that character.
        let mut output = name.to_string();
        output.pop();
        Some((flags.to_string(), output))
    }

    #[test]
    fn the_line_carries_marker_empty_flags_separator_and_newline() {
        assert_eq!(selection_line("PF-1"), "[SELECTION]/screen:PF-1\n");
    }

    #[test]
    fn xdph_reads_our_line_as_an_output_with_no_flags() {
        for name in ["PF-1", "PF-1620-1", "HDMI-A-1", "DP-2"] {
            let (flags, output) = xdph_parse(&selection_line(name)).expect("xdph parses our line");
            assert_eq!(output, name, "xdph must recover the exact output name");
            assert_eq!(flags, "", "we ask for no flags at all");
            assert!(
                !flags.contains('r'),
                "an `r` in the flags makes xdph hand back restore_data + persist_mode=2 we never \
                 requested (Screencopy.cpp:261-267)"
            );
        }
    }

    /// Without `/`, flags include `r` from `screen` (allow-restore-token).
    #[test]
    fn without_the_separator_the_whole_selection_becomes_flags() {
        let (flags, output) = xdph_parse("[SELECTION]screen:PF-1620-1\n").expect("still parses");
        assert_eq!(
            output, "PF-1620-1",
            "the output name did survive — which is precisely why this hid for so long"
        );
        assert_eq!(
            flags, "screen:PF-1620-1\n",
            "…while the entire selection was handed to the flag loop"
        );
        assert!(
            flags.contains('r'),
            "the `r` of `sc*r*een` is xdph's allow-restore-token flag"
        );
    }

    #[test]
    fn the_trailing_newline_is_what_pop_back_consumes() {
        assert!(selection_line("PF-1620-1").ends_with('\n'));
        let (_, truncated) = xdph_parse("[SELECTION]/screen:PF-1620-1").expect("parses");
        assert_eq!(
            truncated, "PF-1620-",
            "pop_back() takes the last real character"
        );
    }

    /// No `[SELECTION]`: xdph prompts. The shim uses that when the file is empty.
    #[test]
    fn an_empty_read_is_not_a_selection() {
        assert!(xdph_parse("").is_none());
        assert!(xdph_parse("screen:PF-1\n").is_none());
    }
}
