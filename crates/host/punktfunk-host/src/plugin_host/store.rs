//! Plugin store: discover and install plugins from signed catalogs
//! (`design/plugin-store.md`).
//!
//! Installing a plugin runs that code with operator privileges. The signed
//! index is the verification gate: each entry pins one version and its tarball
//! hash. Nothing here tracks "latest".
//!
//! Three tiers: **verified** (built-in `unom` index, badged), **external**
//! (operator-added index, attributed, no badge), **unverified** (raw spec in
//! the console danger dialog — install only, never listed).
//!
//! Domain half (catalog state, installed-package facts, trust). HTTP lives in
//! [`crate::mgmt::store`]. Blocking; async callers use `spawn_blocking`.

pub(crate) mod catalog;
pub(crate) mod index;
pub(crate) mod jobs;
pub(crate) mod manifest;
pub(crate) mod sources;

use anyhow::{bail, Context, Result};
use index::{Advisory, Entry, Index};
use sources::Source;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Six hours. Catalogs move when a review lands, not on a poll. Do not fetch
/// while nobody has the store page open.
const CATALOG_TTL_SECS: u64 = 6 * 60 * 60;

/// Same directory the SDK and runner use.
pub(crate) fn plugins_dir() -> PathBuf {
    pf_paths::config_dir().join("plugins")
}

#[derive(Debug, Clone)]
pub(crate) struct InstalledPkg {
    pub pkg: String,
    pub version: Option<String>,
}

/// Top-level `dependencies` of the plugins dir's `package.json`. `None` only
/// when the file is unreadable. A missing `dependencies` key is `[]`: `bun
/// remove` drops it, and a convention fallback would resurrect `plugin-kit`.
fn top_level_deps(dir: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(match v.get("dependencies").and_then(|d| d.as_object()) {
        Some(deps) => deps.keys().cloned().collect(),
        None => Vec::new(),
    })
}

/// Same conventions as the runner: unscoped `punktfunk-plugin-*`, and any
/// scope's `plugin-*`. When [`top_level_deps`] exists, only those names —
/// otherwise a plugin's library (`@punktfunk/plugin-kit`) looks installed.
/// No readable `package.json` falls back to the convention.
pub(crate) fn installed_packages(dir: &Path) -> Vec<InstalledPkg> {
    let modules = dir.join("node_modules");
    let top_level = top_level_deps(dir);
    let mut out = Vec::new();
    let version_of = |pkg_dir: &Path| -> Option<String> {
        let bytes = std::fs::read(pkg_dir.join("package.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v.get("version")?.as_str().map(str::to_string)
    };
    let Ok(entries) = std::fs::read_dir(&modules) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with("punktfunk-plugin-") {
            let dir = modules.join(&name);
            out.push(InstalledPkg {
                version: version_of(&dir),
                pkg: name,
            });
        } else if name.starts_with('@') {
            let scope_dir = modules.join(&name);
            let Ok(scoped) = std::fs::read_dir(&scope_dir) else {
                continue;
            };
            let mut inner: Vec<String> = scoped
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            inner.sort();
            for s in inner {
                if s.starts_with("plugin-") {
                    let dir = scope_dir.join(&s);
                    out.push(InstalledPkg {
                        pkg: format!("{name}/{s}"),
                        version: version_of(&dir),
                    });
                }
            }
        }
    }
    if let Some(top) = top_level {
        out.retain(|p| top.iter().any(|d| d == &p.pkg));
    }
    out
}

/// Hand-formatted into TOML as `"{scope}" = "{url}"`. Do not switch to a
/// denylist of quotes and newlines. No quote, whitespace, control, or
/// backslash can pass.
fn valid_registry_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    !rest.is_empty()
        && url.len() <= 512
        && rest.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '.'
                        | '_'
                        | '~'
                        | ':'
                        | '/'
                        | '?'
                        | '#'
                        | '['
                        | ']'
                        | '@'
                        | '!'
                        | '$'
                        | '&'
                        | '\''
                        | '('
                        | ')'
                        | '*'
                        | '+'
                        | ','
                        | ';'
                        | '='
                        | '%'
                )
        })
}

