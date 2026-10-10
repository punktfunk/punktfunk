//! Signed plugin-store catalog: one exact version plus tarball hash per entry
//! (design `plugin-store.md` §3.2).
//!
//! There is no "track latest". A newer upstream release is not offered until a
//! re-reviewed entry lands in a signed index.
//!
//! [`verify_signature`] runs over the exact bytes first; a failed signature is
//! an error, never a fallback to unsigned. Then every field is validated. A
//! malformed entry is dropped with a warning; a malformed document (unknown
//! schema, non-JSON, oversized) is rejected whole.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Only schema this host parses. A bump makes older hosts refuse the source.
pub(crate) const SCHEMA: u32 = 1;

/// Cap on a fetched index body so a hostile source cannot exhaust memory.
pub(crate) const MAX_INDEX_BYTES: usize = 5 * 1024 * 1024;

/// Caps so a compromised signing key cannot grow the console list without bound.
const MAX_PLUGINS: usize = 500;
const MAX_ADVISORIES: usize = 200;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Index {
    pub schema: u32,
    /// Display only.
    #[serde(default)]
    pub name: String,
    /// Display only. Freshness is our fetch clock, never this source-controlled field.
    #[serde(default)]
    pub generated: String,
    #[serde(default)]
    pub plugins: Vec<Entry>,
    /// Revocations, matched against installed packages as well as catalog rows.
    #[serde(default)]
    pub security: Vec<Advisory>,
}

/// One installable version, pinned by integrity hash.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    /// `definePlugin` id; also the lease-registry key, so catalogued vs running is comparable.
    pub id: String,
    /// Scoped npm name (`@scope/name`). The scope is the `bunfig.toml` key for [`Entry::registry`].
    pub pkg: String,
    /// `https://` only.
    pub registry: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Lucide name; unknown values fall back in the console, not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Exact semver, never a range.
    pub version: String,
    /// Tarball SRI. A republish of the same version with different bytes cannot pass install.
    pub integrity: String,
    /// Review of this tarball. A third-party source can set it, so it never grants Verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// Incompatible rows still list, greyed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_host: Option<String>,
    /// Empty means all.
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<DetectProbes>,
}

/// Existence-only probes (path or `HKLM\…`). No reads, no content match, at most one `*`
/// segment. The index is remotely updatable, so a probe is a stat, never exfil.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DetectProbes {
    #[serde(default)]
    pub linux: Vec<String>,
    #[serde(default)]
    pub windows: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Verification {
    /// Display only.
    pub reviewed_at: String,
}

/// Matched against installed packages, not only catalog rows.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Advisory {
    pub pkg: String,
    /// Unparseable means drop the advisory, never apply it to everything.
    pub versions: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// Shared with update manifests: detached ed25519 over exact bytes against pinned keys.
pub(crate) use pf_update_check::sig::{verify_signature, PublicKey};

impl Index {
    /// Reject the document on a structural problem; drop a bad entry or advisory with a warning.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Index> {
        if bytes.len() > MAX_INDEX_BYTES {
            bail!("index is larger than the {MAX_INDEX_BYTES}-byte cap");
        }
        let mut idx: Index = serde_json::from_slice(bytes).context("index is not valid JSON")?;
        if idx.schema != SCHEMA {
            bail!(
                "unsupported index schema {} (this host understands {SCHEMA})",
                idx.schema
            );
        }
        idx.name = sanitize(&idx.name, 64);
        idx.generated = sanitize(&idx.generated, 40);

        let mut seen: Vec<String> = Vec::new();
        idx.plugins.truncate(MAX_PLUGINS);
        idx.plugins.retain_mut(|e| match e.validate() {
            Err(why) => {
                tracing::warn!(pkg = %e.pkg, "dropping catalog entry: {why}");
                false
            }
            Ok(()) => {
                // Duplicate ids make install ambiguous; first wins.
                if seen.iter().any(|s| s == &e.id) {
                    tracing::warn!(id = %e.id, "dropping duplicate catalog entry");
                    false
                } else {
                    seen.push(e.id.clone());
                    true
                }
            }
        });

        idx.security.truncate(MAX_ADVISORIES);
        idx.security.retain_mut(|a| match a.validate() {
            Err(why) => {
                tracing::warn!(pkg = %a.pkg, "dropping advisory: {why}");
                false
            }
            Ok(()) => true,
        });
        Ok(idx)
    }
}

