//! Catalog sources: one URL, one signed index (design `plugin-store.md`).
//!
//! The official source is compiled in (slug, URL, two key slots) and is not
//! stored in config, so a hand-edited file cannot remove it. Operator sources
//! live in `<config_dir>/plugin-sources.json`.
//!
//! Official entries may show Verified. Operator sources get attribution only;
//! a third-party curator does not inherit tarball review.

use super::index::PublicKey;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Built-in slug. Operator sources may not reuse it.
pub(crate) const OFFICIAL_NAME: &str = "unom";

/// Built-in index URL. The document is signed; HTTPS is not the trust.
///
/// CI signs after merge, so the index can be newer than `<url>.sig`. That
/// window fails closed: verify rejects, the host keeps the last good copy.
pub(crate) const OFFICIAL_URL: &str =
    "https://git.unom.io/unom/punktfunk-plugin-index/raw/branch/main/v1/index.json";

/// Official signing keys. Two slots so rotation can overlap; an empty slot is ignored.
pub(crate) const OFFICIAL_KEYS: [&str; 2] = [
    "ed25519:V7KKMg8sq2A2TW7D/GFWaM0ruAvigpld9r93JdWcQHw=",
    "", // rotation slot
];

/// Operator-source cap. Guard rail, not a protocol limit.
const MAX_SOURCES: usize = 32;

/// A row in `plugin-sources.json`. camelCase to match the index; `mgmt::store` is snake_case.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Source {
    /// Slug (`[a-z][a-z0-9-]*`, ≤32). Cache file name and console attribution.
    pub name: String,
    /// `https://` index URL. Signature is fetched from `<url>.sig`.
    pub url: String,
    /// Pinned ed25519 key. Missing ⇒ unsigned: still listed, entries inherit the marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

impl Source {
    pub(crate) fn official() -> Source {
        Source {
            name: OFFICIAL_NAME.to_string(),
            url: OFFICIAL_URL.to_string(),
            // Keys live in [`OFFICIAL_KEYS`]. Config must not downgrade official to unsigned.
            public_key: None,
        }
    }

    pub(crate) fn is_official(&self) -> bool {
        self.name == OFFICIAL_NAME
    }

    /// Empty ⇒ unsigned (accepted, flagged).
    pub(crate) fn keys(&self) -> Vec<PublicKey> {
        if self.is_official() {
            return OFFICIAL_KEYS
                .iter()
                .filter(|k| !k.is_empty())
                .filter_map(|k| PublicKey::parse(k).ok())
                .collect();
        }
        self.public_key
            .as_deref()
            .and_then(|k| PublicKey::parse(k).ok())
            .into_iter()
            .collect()
    }

    /// Drives the console's unsigned marker.
    pub(crate) fn is_signed(&self) -> bool {
        !self.keys().is_empty()
    }

    /// `{url}.sig`. Derived, not stored, so a record cannot retarget the signature.
    pub(crate) fn sig_url(&self) -> String {
        format!("{}.sig", self.url)
    }