/// Written here, not via `runner --registry`: `runner_command()` may resolve
/// an older scripting package that treats the flag value as a package name.
/// Idempotent; unrelated content survives. Matches `sdk/src/plugins.ts::ensureBunfig`.
pub(crate) fn ensure_bunfig_scope(dir: &Path, scope: &str, url: &str) -> Result<()> {
    // Both halves are interpolated into `"{scope}" = "{url}"`. `Entry::registry`
    // never goes through `sanitize`; a URL that closes the quote injects a
    // top-level `[install]` table that bun keeps after the source is deleted.
    if !index::valid_scoped_pkg(&format!("{scope}/x")) || !valid_registry_url(url) {
        bail!("refusing to map scope `{scope}` to `{url}`");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("bunfig.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let wanted = format!("\"{scope}\" = \"{url}\"");

    let is_mapping_for_scope = |line: &str| {
        let t = line.trim_start();
        t.starts_with(&format!("\"{scope}\"")) || t.starts_with(&format!("{scope} "))
    };
    if existing.lines().any(|l| l.trim() == wanted) {
        return Ok(());
    }
    let updated = if existing.lines().any(is_mapping_for_scope) {
        existing
            .lines()
            .map(|l| {
                if is_mapping_for_scope(l) {
                    wanted.clone()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else if let Some(pos) = existing
        .lines()
        .position(|l| l.trim() == "[install.scopes]")
    {
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        lines.insert(pos + 1, wanted);
        lines.join("\n") + "\n"
    } else if existing.trim().is_empty() {
        format!("[install.scopes]\n{wanted}\n")
    } else {
        format!(
            "{}{}\n[install.scopes]\n{wanted}\n",
            existing,
            if existing.ends_with('\n') { "" } else { "\n" }
        )
    };
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Seed a `package.json` so `bun add` installs here. It walks up to the
/// nearest ancestor and still exits 0 if a stray file captures the tree.
/// Skip when `package.json` or `node_modules` exists: empty `dependencies`
/// would hide plugins [`installed_packages`] already sees.
pub(crate) fn ensure_plugin_root(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("package.json");
    if path.exists() || dir.join("node_modules").exists() {
        return Ok(());
    }
    std::fs::write(
        &path,
        "{\n  \"name\": \"punktfunk-plugins\",\n  \"private\": true\n}\n",
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Nearest `package.json` **above** `dir` — the tree that would capture `bun add`.
pub(crate) fn capturing_ancestor(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .map(|a| a.join("package.json"))
        .find(|p| p.exists())
}

/// Uninstall must not take a shared dependency.
pub(crate) fn valid_installed_pkg(pkg: &str) -> Result<()> {
    let plausible = pkg.starts_with("punktfunk-plugin-")
        || (index::valid_scoped_pkg(pkg)
            && pkg
                .split_once('/')
                .is_some_and(|(_, name)| name.starts_with("plugin-")));
    if !plausible {
        bail!("`{pkg}` is not a plugin package (`@scope/plugin-*` or `punktfunk-plugin-*`)");
    }
    Ok(())
}

/// Best-effort; a catalogued plugin's authoritative id is its index entry.
pub(crate) fn plugin_id_for_pkg(pkg: &str) -> Option<String> {
    let last = pkg.rsplit('/').next()?;
    let id = last
        .strip_prefix("punktfunk-plugin-")
        .or_else(|| last.strip_prefix("plugin-"))?;
    crate::slug::plugin_id(id).then(|| id.to_string())
}

#[derive(Clone)]
pub(crate) struct SourceState {
    pub source: Source,
    pub index: Option<Index>,
    /// Unix seconds of the fetch that produced [`Self::index`].
    pub fetched_at: Option<u64>,
    /// Last refresh failed; this is an older copy.
    pub stale: bool,
    /// Last failure, shown against the source in the console.
    pub error: Option<String>,
    pub etag: Option<String>,
}

impl SourceState {
    fn empty(source: Source) -> Self {
        Self {
            source,
            index: None,
            fetched_at: None,
            stale: false,
            error: None,
            etag: None,
        }
    }

    fn is_fresh(&self) -> bool {
        self.index.is_some()
            && self
                .fetched_at
                .is_some_and(|t| crate::clock::unix_secs_u64().saturating_sub(t) < CATALOG_TTL_SECS)
    }
}

fn state() -> &'static RwLock<Vec<SourceState>> {
    static STATE: std::sync::OnceLock<RwLock<Vec<SourceState>>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| RwLock::new(Vec::new()))
}

/// New source names seed from the on-disk cache so a cold host can browse.
fn sync_sources() {
    let configured = sources::load();
    let dir = catalog::cache_dir();
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    st.retain(|s| configured.iter().any(|c| c.name == s.source.name));
    for c in configured {
        match st.iter_mut().find(|s| s.source.name == c.name) {
            // URL or key changed under this name — drop the old index rather than
            // attributing it to the new definition.
            Some(existing) => {
                if existing.source.url != c.url || existing.source.public_key != c.public_key {
                    *existing = SourceState::empty(c);
                } else {
                    existing.source = c;
                }
            }
            None => {
                let mut fresh = SourceState::empty(c.clone());
                if let Some((index, meta)) = catalog::read_cache(&dir, &c.name) {
                    fresh.index = Some(index);
                    fresh.fetched_at = Some(meta.fetched_at);
                    fresh.etag = meta.etag;
                    fresh.stale = true; // disk cache: freshness unverified until a fetch
                }
                st.push(fresh);
            }
        }
    }
}

/// **Blocking**. Refreshes past-TTL sources (or all when `force`). Freshness
/// is our fetch clock, never the document's `generated`.
pub(crate) fn catalogs(force: bool) -> Vec<SourceState> {
    sync_sources();
    let dir = catalog::cache_dir();
    let todo: Vec<Source> = {
        let st = state().read().unwrap_or_else(|e| e.into_inner());
        st.iter()
            .filter(|s| force || !s.is_fresh())
            .map(|s| s.source.clone())
            .collect()
    };
    for source in todo {
        let etag = {
            let st = state().read().unwrap_or_else(|e| e.into_inner());
            st.iter()
                .find(|s| s.source.name == source.name)
                .and_then(|s| s.etag.clone())
        };
        let outcome = catalog::fetch(&source, etag.as_deref());
        let now = crate::clock::unix_secs_u64();
        let mut st = state().write().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = st.iter_mut().find(|s| s.source.name == source.name) else {
            continue; // source removed during the fetch
        };
        match outcome {
            catalog::Fetched::Fresh { index, etag } => {
                let count = index.plugins.len();
                catalog::write_cache(
                    &dir,
                    &source.name,
                    &index,
                    &catalog::CacheMeta {
                        etag: etag.clone(),
                        fetched_at: now,
                    },
                );
                slot.index = Some(*index);
                slot.fetched_at = Some(now);
                slot.etag = etag;
                slot.stale = false;
                slot.error = None;
                tracing::info!(source = %source.name, entries = count, "plugin catalog refreshed");
            }
            catalog::Fetched::NotModified => {
                slot.fetched_at = Some(now);
                slot.stale = false;
                slot.error = None;
            }
            catalog::Fetched::Failed(why) => {
                // Keep the last good index; mark it stale.
                tracing::warn!(source = %source.name, "plugin catalog refresh failed: {why}");
                slot.stale = true;
                slot.error = Some(why);
            }
        }
    }
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// No network. For install resolve and advisory lookup.
pub(crate) fn cached_catalogs() -> Vec<SourceState> {
    sync_sources();
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// The bool is whether the source is built-in (`verified`).
pub(crate) fn find_entry(source_name: &str, id: &str) -> Option<(Entry, bool)> {
    cached_catalogs().into_iter().find_map(|s| {
        if s.source.name != source_name {
            return None;
        }
        let entry = s.index?.plugins.into_iter().find(|e| e.id == id)?;
        Some((entry, s.source.is_official()))
    })
}

/// Revocations are not scoped to the source the plugin came from.
pub(crate) fn advisory_for(pkg: &str, version: Option<&str>) -> Option<Advisory> {
    let version = version?;
    cached_catalogs().into_iter().find_map(|s| {
        s.index?
            .security
            .into_iter()
            .find(|a| a.matches(pkg, version))
    })
}

/// Drop a removed source's cached index so re-adding the name cannot serve stale rows.
pub(crate) fn drop_source_cache(name: &str) {
    catalog::drop_cache(&catalog::cache_dir(), name);
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    st.retain(|s| s.source.name != name);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_pkg(root: &Path, pkg: &str, version: &str) {
        let dir = root.join("node_modules").join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{pkg}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn scans_both_conventions_and_any_scope() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.0");
        // Any scope's `plugin-*` must be found; catalog entries are required to
        // be scoped, so limiting discovery to `@punktfunk` would hide externals.
        touch_pkg(dir.path(), "@retro-hub/plugin-x", "1.0.0");
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        touch_pkg(dir.path(), "effect", "4.0.0");
        touch_pkg(dir.path(), "@punktfunk/host", "0.1.2");

        let found = installed_packages(dir.path());
        let names: Vec<&str> = found.iter().map(|p| p.pkg.as_str()).collect();
        assert!(
            names.contains(&"@punktfunk/plugin-rom-manager"),
            "{names:?}"
        );
        assert!(names.contains(&"@retro-hub/plugin-x"), "{names:?}");
        assert!(names.contains(&"punktfunk-plugin-legacy"), "{names:?}");
        assert!(!names.contains(&"effect"), "{names:?}");
        assert!(!names.contains(&"@punktfunk/host"), "{names:?}");
        assert_eq!(
            found
                .iter()
                .find(|p| p.pkg == "@punktfunk/plugin-rom-manager")
                .unwrap()
                .version
                .as_deref(),
            Some("0.3.0")
        );
    }

    #[test]
    fn scan_of_a_missing_dir_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_packages(dir.path()).is_empty());
    }

    #[test]
    fn transitive_plugin_named_dependencies_are_not_installed_plugins() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3");
        touch_pkg(dir.path(), "@punktfunk/host", "0.1.2");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"^0.3.1"}}"#,
        )
        .unwrap();

        let found = installed_packages(dir.path());
        assert_eq!(
            found.iter().map(|p| p.pkg.as_str()).collect::<Vec<_>>(),
            vec!["@punktfunk/plugin-rom-manager"],
            "only the top-level install counts"
        );
    }

    #[test]
    fn a_tree_with_no_package_json_falls_back_to_the_convention() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        assert_eq!(installed_packages(dir.path()).len(), 1);
    }

    #[test]
    fn a_fresh_dir_is_seeded_as_its_own_install_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        ensure_plugin_root(&root).unwrap();
        let seeded = std::fs::read_to_string(root.join("package.json")).unwrap();
        assert!(seeded.contains("punktfunk-plugins"), "{seeded}");
        assert!(installed_packages(&root).is_empty());
    }

    #[test]
    fn seeding_never_touches_an_existing_package_json() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        let manifest = r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"0.3.1"}}"#;
        std::fs::write(dir.path().join("package.json"), manifest).unwrap();
        ensure_plugin_root(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
            manifest
        );
    }

    /// Packages present, no `package.json`: [`installed_packages`] uses the
    /// naming convention, and a seeded empty `dependencies` would hide them.
    #[test]
    fn seeding_skips_a_tree_that_already_has_packages() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        ensure_plugin_root(dir.path()).unwrap();
        assert!(!dir.path().join("package.json").exists());
        assert_eq!(installed_packages(dir.path()).len(), 1);
    }

    #[test]
    fn capturing_ancestor_looks_strictly_upwards() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("config/punktfunk/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            capturing_ancestor(&plugins),
            Some(dir.path().join("package.json"))
        );

        // Nearest ancestor is the tree bun would pick.
        let nearer = dir.path().join("config/package.json");
        std::fs::write(&nearer, "{}").unwrap();
        assert_eq!(capturing_ancestor(&plugins), Some(nearer));

        // The dir's own manifest is the anchor, not a capture.
        std::fs::write(plugins.join("package.json"), "{}").unwrap();
        assert_ne!(
            capturing_ancestor(&plugins),
            Some(plugins.join("package.json"))
        );
    }

    #[test]
    fn an_emptied_dependency_list_means_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3"); // leftover transitive
        std::fs::write(dir.path().join("package.json"), r#"{"name":"plugins"}"#).unwrap();
        assert!(installed_packages(dir.path()).is_empty());

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"plugins","dependencies":{}}"#,
        )
        .unwrap();
        assert!(installed_packages(dir.path()).is_empty());
    }

    #[test]
    fn uninstall_target_must_be_a_plugin_package() {
        assert!(valid_installed_pkg("@punktfunk/plugin-rom-manager").is_ok());
        assert!(valid_installed_pkg("@retro-hub/plugin-x").is_ok());
        assert!(valid_installed_pkg("punktfunk-plugin-legacy").is_ok());
        assert!(valid_installed_pkg("effect").is_err());
        assert!(valid_installed_pkg("@punktfunk/host").is_err());
        assert!(valid_installed_pkg("../../etc").is_err());
        assert!(valid_installed_pkg("").is_err());
    }

    #[test]
    fn bunfig_scope_mapping_is_idempotent_and_preserves_other_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let read = || std::fs::read_to_string(dir.path().join("bunfig.toml")).unwrap();

        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://retro.example/npm/").unwrap();
        assert!(read().contains("[install.scopes]"));
        assert!(read().contains("\"@retro-hub\" = \"https://retro.example/npm/\""));

        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://retro.example/npm/").unwrap();
        assert_eq!(read().matches("@retro-hub").count(), 1);

        ensure_bunfig_scope(
            dir.path(),
            "@punktfunk",
            "https://git.unom.io/api/packages/unom/npm/",
        )
        .unwrap();
        assert!(read().contains("@punktfunk"));
        assert!(read().contains("@retro-hub"));

        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://new.example/npm/").unwrap();
        assert_eq!(read().matches("@retro-hub").count(), 1);
        assert!(read().contains("https://new.example/npm/"));
        assert!(!read().contains("retro.example"));
        assert!(read().contains("@punktfunk"), "unrelated scope survives");
    }

    #[test]
    fn bunfig_scope_mapping_refuses_junk() {
        let dir = tempfile::tempdir().unwrap();
        // Second check at the one place we format TOML by hand.
        assert!(ensure_bunfig_scope(dir.path(), "@x", "http://insecure/").is_err());
        assert!(ensure_bunfig_scope(dir.path(), "no-at-sign", "https://e/").is_err());
        assert!(ensure_bunfig_scope(dir.path(), "@bad\"quote", "https://e/").is_err());
        assert!(!dir.path().join("bunfig.toml").exists());
    }

    /// `Entry::registry` is not sanitized. A URL that closes `"{url}"` injects
    /// a top-level `[install]` table that outlives the source.
    #[test]
    fn bunfig_registry_url_cannot_inject_a_toml_table() {
        let dir = tempfile::tempdir().unwrap();
        let injection = "https://ok.example/\"\n[install]\nregistry = \"https://evil.example/";
        assert!(
            ensure_bunfig_scope(dir.path(), "@x", injection).is_err(),
            "a registry URL that closes the TOML string must be refused"
        );
        assert!(!dir.path().join("bunfig.toml").exists());

        for bad in [
            "https://e/\"quote",
            "https://e/\nnewline",
            "https://e/\rcarriage",
            "https://e/ space",
            "https://e/\ttab",
            "https://e/back\\slash",
            "https://e/nul\0byte",
        ] {
            assert!(
                ensure_bunfig_scope(dir.path(), "@x", bad).is_err(),
                "must refuse registry URL {bad:?}"
            );
        }
        for good in [
            "https://git.unom.io/api/packages/unom/npm/",
            "https://registry.example.com:8443/npm/",
            "https://example.com/npm/?token=abc%20def",
        ] {
            assert!(
                ensure_bunfig_scope(dir.path(), "@x", good).is_ok(),
                "must accept registry URL {good:?}"
            );
        }
    }

    /// [`valid_installed_pkg`] accepts `@punktfunk/plugin-kit`. Uninstall must
    /// also require membership in [`installed_packages`].
    #[test]
    fn name_shape_alone_does_not_protect_a_plugins_framework() {
        assert!(
            valid_installed_pkg("@punktfunk/plugin-kit").is_ok(),
            "shape check passes plugin-kit — the handler must additionally require that the \
             package is a top-level install"
        );

        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"0.3.1"}}"#,
        )
        .unwrap();
        let installed = installed_packages(dir.path());
        assert!(installed
            .iter()
            .any(|p| p.pkg == "@punktfunk/plugin-rom-manager"));
        assert!(
            !installed.iter().any(|p| p.pkg == "@punktfunk/plugin-kit"),
            "the framework is not an installed plugin, so uninstall must refuse it"
        );
    }

    #[test]
    fn plugin_id_derivation() {
        assert_eq!(
            plugin_id_for_pkg("@punktfunk/plugin-rom-manager").as_deref(),
            Some("rom-manager")
        );
        assert_eq!(
            plugin_id_for_pkg("punktfunk-plugin-playnite").as_deref(),
            Some("playnite")
        );
        assert_eq!(plugin_id_for_pkg("@a/plugin-x").as_deref(), Some("x"));
        assert_eq!(plugin_id_for_pkg("effect"), None);
    }
}