impl Entry {
    fn validate(&mut self) -> Result<()> {
        if !crate::slug::plugin_id(&self.id) {
            bail!("id must be kebab-case `[a-z][a-z0-9-]*`, ≤64");
        }
        if !valid_scoped_pkg(&self.pkg) {
            bail!("pkg must be a scoped npm name (`@scope/name`)");
        }
        if !is_https(&self.registry) {
            bail!("registry must be an https:// URL");
        }
        self.title = sanitize(&self.title, 64);
        if self.title.is_empty() {
            bail!("title must not be empty");
        }
        self.description = sanitize(&self.description, 280);
        self.author = sanitize(&self.author, 64);
        self.version = sanitize(&self.version, 32);
        if semver::Version::parse(&self.version).is_err() {
            bail!("version must be exact semver (`1.2.3`), not a range");
        }
        if !valid_integrity(&self.integrity) {
            bail!("integrity must look like `sha512-<base64>`");
        }
        if let Some(icon) = &self.icon {
            if !crate::slug::lucide_icon(icon) {
                self.icon = None; // drop the icon, not the entry
            }
        }
        if let Some(h) = &self.homepage {
            if !is_https(h) || h.len() > 200 {
                self.homepage = None;
            }
        }
        if let Some(l) = &self.license {
            self.license = Some(sanitize(l, 64)).filter(|s| !s.is_empty());
        }
        if let Some(v) = &self.verification {
            if v.reviewed_at.len() > 32 {
                self.verification = None;
            }
        }
        if let Some(m) = &self.min_host {
            if semver::Version::parse(m).is_err() {
                self.min_host = None; // unusable constraint means no constraint
            }
        }
        self.platforms
            .retain(|p| matches!(p.as_str(), "linux" | "windows" | "macos"));
        self.platforms.truncate(4);
        // Unknown or malformed categories/probes drop those fields, never the entry.
        self.categories.retain(|c| crate::slug::category(c));
        self.categories.truncate(4);
        if let Some(d) = &mut self.detect {
            d.linux.retain(|p| valid_probe(p));
            d.windows.retain(|p| valid_probe(p));
            d.linux.truncate(MAX_PROBES);
            d.windows.truncate(MAX_PROBES);
            if d.linux.is_empty() && d.windows.is_empty() {
                self.detect = None;
            }
        }
        Ok(())
    }

    /// `None` means no probes for this platform (unknown), not "not detected".
    pub(crate) fn detected(&self) -> Option<bool> {
        let probes = self.detect.as_ref()?;
        let list = if cfg!(windows) {
            &probes.windows
        } else if cfg!(target_os = "linux") {
            &probes.linux
        } else {
            return None;
        };
        if list.is_empty() {
            return None;
        }
        Some(list.iter().any(|p| probe_matches(p)))
    }

    pub(crate) fn incompatible_reason(&self) -> Option<String> {
        if !self.platforms.is_empty() && !self.platforms.iter().any(|p| p == HOST_PLATFORM) {
            return Some(format!("requires {}", self.platforms.join(" or ")));
        }
        if let Some(min) = &self.min_host {
            let min = semver::Version::parse(min).ok()?;
            if older_than(host_version(), &min) {
                return Some(format!("needs punktfunk {min} or newer"));
            }
        }
        None
    }
}

/// Whether the build stamped `host` predates `min`, on `major.minor.patch` alone: a canary
/// stamped `0.39.0-0.N` is 0.39.0. A stamp with no leading triple is never too old.
fn older_than(host: &str, min: &semver::Version) -> bool {
    pf_update_check::triple(host).is_some_and(|h| h < (min.major, min.minor, min.patch))
}

impl Advisory {
    fn validate(&mut self) -> Result<()> {
        if self.pkg.trim().is_empty() || self.pkg.len() > 214 {
            bail!("advisory pkg is empty or too long");
        }
        semver::VersionReq::parse(&self.versions)
            .context("advisory `versions` is not a semver requirement")?;
        self.reason = sanitize(&self.reason, 280);
        if let Some(u) = &self.url {
            if !is_https(u) || u.len() > 200 {
                self.url = None;
            }
        }
        Ok(())
    }

    pub(crate) fn matches(&self, pkg: &str, version: &str) -> bool {
        if self.pkg != pkg {
            return false;
        }
        let (Ok(req), Ok(v)) = (
            semver::VersionReq::parse(&self.versions),
            semver::Version::parse(version),
        ) else {
            return false;
        };
        req.matches(&v)
    }
}

