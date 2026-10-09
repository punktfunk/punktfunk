//! Provenance of each installed plugin (design `plugin-store.md`).
//!
//! `<plugins_dir>/install-manifest.json`, written only by store installs.
//! `node_modules` names the package; this file names how it arrived.
//!
//! Absence is [`Tier::Cli`]: CLI `plugins add` never writes a row, and a
//! missing or corrupt file reads as empty rather than inventing a tier.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// JSON spelling is the console badge key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tier {
    /// Official index; this exact tarball was reviewed.
    Verified,
    /// Operator-added index. Attribution only — never the verified badge.
    External,
    /// Raw package spec. No index pin.
    Unverified,
    /// No manifest row. CLI `plugins add` never writes one.
    Cli,
}

impl Tier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tier::Verified => "verified",
            Tier::External => "external",
            Tier::Unverified => "unverified",
            Tier::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    pub tier: Tier,
    /// Catalog source slug; absent for unverified and CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Catalog entry id. Re-links the catalog row when the package name differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Requested pin. Live version is always read from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Raw spec for [`Tier::Unverified`]. Only record of what was typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// Wall-clock RFC-3339 stamp. Display only; never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    schema: u32,
    /// npm package name → record. `BTreeMap` keeps the on-disk order stable.
    #[serde(default)]
    plugins: BTreeMap<String, Record>,
}

impl Default for ManifestFile {
    fn default() -> Self {
        Self {
            schema: 1,
            plugins: BTreeMap::new(),
        }
    }
}

fn manifest_path(plugins_dir: &std::path::Path) -> std::path::PathBuf {
    plugins_dir.join("install-manifest.json")
}

/// Missing or corrupt file is empty: every package then reports as CLI.
pub(crate) fn load(plugins_dir: &std::path::Path) -> BTreeMap<String, Record> {
    let Ok(bytes) = std::fs::read(manifest_path(plugins_dir)) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<ManifestFile>(&bytes) {
        Ok(f) if f.schema == 1 => f.plugins,
        Ok(f) => {
            tracing::warn!(
                schema = f.schema,
                "unknown install-manifest schema — ignoring"
            );
            BTreeMap::new()
        }
        Err(e) => {
            tracing::warn!("install-manifest.json is unreadable ({e}) — treating as empty");
            BTreeMap::new()
        }
    }
}

pub(crate) fn record(plugins_dir: &std::path::Path, pkg: &str, rec: Record) -> Result<()> {
    let mut plugins = load(plugins_dir);
    plugins.insert(pkg.to_string(), rec);
    write(plugins_dir, plugins)
}

pub(crate) fn forget(plugins_dir: &std::path::Path, pkg: &str) -> Result<()> {
    let mut plugins = load(plugins_dir);
    if plugins.remove(pkg).is_none() {
        return Ok(());
    }
    write(plugins_dir, plugins)
}

fn write(plugins_dir: &std::path::Path, plugins: BTreeMap<String, Record>) -> Result<()> {
    let json = serde_json::to_string_pretty(&ManifestFile { schema: 1, plugins })
        .context("serialize install-manifest.json")?;
    let path = manifest_path(plugins_dir);
    pf_paths::replace_file(&path, format!("{json}\n").as_bytes())
        .with_context(|| format!("replace {}", path.display()))
}

/// RFC-3339 UTC stamp for [`Record::installed_at`].
pub(crate) fn now_stamp() -> String {
    punktfunk_core::time::utc_rfc3339(crate::clock::unix_ms(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tier: Tier) -> Record {
        Record {
            tier,
            source: Some("unom".into()),
            entry_id: Some("rom-manager".into()),
            version: Some("0.2.1".into()),
            spec: None,
            installed_at: Some(now_stamp()),
        }
    }

    #[test]
    fn round_trips_and_absence_means_cli() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty(), "no file ⇒ nothing recorded");

        record(
            dir.path(),
            "@punktfunk/plugin-rom-manager",
            rec(Tier::Verified),
        )
        .unwrap();
        let m = load(dir.path());
        let r = m.get("@punktfunk/plugin-rom-manager").unwrap();
        assert_eq!(r.tier, Tier::Verified);
        assert_eq!(r.version.as_deref(), Some("0.2.1"));
        assert!(!m.contains_key("punktfunk-plugin-other"));
    }

    #[test]
    fn tier_survives_a_reinstall_at_a_new_tier() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "@x/y", rec(Tier::Verified)).unwrap();
        record(dir.path(), "@x/y", rec(Tier::Unverified)).unwrap();
        assert_eq!(load(dir.path()).get("@x/y").unwrap().tier, Tier::Unverified);
    }

    #[test]
    fn forget_removes_only_the_named_package() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "@x/y", rec(Tier::Verified)).unwrap();
        record(dir.path(), "@x/z", rec(Tier::External)).unwrap();
        forget(dir.path(), "@x/y").unwrap();
        let m = load(dir.path());
        assert!(!m.contains_key("@x/y"));
        assert!(m.contains_key("@x/z"));
        forget(dir.path(), "@not/here").unwrap();
    }

    #[test]
    fn corrupt_manifest_reads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("install-manifest.json"), b"{ not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn tier_json_spelling_is_stable() {
        assert_eq!(
            serde_json::to_string(&Tier::Unverified).unwrap(),
            "\"unverified\""
        );
        assert_eq!(Tier::Verified.as_str(), "verified");
    }

    #[test]
    fn stamp_is_rfc3339_shaped() {
        let s = now_stamp();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        // Floor so a zeroed or pre-epoch conversion cannot pass as now.
        assert!(s.as_str() > "2026-01-01T00:00:00Z", "{s}");
    }
}
