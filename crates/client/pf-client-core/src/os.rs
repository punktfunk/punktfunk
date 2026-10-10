//! Client half of the host's OS-identity advertisement (mDNS `os=` TXT; producer
//! is the host crate's `osinfo.rs`): sanitize the untrusted chain once, then the
//! icon-lookup order every front-end walks.
//!
//! Slash-separated, generic → specific (`linux[/<family>][/<id>]`). A UI walks
//! [`os_icon_tokens`] most-specific-first (brand aliases applied) and takes the
//! first token it has art for. Empty or unknown chains fall through to the UI's
//! fallback glyph. UI-agnostic so every shell resolves identically.

pub use punktfunk_core::discovery::sanitize_os;

/// Most-specific-first after sanitize. Empty means no OS icon.
pub fn os_icon_tokens(chain: &str) -> Vec<String> {
    sanitize_os(chain)
        .split('/')
        .rev()
        .filter(|t| !t.is_empty())
        .map(|t| match t {
            "macos" => "apple".to_string(),
            "steamos" => "steam".to_string(),
            t => t.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_is_most_specific_first() {
        assert_eq!(
            os_icon_tokens("linux/fedora/bazzite"),
            ["bazzite", "fedora", "linux"]
        );
        assert_eq!(os_icon_tokens("windows"), ["windows"]);
    }

    #[test]
    fn walk_applies_brand_aliases() {
        assert_eq!(os_icon_tokens("macos"), ["apple"]);
        assert_eq!(
            os_icon_tokens("linux/arch/steamos"),
            ["steam", "arch", "linux"]
        );
    }

    #[test]
    fn walk_of_nothing_is_empty() {
        assert!(os_icon_tokens("").is_empty());
        assert!(os_icon_tokens("!!!").is_empty());
    }
}
