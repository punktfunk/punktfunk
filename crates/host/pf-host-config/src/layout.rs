//! Where the host session's keyboard layout comes from.
//!
//! The key wire is US-positional: a client sends the Windows VK of the
//! physical key, [`vk_to_evdev`](super::keymap::vk_to_evdev) turns that into
//! an evdev code, and the **session keymap** decides the character. Host
//! layout must match the client's printed layout, or ISO keys scramble
//! (`#`→`\`, `ä`→`'`, y↔z).
//!
//! A Wayland compositor never reads `/etc/X11/xorg.conf.d/00-keyboard.conf`
//! (`localectl` writes it for Xorg). libxkbcommon's fallback stops at
//! `XKB_DEFAULT_*`, which no session manager exports. Compiling from empty
//! names on a configured German box still yields evdev/pc105/us.
//!
//! [`system_layout`] reads what the machine recorded. Advisory for backends
//! whose keymap we do not own (libei/KWin/gamescope — see
//! [`crate::text_input_supported`]); there it only feeds the diagnostic.

use std::path::{Path, PathBuf};

/// The five xkb rule names. `None` means unset; libxkbcommon then applies
/// `evdev`/`pc105`/`us`/``/``.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XkbNames {
    pub rules: Option<String>,
    pub model: Option<String>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
}

impl XkbNames {
    pub fn is_empty(&self) -> bool {
        self.rules.is_none()
            && self.model.is_none()
            && self.layout.is_none()
            && self.variant.is_none()
            && self.options.is_none()
    }

    /// Fill unset fields from `fallback`, one field at a time — matching
    /// libxkbcommon's `XKB_DEFAULT_*` (a box that exports only LAYOUT still
    /// keeps its configured variant).
    fn fill_from(&mut self, fallback: XkbNames) {
        for (slot, value) in [
            (&mut self.rules, fallback.rules),
            (&mut self.model, fallback.model),
            (&mut self.layout, fallback.layout),
            (&mut self.variant, fallback.variant),
            (&mut self.options, fallback.options),
        ] {
            if slot.is_none() {
                *slot = value;
            }
        }
    }

    /// Names as `xkb_keymap_new_from_names` wants them: empty string is unset,
    /// and libxkbcommon applies its default for that field.
    pub fn as_args(&self) -> (&str, &str, &str, &str, Option<String>) {
        (
            self.rules.as_deref().unwrap_or(""),
            self.model.as_deref().unwrap_or(""),
            self.layout.as_deref().unwrap_or(""),
            self.variant.as_deref().unwrap_or(""),
            self.options.clone(),
        )
    }

    /// `XKB_DEFAULT_*` pairs for a compositor we spawn. Only resolved fields:
    /// an explicit empty `XKB_DEFAULT_VARIANT` overrides a variant the rules
    /// file would otherwise supply.
    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        [
            ("XKB_DEFAULT_RULES", &self.rules),
            ("XKB_DEFAULT_MODEL", &self.model),
            ("XKB_DEFAULT_LAYOUT", &self.layout),
            ("XKB_DEFAULT_VARIANT", &self.variant),
            ("XKB_DEFAULT_OPTIONS", &self.options),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k, v.clone())))
        .collect()
    }

    pub fn describe(&self) -> String {
        match (&self.layout, &self.variant) {
            (Some(l), Some(v)) if !v.is_empty() => format!("{l}({v})"),
            (Some(l), _) => l.clone(),
            (None, _) => "us (libxkbcommon default)".to_string(),
        }
    }
}

/// Resolved layout plus the file an operator would edit.
#[derive(Clone, Debug)]
pub struct SystemLayout {
    pub names: XkbNames,
    /// Origin of `names.layout` — the only field worth naming in a one-line log.
    pub source: String,
}

/// What `localectl set-x11-keymap` writes.
const X11_CONF_DIR: &str = "/etc/X11/xorg.conf.d";
/// systemd ≥ 249 mirrors the X11 keymap here as `XKBLAYOUT=` / `XKBVARIANT=`.
const VCONSOLE_CONF: &str = "/etc/vconsole.conf";

/// Host keyboard layout: `XKB_DEFAULT_*` → `/etc/X11/xorg.conf.d/*keyboard*.conf`
/// → `/etc/vconsole.conf`. Per-field, so a partial box keeps each source's knowledge.
pub fn system_layout() -> SystemLayout {
    resolve_from(
        from_env(),
        Path::new(X11_CONF_DIR),
        Path::new(VCONSOLE_CONF),
    )
}

