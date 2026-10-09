//! Kebab-case slugs: `[a-z0-9-]` under a per-kind length cap. Every surface that checks one
//! kind (registration API, catalog, sources) calls the same function, so they cannot drift.

/// `[a-z0-9-]{1,max}`, starting with a letter when `leading_letter`.
pub(crate) fn is_kebab(s: &str, max: usize, leading_letter: bool) -> bool {
    (1..=max).contains(&s.len())
        && (!leading_letter || s.starts_with(|c: char| c.is_ascii_lowercase()))
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Plugin id: the kebab-case rule the SDK enforces, so the registration id matches the package
/// name.
pub(crate) fn plugin_id(s: &str) -> bool {
    is_kebab(s, 64, true)
}

/// Plugin category. Closed charset, open vocabulary: an unknown one matches no console rule.
pub(crate) fn category(s: &str) -> bool {
    is_kebab(s, 32, true)
}

/// A lucide icon name.
pub(crate) fn lucide_icon(s: &str) -> bool {
    is_kebab(s, 48, false)
}

/// Name of a store catalog source.
pub(crate) fn source_name(s: &str) -> bool {
    is_kebab(s, 32, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ids() {
        assert!(plugin_id("rom-manager"));
        assert!(plugin_id("a"));
        assert!(plugin_id("x9"));
        assert!(!plugin_id(""));
        assert!(!plugin_id("9lives"));
        assert!(!plugin_id("-lead"));
        assert!(!plugin_id("Rom"));
        assert!(!plugin_id("rom_manager"));
        assert!(!plugin_id(&"a".repeat(65)));
    }

    #[test]
    fn icons_may_lead_with_a_digit() {
        assert!(lucide_icon("3d-rotate"));
        assert!(lucide_icon(&"a".repeat(48)));
        assert!(!lucide_icon(&"a".repeat(49)));
        assert!(!lucide_icon(""));
        assert!(!category("3d"));
        assert!(!source_name(&"a".repeat(33)));
    }
}
