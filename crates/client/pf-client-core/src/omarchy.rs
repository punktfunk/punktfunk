//! The desktop's Omarchy theme, as four colours this client draws from.
//!
//! Omarchy stores the active theme at `~/.local/state/omarchy/current/theme/`.
//! Two files there carry colours:
//!
//! - `punktfunk.json` — our template, rendered on `omarchy-theme-set` after
//!   `punktfunk-omarchy setup`. Exact, host-side opt-in.
//! - `colors.toml` — the theme's native palette, present on every Omarchy box.
//!
//! File read, not an API: a non-Omarchy box has no directory, so every miss
//! is "no theme", never an error. JSON rules match
//! `web/server/util/omarchyTheme.ts`. GTK (`clients/linux/src/omarchy.rs`)
//! and the console (`pf-console-ui`) consume the same `OsTheme`.

pub use crate::rgb::{contrast, on_accent, readable, Rgb};

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`. Alpha is dropped: surfaces are solid.
///
/// Hex only. An unrendered template still holds literal `{{ accent }}`; this
/// refuses it.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let w = match h.len() {
        3 | 4 => 1,
        6 | 8 => 2,
        _ => return None,
    };
    let chan = |i: usize| -> Option<f64> {
        let raw = u8::from_str_radix(h.get(i * w..i * w + w)?, 16).ok()?;
        // One nibble repeats: `f` is `ff`, not `f0`.
        Some(f64::from(raw) * if w == 1 { 17.0 } else { 1.0 } / 255.0)
    };
    Some(Rgb(chan(0)?, chan(1)?, chan(2)?))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsTheme {
    pub dark: bool,
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
}

/// All three colours are required: half a palette is worse than none.
/// Unparsable `mode` is `dark`, matching the console.
pub fn parse_punktfunk_json(raw: &str) -> Option<OsTheme> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let colour = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .and_then(parse_hex)
    };
    Some(OsTheme {
        dark: v.get("mode").and_then(serde_json::Value::as_str) != Some("light"),
        bg: colour("background")?,
        fg: colour("foreground")?,
        accent: colour("accent")?,
    })
}

/// Flat `key = "value"` lines — no toml crate, nothing nested. Only the four
/// semantic keys; the ANSI ramp is the terminal's.
pub fn parse_colors_toml(raw: &str) -> Option<OsTheme> {
    let value = |key: &str| {
        raw.lines().find_map(|l| {
            let rest = l.trim().strip_prefix(key)?.trim_start();
            rest.strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .split('"')
                .next()
        })
    };
    Some(OsTheme {
        dark: value("mode") != Some("light"),
        bg: parse_hex(value("background")?)?,
        fg: parse_hex(value("foreground")?)?,
        accent: parse_hex(value("accent")?)?,
    })
}

/// `$XDG_STATE_HOME/omarchy`, else `~/.local/state/omarchy`.
fn omarchy_state_dir() -> Option<std::path::PathBuf> {
    Some(crate::paths::xdg_home("XDG_STATE_HOME", ".local/state")?.join("omarchy"))
}

/// True iff Omarchy's state directory exists — one `stat`, and the Follow-system row's gate.
pub fn present() -> bool {
    omarchy_state_dir().is_some_and(|d| d.is_dir())
}

/// Active theme, or `None` on a non-Omarchy box. `punktfunk.json` wins when
/// registered; `colors.toml` covers a client-only install. Uncached:
/// `omarchy-theme-set` retargets the `current` symlink.
pub fn current() -> Option<OsTheme> {
    let theme = omarchy_state_dir()?.join("current/theme");
    // A half-written switch parses as nothing; the other file, or a tick of "no theme", is fine.
    std::fs::read_to_string(theme.join("punktfunk.json"))
        .ok()
        .and_then(|raw| parse_punktfunk_json(&raw))
        .or_else(|| {
            std::fs::read_to_string(theme.join("colors.toml"))
                .ok()
                .and_then(|raw| parse_colors_toml(&raw))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(mode: &str, bg: &str, fg: &str, accent: &str) -> String {
        format!(
            r#"{{"schema":1,"mode":"{mode}","background":"{bg}","foreground":"{fg}","accent":"{accent}"}}"#
        )
    }

    #[test]
    fn an_unrendered_template_is_not_a_theme() {
        // `{{ accent }}` is JSON-valid; parse_hex is what refuses it.
        assert!(
            parse_punktfunk_json(&rendered("dark", "#1a1b26", "#c0caf5", "{{ accent }}")).is_none()
        );
        assert!(
            parse_punktfunk_json(r##"{"mode":"dark","accent":"#89b4fa"}"##).is_none(),
            "a partial palette"
        );
        assert!(
            parse_punktfunk_json(r#"{"mode":"dark","acc"#).is_none(),
            "half-written during a switch"
        );
        assert!(parse_punktfunk_json(&rendered("dark", "#1a1b26", "#c0caf5", "#zzzzzz")).is_none());
    }

    #[test]
    fn hex_shorthand_and_alpha_both_parse() {
        assert_eq!(parse_hex("#fff"), Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(parse_hex("#ffffff"), Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(
            parse_hex("#ffffffff"),
            parse_hex("#ffffff"),
            "alpha is dropped"
        );
        assert_eq!(parse_hex("#000f"), parse_hex("#000"));
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#ff"), None);
    }

    #[test]
    fn mode_defaults_to_dark_and_light_is_honoured() {
        let t = |m| parse_punktfunk_json(&rendered(m, "#1a1b26", "#c0caf5", "#7aa2f7")).unwrap();
        assert!(t("dark").dark);
        assert!(!t("light").dark);
        assert!(
            t("nonsense").dark,
            "an unparsable mode is dark, as in the console"
        );
    }

    #[test]
    fn colors_toml_reads_the_semantic_keys_and_nothing_else() {
        // Sibling keys PREFIX the ones we want (`dark_background`, `bright_foreground`).
        let raw = r##"
mode = "dark"

accent = "#7d82d9"
muted = "#6d7db6"

background = "#060B1E"
dark_background = "#040816"
lighter_background = "#131a3a"

foreground = "#ffcead"
bright_foreground = "#ffddcc"
"##;
        let t = parse_colors_toml(raw).unwrap();
        assert!(t.dark);
        assert_eq!(t.bg.hex(), "#060b1e");
        assert_eq!(t.fg.hex(), "#ffcead");
        assert_eq!(t.accent.hex(), "#7d82d9");
        assert!(
            parse_colors_toml("mode = \"dark\"\naccent = \"#fff\"").is_none(),
            "half a palette"
        );
    }

    #[test]
    fn a_washed_out_accent_is_lifted_until_it_reads_as_text() {
        let (bg, fg, accent) = (
            parse_hex("#fdf6e3").unwrap(),
            parse_hex("#5c6a72").unwrap(),
            parse_hex("#dfa000").unwrap(),
        );
        assert!(
            contrast(accent, bg) < 3.0,
            "the premise: unreadable as text"
        );
        assert!(contrast(readable(accent, bg, fg), bg) >= 3.0);
    }

    #[test]
    fn an_accent_that_already_reads_is_left_alone() {
        let (bg, fg, accent) = (
            parse_hex("#1a1b26").unwrap(),
            parse_hex("#c0caf5").unwrap(),
            parse_hex("#7aa2f7").unwrap(),
        );
        assert_eq!(readable(accent, bg, fg), accent);
    }

    #[test]
    fn text_on_the_accent_picks_the_readable_pole() {
        assert_eq!(
            on_accent(parse_hex("#dfa000").unwrap()),
            Rgb(0.0, 0.0, 0.0),
            "light fill"
        );
        assert_eq!(
            on_accent(parse_hex("#1a1b26").unwrap()),
            Rgb(1.0, 1.0, 1.0),
            "dark fill"
        );
    }
}