/// [`system_layout`] with inputs injected so tests do not read the machine's `XKB_DEFAULT_*`.
fn resolve_from(env: XkbNames, x11_dir: &Path, vconsole: &Path) -> SystemLayout {
    let mut names = env;
    let mut source = if names.layout.is_some() {
        "XKB_DEFAULT_LAYOUT".to_string()
    } else {
        String::new()
    };

    for path in x11_keyboard_confs(x11_dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = parse_x11_keyboard_conf(&text);
        if source.is_empty() && parsed.layout.is_some() {
            source = path.display().to_string();
        }
        names.fill_from(parsed);
    }

    if let Ok(text) = std::fs::read_to_string(vconsole) {
        let parsed = parse_vconsole_conf(&text);
        if source.is_empty() && parsed.layout.is_some() {
            source = vconsole.display().to_string();
        }
        names.fill_from(parsed);
    }

    if source.is_empty() {
        source = "unconfigured".to_string();
    }
    SystemLayout { names, source }
}

fn from_env() -> XkbNames {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    XkbNames {
        rules: get("XKB_DEFAULT_RULES"),
        model: get("XKB_DEFAULT_MODEL"),
        layout: get("XKB_DEFAULT_LAYOUT"),
        variant: get("XKB_DEFAULT_VARIANT"),
        options: get("XKB_DEFAULT_OPTIONS"),
    }
}

/// `*keyboard*.conf` in **reverse** lexical order. Xorg merges low-to-high
/// (later file wins); [`resolve_from`] is first-hit-wins, so the highest-
/// numbered snippet must come first.
fn x11_keyboard_confs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "conf")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("keyboard"))
        })
        .collect();
    out.sort();
    out.reverse();
    out
}

/// `Option "XkbLayout" "de"` from an Xorg `InputClass` snippet. Section-blind:
/// `localectl` writes one `MatchIsKeyboard` class; scanning every `Option`
/// beats half-implementing the Xorg grammar.
fn parse_x11_keyboard_conf(text: &str) -> XkbNames {
    let mut names = XkbNames::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('"');
        let Some(head) = fields.next() else { continue };
        if !head.trim_end().eq_ignore_ascii_case("Option") {
            continue;
        }
        let (Some(key), Some(_), Some(value)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let slot = match key.to_ascii_lowercase().as_str() {
            "xkbrules" => &mut names.rules,
            "xkbmodel" => &mut names.model,
            "xkblayout" => &mut names.layout,
            "xkbvariant" => &mut names.variant,
            "xkboptions" => &mut names.options,
            _ => continue,
        };
        *slot = Some(value.to_string());
    }
    names
}

/// `XKBLAYOUT=de` / `XKBVARIANT="nodeadkeys"` from `/etc/vconsole.conf`.
///
/// `KEYMAP=` is ignored: it names a console keymap (`de-nodeadkeys`, `uk`,
/// `sg-latin1`) whose namespace only coincides with xkb by accident (`uk` is
/// xkb `gb`). A wrong guess here mis-types every key.
fn parse_vconsole_conf(text: &str) -> XkbNames {
    let mut names = XkbNames::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).to_string();
        if value.is_empty() {
            continue;
        }
        let slot = match key.trim().to_ascii_uppercase().as_str() {
            "XKBRULES" => &mut names.rules,
            "XKBMODEL" => &mut names.model,
            "XKBLAYOUT" => &mut names.layout,
            "XKBVARIANT" => &mut names.variant,
            "XKBOPTIONS" => &mut names.options,
            _ => continue,
        };
        *slot = Some(value);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `localectl set-x11-keymap de pc105 nodeadkeys` output.
    const LOCALECTL_DE: &str = r#"# Written by systemd-localed(8), read by systemd-localed and Xorg. It's
# probably wise not to edit this file manually. Use localectl(1) to
# update this file.
Section "InputClass"
        Identifier "system-keyboard"
        MatchIsKeyboard "on"
        Option "XkbLayout" "de"
        Option "XkbModel" "pc105"
        Option "XkbVariant" "nodeadkeys"