pub(crate) const HOST_PLATFORM: &str = if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "macos") {
    "macos"
} else {
    "linux"
};

/// Left-hand side of every `minHost` comparison: the build stamp, not the manifest version,
/// which on a canary still names the last release.
pub(crate) fn host_version() -> &'static str {
    crate::version::get()
}

/// Catalog strings land in logs and the console: controls and bidi marks go, as in a device name.
fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && !crate::native_pairing::is_spoofy_char(c))
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `@scope/name`, stricter than npm: no URL-ish characters into `bun add` or `bunfig.toml`.
pub(crate) fn valid_scoped_pkg(pkg: &str) -> bool {
    let Some(rest) = pkg.strip_prefix('@') else {
        return false;
    };
    if pkg.len() > 214 {
        return false;
    }
    let Some((scope, name)) = rest.split_once('/') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
    };
    ok(scope) && ok(name)
}

/// `bunfig.toml` registry key for a scoped package.
pub(crate) fn scope_of(pkg: &str) -> Option<String> {
    let rest = pkg.strip_prefix('@')?;
    let (scope, _) = rest.split_once('/')?;
    Some(format!("@{scope}"))
}

fn valid_integrity(s: &str) -> bool {
    if s.len() > 200 {
        return false;
    }
    let Some((alg, b64)) = s.split_once('-') else {
        return false;
    };
    matches!(alg, "sha512" | "sha384" | "sha256" | "sha1")
        && !b64.is_empty()
        && b64
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn is_https(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

/// Per-platform probe cap; bounds the stat cost of rendering the catalog.
const MAX_PROBES: usize = 8;

/// Absolute path with at most one `*` segment, or `HKLM\…`. Relative would follow cwd.
/// One `*` bounds fan-out. `HKCU` is unreadable as LocalService; other hives are not for a remote index.
fn valid_probe(p: &str) -> bool {
    if p.is_empty() || p.len() > 260 {
        return false;
    }
    if let Some(key) = p.strip_prefix("HKLM\\") {
        return !key.is_empty()
            && !key.contains("..")
            && key.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'\\' | b' ' | b'-' | b'_' | b'.')
            });
    }
    let b = p.as_bytes();
    let absolute = p.starts_with('/') || (b.len() >= 3 && b[1] == b':' && b[2] == b'\\');
    // `~` is not expanded: the service account's home is not the user's launcher install.
    absolute && !p.contains("..") && p.matches('*').count() <= 1
}

