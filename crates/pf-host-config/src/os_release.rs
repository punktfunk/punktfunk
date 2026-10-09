//! The distro's `os-release`, read once. Shared so the host, the injector and
//! the installer identify the distro the same way.

use std::sync::OnceLock;

/// The `os-release` keys the host and the installer read. Values are unquoted,
/// not sanitized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: Option<String>,
    /// Most-similar-first, per os-release(5).
    pub id_like: Vec<String>,
    pub version_id: Option<String>,
    pub pretty_name: Option<String>,
    pub name: Option<String>,
}

impl OsRelease {
    /// `KEY=value` lines, values optionally quoted. A later line wins.
    pub fn parse(contents: &str) -> OsRelease {
        let mut os = OsRelease::default();
        for line in contents.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = unquote(value);
            match key {
                "ID" => os.id = Some(value),
                "ID_LIKE" => os.id_like = value.split_whitespace().map(String::from).collect(),
                "VERSION_ID" => os.version_id = Some(value),
                "PRETTY_NAME" => os.pretty_name = Some(value),
                "NAME" => os.name = Some(value),
                _ => {}
            }
        }
        os
    }

    /// Whether `ID` or an `ID_LIKE` token is `tok`, ignoring ASCII case.
    pub fn is(&self, tok: &str) -> bool {
        self.id
            .iter()
            .chain(&self.id_like)
            .any(|t| t.eq_ignore_ascii_case(tok))
    }
}

/// This machine's `/etc/os-release`, else `/usr/lib/os-release`. Empty when
/// neither exists (Windows, macOS).
pub fn os_release() -> &'static OsRelease {
    static OS: OnceLock<OsRelease> = OnceLock::new();
    OS.get_or_init(|| {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|s| OsRelease::parse(&s))
            .unwrap_or_default()
    })
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_id_and_id_like_tokens_both_match() {
        let os = OsRelease::parse(
            "NAME=\"SteamOS\"\nID=\"steamos\"\nID_LIKE='arch'\nPRETTY_NAME=\"SteamOS Holo\"\n",
        );
        assert_eq!(os.id.as_deref(), Some("steamos"));
        assert_eq!(os.pretty_name.as_deref(), Some("SteamOS Holo"));
        assert!(os.is("steamos") && os.is("arch") && os.is("SteamOS"));
        let derivative =
            OsRelease::parse("ID=bazzite\nID_LIKE=\"fedora steamos\"\nVERSION_ID='43'\n");
        assert_eq!(derivative.id_like, ["fedora", "steamos"]);
        assert_eq!(derivative.version_id.as_deref(), Some("43"));
        assert!(derivative.is("steamos"));
        assert!(!derivative.is("steam"));
    }

    #[test]
    fn empty_or_garbage_is_nothing() {
        assert_eq!(OsRelease::parse(""), OsRelease::default());
        assert!(!OsRelease::parse("not an os-release file\n===\n").is("linux"));
    }
}