EndSection
"#;

    #[test]
    fn parses_localectl_x11_snippet() {
        let n = parse_x11_keyboard_conf(LOCALECTL_DE);
        assert_eq!(n.layout.as_deref(), Some("de"));
        assert_eq!(n.model.as_deref(), Some("pc105"));
        assert_eq!(n.variant.as_deref(), Some("nodeadkeys"));
        assert_eq!(n.rules, None);
        assert_eq!(n.describe(), "de(nodeadkeys)");
    }

    #[test]
    fn x11_snippet_ignores_comments_and_non_xkb_options() {
        let n = parse_x11_keyboard_conf(
            "# Option \"XkbLayout\" \"fr\"\n\
             Identifier \"system-keyboard\"\n\
             Option \"XkbLayout\" \"ch\"\n\
             Option \"SomethingElse\" \"nope\"\n",
        );
        assert_eq!(n.layout.as_deref(), Some("ch"));
        assert!(n.options.is_none());
    }

    #[test]
    fn parses_vconsole_and_ignores_console_keymap() {
        let n = parse_vconsole_conf("KEYMAP=\"de-nodeadkeys\"\nFONT=\"eurlatgr\"\n");
        assert!(n.is_empty(), "KEYMAP must not be read as an xkb layout");

        let n = parse_vconsole_conf("XKBLAYOUT=de\nXKBVARIANT=\"nodeadkeys\"\nKEYMAP=de\n");
        assert_eq!(n.layout.as_deref(), Some("de"));
        assert_eq!(n.variant.as_deref(), Some("nodeadkeys"));
    }

    #[test]
    fn empty_names_describe_as_the_us_default() {
        let n = XkbNames::default();
        assert!(n.is_empty());
        assert_eq!(n.describe(), "us (libxkbcommon default)");
        assert_eq!(n.as_args(), ("", "", "", "", None));
        assert!(n.env_pairs().is_empty());
    }

    #[test]
    fn per_field_fallback_keeps_the_more_specific_source() {
        let mut n = XkbNames {
            layout: Some("fr".into()),
            ..Default::default()
        };
        n.fill_from(parse_x11_keyboard_conf(LOCALECTL_DE));
        assert_eq!(n.layout.as_deref(), Some("fr"), "env layout must win");
        assert_eq!(n.variant.as_deref(), Some("nodeadkeys"));
        assert_eq!(n.model.as_deref(), Some("pc105"));
    }

    #[test]
    fn env_pairs_round_trip_only_the_resolved_fields() {
        let n = XkbNames {
            layout: Some("de".into()),
            variant: Some("nodeadkeys".into()),
            ..Default::default()
        };
        assert_eq!(
            n.env_pairs(),
            vec![
                ("XKB_DEFAULT_LAYOUT", "de".to_string()),
                ("XKB_DEFAULT_VARIANT", "nodeadkeys".to_string()),
            ]
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-layout-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("xorg.conf.d")).unwrap();
        dir
    }

    #[test]
    fn resolve_reads_the_x11_snippet_then_vconsole() {
        let dir = scratch("x11-then-vconsole");
        let x11 = dir.join("xorg.conf.d");
        std::fs::write(x11.join("00-keyboard.conf"), LOCALECTL_DE).unwrap();
        let vconsole = dir.join("vconsole.conf");
        std::fs::write(&vconsole, "XKBLAYOUT=fr\nXKBOPTIONS=compose:ralt\n").unwrap();

        let got = resolve_from(XkbNames::default(), &x11, &vconsole);
        assert_eq!(got.names.layout.as_deref(), Some("de"));
        assert_eq!(got.names.variant.as_deref(), Some("nodeadkeys"));
        assert_eq!(got.names.options.as_deref(), Some("compose:ralt"));
        assert!(got.source.ends_with("00-keyboard.conf"), "{}", got.source);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_higher_numbered_xorg_snippet_wins() {
        let dir = scratch("xorg-precedence");
        let x11 = dir.join("xorg.conf.d");
        std::fs::write(x11.join("00-keyboard.conf"), LOCALECTL_DE).unwrap();
        std::fs::write(
            x11.join("90-custom-keyboard.conf"),
            "Option \"XkbLayout\" \"no\"\n",
        )
        .unwrap();

        let got = resolve_from(XkbNames::default(), &x11, &dir.join("absent"));
        assert_eq!(got.names.layout.as_deref(), Some("no"));
        assert_eq!(got.names.variant.as_deref(), Some("nodeadkeys"));
        assert!(
            got.source.ends_with("90-custom-keyboard.conf"),
            "{}",
            got.source
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_outranks_every_file() {
        let dir = scratch("env-wins");
        let x11 = dir.join("xorg.conf.d");
        std::fs::write(x11.join("00-keyboard.conf"), LOCALECTL_DE).unwrap();

        let env = XkbNames {
            layout: Some("us".into()),
            ..Default::default()
        };
        let got = resolve_from(env, &x11, &dir.join("absent"));
        assert_eq!(got.names.layout.as_deref(), Some("us"));
        assert_eq!(got.source, "XKB_DEFAULT_LAYOUT");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_on_a_bare_box_reports_unconfigured() {
        let missing = Path::new("/nonexistent/pf-layout-test");
        let got = resolve_from(XkbNames::default(), missing, missing);
        assert_eq!(got.source, "unconfigured");
        assert!(got.names.is_empty());
        assert_eq!(got.names.describe(), "us (libxkbcommon default)");
    }
}