/// Existence only — never a read.
fn probe_matches(p: &str) -> bool {
    #[cfg(windows)]
    if let Some(key) = p.strip_prefix("HKLM\\") {
        use std::os::windows::process::CommandExt;
        // `reg.exe query`, not a registry crate — no extra dep under LocalService.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        return std::process::Command::new(crate::install::sys32("reg.exe"))
            .args(["query", &format!("HKLM\\{key}")])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    #[cfg(not(windows))]
    if p.starts_with("HKLM\\") {
        return false;
    }
    match p.split_once('*') {
        None => std::path::Path::new(p).exists(),
        // One `*`: one directory read, match prefix/suffix around that segment.
        Some((before, after)) => {
            let (dir, prefix) = match before.rfind(['/', '\\']) {
                Some(i) => (&before[..=i], &before[i + 1..]),
                None => return false,
            };
            let (suffix, rest) = match after.find(['/', '\\']) {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            let Ok(read) = std::fs::read_dir(dir) else {
                return false;
            };
            read.flatten().any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(prefix)
                    && name.ends_with(suffix)
                    && name.len() >= prefix.len() + suffix.len()
                    && (rest.is_empty()
                        || e.path().join(rest.trim_start_matches(['/', '\\'])).exists())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(entry_json: &str) -> Vec<u8> {
        format!(r#"{{"schema":1,"name":"t","plugins":[{entry_json}]}}"#).into_bytes()
    }

    const GOOD: &str = r#"{"id":"rom-manager","pkg":"@punktfunk/plugin-rom-manager",
        "registry":"https://git.unom.io/api/packages/unom/npm/","title":"ROM Manager",
        "description":"d","author":"unom","version":"0.2.1","integrity":"sha512-AAAA",
        "verification":{"reviewedAt":"2026-07-19"},"platforms":["linux","windows"]}"#;

    #[test]
    fn parses_a_good_entry() {
        let idx = Index::parse(&doc(GOOD)).unwrap();
        assert_eq!(idx.plugins.len(), 1);
        let e = &idx.plugins[0];
        assert_eq!(e.pkg, "@punktfunk/plugin-rom-manager");
        assert_eq!(e.version, "0.2.1");
        assert!(e.verification.is_some());
    }

    #[test]
    fn rejects_unknown_schema_and_bad_json() {
        assert!(Index::parse(br#"{"schema":2,"plugins":[]}"#).is_err());
        assert!(Index::parse(b"not json").is_err());
    }

    /// SCHEMA stays 1: unknown fields ignored, absent fields default. No flag day.
    #[test]
    fn categories_and_probes_are_additive_and_sanitized() {
        let e = &Index::parse(&doc(GOOD)).unwrap().plugins[0];
        assert!(e.categories.is_empty());
        assert!(e.detect.is_none());
        assert_eq!(e.detected(), None, "no probes ⇒ unknown, not `false`");

        let rich = GOOD.trim_end_matches('}').to_string()
            + r#","categories":["library","Bad Cat","x","y","z","w"],
                 "detect":{"linux":["/usr/bin/steam","relative/path","/etc/../etc/passwd"],
                           "windows":["HKLM\\SOFTWARE\\Valve\\Steam","HKCU\\SOFTWARE\\Valve"]}}"#;
        let e = &Index::parse(&doc(&rich)).unwrap().plugins[0];
        assert_eq!(
            e.categories,
            ["library", "x", "y", "z"],
            "malformed dropped, capped at 4"
        );
        let d = e.detect.as_ref().expect("probes kept");
        assert_eq!(d.linux, ["/usr/bin/steam"], "relative + traversal dropped");
        assert_eq!(
            d.windows,
            ["HKLM\\SOFTWARE\\Valve\\Steam"],
            "HKCU is not evaluable as LocalService — dropped"
        );
    }

    #[test]
    fn probe_shapes_are_bounded() {
        assert!(valid_probe("/usr/bin/steam"));
        assert!(
            valid_probe("/home/*/.steam"),
            "one wildcard segment is fine"
        );
        assert!(valid_probe(r"C:\Program Files (x86)\Steam\steam.exe"));
        assert!(valid_probe(r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam"));
        assert!(!valid_probe("steam"));
        assert!(!valid_probe("/usr/../etc/passwd"));
        assert!(!valid_probe("/home/*/games/*/steam"));
        assert!(!valid_probe(r"HKCU\SOFTWARE\Valve"));
        assert!(!valid_probe(""));
        assert!(!valid_probe(&"/x".repeat(200)));
    }

    #[test]
    fn probes_evaluate_against_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("pf-probe-{}", std::process::id()));
        let nested = dir.join("SteamLibrary-42");
        std::fs::create_dir_all(nested.join("steamapps")).unwrap();
        let d = dir.to_string_lossy().into_owned();

        assert!(probe_matches(&format!("{d}/SteamLibrary-42")));
        assert!(!probe_matches(&format!("{d}/nope")));
        assert!(probe_matches(&format!("{d}/SteamLibrary-*")));
        assert!(probe_matches(&format!("{d}/SteamLibrary-*/steamapps")));
        assert!(!probe_matches(&format!("{d}/SteamLibrary-*/nope")));
        assert!(!probe_matches(&format!("{d}/Other-*")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drops_invalid_entries_but_keeps_the_rest() {
        let bad_unscoped = GOOD.replace("@punktfunk/plugin-rom-manager", "punktfunk-plugin-x");
        let body = format!(r#"{{"schema":1,"plugins":[{bad_unscoped},{GOOD}]}}"#);
        let idx = Index::parse(body.as_bytes()).unwrap();
        assert_eq!(idx.plugins.len(), 1, "unscoped pkg must be dropped (D8)");
        assert_eq!(idx.plugins[0].id, "rom-manager");
    }

    #[test]
    fn rejects_version_ranges_and_http_registries() {
        assert!(Index::parse(&doc(
            &GOOD.replace(r#""version":"0.2.1""#, r#""version":"^0.2.1""#)
        ))
        .unwrap()
        .plugins
        .is_empty());
        assert!(Index::parse(&doc(
            &GOOD.replace("https://git.unom.io", "http://git.unom.io")
        ))
        .unwrap()
        .plugins
        .is_empty());
    }

    #[test]
    fn drops_duplicate_ids() {
        let body = format!(r#"{{"schema":1,"plugins":[{GOOD},{GOOD}]}}"#);
        assert_eq!(Index::parse(body.as_bytes()).unwrap().plugins.len(), 1);
    }

    #[test]
    fn sanitizes_control_characters_in_display_fields() {
        let e = GOOD.replace("ROM Manager", "RO\\u0007\\u202eM");
        let idx = Index::parse(&doc(&e)).unwrap();
        assert_eq!(idx.plugins[0].title, "ROM");
    }

    #[test]
    fn advisory_matching_is_semver_ranged() {
        let mut a = Advisory {
            pkg: "@x/y".into(),
            versions: "<0.3.2".into(),
            reason: "bad".into(),
            url: None,
        };
        a.validate().unwrap();
        assert!(a.matches("@x/y", "0.3.1"));
        assert!(!a.matches("@x/y", "0.3.2"));
        assert!(!a.matches("@other/z", "0.3.1"));
    }

    #[test]
    fn advisory_with_unparseable_range_is_dropped() {
        let body = r#"{"schema":1,"plugins":[],"security":[
            {"pkg":"@x/y","versions":"not a range","reason":"r"}]}"#;
        assert!(Index::parse(body.as_bytes()).unwrap().security.is_empty());
    }

    #[test]
    fn scope_extraction() {
        assert_eq!(scope_of("@punktfunk/plugin-x").unwrap(), "@punktfunk");
        assert_eq!(scope_of("@a/b").unwrap(), "@a");
        assert!(scope_of("punktfunk-plugin-x").is_none());
    }

    #[test]
    fn min_host_compares_the_build_stamp_triple() {
        let min = semver::Version::parse("0.39.0").unwrap();
        // Every packaging spelling of a 0.39 canary is 0.39.0.
        for canary in [
            "0.39.0-0.00028370",
            "0.39.0~ci28370.gab12cd34",
            "0.39.28370",
            "0.39.0",
        ] {
            assert!(!older_than(canary, &min), "{canary}");
        }
        for old in ["0.38.0", "0.38.2-1", "0.38.0+gad2aee123"] {
            assert!(older_than(old, &min), "{old}");
        }
        assert!(!older_than("unknown", &min));
    }

    #[test]
    fn integrity_shape() {
        assert!(valid_integrity("sha512-abcABC123+/="));
        assert!(!valid_integrity("md5-abc"));
        assert!(!valid_integrity("sha512-"));
        assert!(!valid_integrity("sha512"));
    }

    /// Snapshot of the published `v1/index.json`. The index repo's TypeScript validator
    /// reimplements these rules; drift here silently drops entries from every operator's store.
    #[test]
    fn the_published_seed_index_parses() {
        let bytes = include_bytes!("testdata/seed-index.json");
        let idx = Index::parse(bytes).expect("the published index must parse");
        assert_eq!(idx.plugins.len(), 2, "no entry may be silently dropped");

        let rom = idx
            .plugins
            .iter()
            .find(|e| e.id == "rom-manager")
            .expect("rom-manager entry");
        assert_eq!(rom.pkg, "@punktfunk/plugin-rom-manager");
        assert!(rom.integrity.starts_with("sha512-"));
        assert!(
            semver::Version::parse(&rom.version).is_ok(),
            "exact version"
        );
        assert_eq!(
            rom.verification.as_ref().map(|v| v.reviewed_at.as_str()),
            Some("2026-07-20"),
            "camelCase `reviewedAt` must decode"
        );
        assert_eq!(
            rom.min_host.as_deref(),
            Some("0.15.0"),
            "camelCase `minHost`"
        );
        assert_eq!(scope_of(&rom.pkg).unwrap(), "@punktfunk");

        // Listed everywhere; installable only on a matching platform.
        let playnite = idx
            .plugins
            .iter()
            .find(|e| e.id == "playnite")
            .expect("playnite entry");
        assert_eq!(playnite.platforms, vec!["windows"]);
        if HOST_PLATFORM == "windows" {
            assert!(playnite.incompatible_reason().is_none());
        } else {
            assert!(playnite.incompatible_reason().is_some());
        }
    }

    #[test]
    fn public_key_parsing_rejects_junk() {
        assert!(PublicKey::parse("nope").is_err());
        assert!(PublicKey::parse("ed25519:!!!").is_err());
        assert!(PublicKey::parse("ed25519:AAAA").is_err());
    }
}