    fn validate(&self) -> Result<()> {
        if !crate::slug::source_name(&self.name) {
            bail!("source name must be kebab-case `[a-z][a-z0-9-]*`, ≤32 characters");
        }
        if !self.url.starts_with("https://") || self.url.len() > 500 {
            bail!("source url must be an https:// URL (≤500 characters)");
        }
        if let Some(k) = &self.public_key {
            PublicKey::parse(k).context("source publicKey")?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SourcesFile {
    #[serde(default = "one")]
    schema: u32,
    #[serde(default)]
    sources: Vec<Source>,
}

fn one() -> u32 {
    1
}

fn sources_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("plugin-sources.json")
}

/// Official is prepended here, never stored in the config file.
pub(crate) fn load() -> Vec<Source> {
    let mut out = vec![Source::official()];
    let path = sources_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return out;
    };
    match serde_json::from_slice::<SourcesFile>(&bytes) {
        Ok(file) => {
            for s in file.sources.into_iter().take(MAX_SOURCES) {
                // Drop hand-edited rows that reuse the official name or fail validate.
                if s.is_official() || s.validate().is_err() {
                    tracing::warn!(name = %s.name, "ignoring invalid entry in plugin-sources.json");
                    continue;
                }
                if !out.iter().any(|e| e.name == s.name) {
                    out.push(s);
                }
            }
        }
        Err(e) => tracing::warn!(
            "plugin-sources.json is unreadable ({e}) — using the official source only"
        ),
    }
    out
}

pub(crate) fn put(source: Source) -> Result<()> {
    source.validate()?;
    if source.is_official() {
        bail!("`{OFFICIAL_NAME}` is the built-in source and cannot be redefined");
    }
    let mut list: Vec<Source> = load().into_iter().filter(|s| !s.is_official()).collect();
    if list.len() >= MAX_SOURCES && !list.iter().any(|s| s.name == source.name) {
        bail!("too many plugin sources (max {MAX_SOURCES})");
    }
    list.retain(|s| s.name != source.name);
    list.push(source);
    save(list)
}

/// `Ok(false)` if the name was not present.
pub(crate) fn remove(name: &str) -> Result<bool> {
    if name == OFFICIAL_NAME {
        bail!("the built-in `{OFFICIAL_NAME}` source cannot be removed");
    }
    let list: Vec<Source> = load().into_iter().filter(|s| !s.is_official()).collect();
    if !list.iter().any(|s| s.name == name) {
        return Ok(false); // absent: do not rewrite the file
    }
    save(list.into_iter().filter(|s| s.name != name).collect())?;
    Ok(true)
}

fn save(list: Vec<Source>) -> Result<()> {
    let dir = pf_paths::config_dir();
    pf_paths::create_private_dir(&dir).context("create the punktfunk config dir")?;
    let file = SourcesFile {
        schema: 1,
        sources: list,
    };
    let json = serde_json::to_string_pretty(&file).context("serialize plugin-sources.json")?;
    let path = sources_path();
    pf_paths::replace_file(&path, format!("{json}\n").as_bytes())
        .with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_source_is_signed_by_compiled_in_keys() {
        let s = Source::official();
        assert!(s.is_official());
        assert!(s.is_signed(), "the built-in source must carry a pinned key");
        assert_eq!(s.sig_url(), format!("{OFFICIAL_URL}.sig"));
        // `keys()` ignores the record field; config cannot swap the official key.
        let mut forged = Source::official();
        forged.public_key = Some("ed25519:AAAA".into());
        assert_eq!(forged.keys().len(), 1);
    }

    #[test]
    fn compiled_in_keys_parse() {
        for k in OFFICIAL_KEYS.iter().filter(|k| !k.is_empty()) {
            PublicKey::parse(k).expect("compiled-in official key must parse");
        }
    }

    #[test]
    fn unsigned_third_party_source_is_flagged_not_rejected() {
        let s = Source {
            name: "retro-hub".into(),
            url: "https://example.org/index.json".into(),
            public_key: None,
        };
        s.validate().unwrap();
        assert!(!s.is_signed());
    }

    #[test]
    fn validation_rejects_bad_names_urls_and_keys() {
        let base = Source {
            name: "ok".into(),
            url: "https://e.org/i.json".into(),
            public_key: None,
        };
        assert!(base.validate().is_ok());
        assert!(Source {
            name: "Bad".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            name: "9bad".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            url: "http://e.org/i.json".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            public_key: Some("ed25519:nope".into()),
            ..base.clone()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn sig_url_is_derived_not_configurable() {
        let s = Source {
            name: "x".into(),
            url: "https://e.org/v1/index.json".into(),
            public_key: None,
        };
        assert_eq!(s.sig_url(), "https://e.org/v1/index.json.sig");
    }
}
