//! What an installed plugin declares about itself: the paths it reads, whether it needs the
//! network, and the commands the host may run on its behalf.
//!
//! The manifest is a `punktfunk` block in the package's own `package.json`, so it is part of the
//! reviewed, hash-pinned tarball rather than something a running plugin can choose. It is the only
//! source of a plugin-owned command: the host builds every argv from a template here, with the
//! parameters validated per launch, instead of running a string the plugin answers with.
//!
//! Pin: `manifest_tests` below, and `library::launch::exec` for the launch half.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One argv template the host may run for this plugin.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecTemplate {
    /// A bare program name resolved from `PATH`, or an absolute path under a declared root.
    pub exe: String,
    /// argv after the program. `{param}` is replaced by a validated value, never by a shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// Parameter name → what a value may look like. Anything not listed cannot be passed.
    #[serde(default)]
    pub params: BTreeMap<String, ParamKind>,
    /// Working directory, if the program needs one. Absolute, under a declared root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The shapes a template parameter may take. Deliberately a closed set of character classes
/// rather than plugin-supplied patterns: a regex from a package is a validator nobody reviewed.
///
/// No value may begin with `-`: it becomes one argv element, and a program that reads it as a
/// flag is the one way a validated value still changes what runs.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    /// Printable text — a game or bottle name, which legitimately carries punctuation. Safe
    /// because it is quoted into exactly one argv element, never parsed as syntax.
    Name,
    /// `[A-Za-z0-9._-]`, for ids and core names.
    Id,
    /// ASCII digits.
    Digits,
    /// An absolute path, checked against the plugin's declared roots at launch.
    Path,
    /// Extra argv elements — the operator's own flags for this program, each its own element.
    /// The one kind that may start with `-`, because flags are what it is for, and the one that
    /// may be given more than once. The program itself still comes from the template.
    Args,
}

/// A plugin's `punktfunk` block.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginManifest {
    /// Must be 1. A later schema is refused rather than guessed at.
    #[serde(default)]
    pub schema: u32,
    /// The id the plugin registers under (`definePlugin({ name })`), which is also its provider
    /// id in the library. How a launch finds the templates it may use.
    #[serde(default)]
    pub id: String,
    /// Paths the plugin reads. Absolute, or `~`-rooted.
    #[serde(default)]
    pub reads: Vec<String>,
    /// Paths it also writes.
    #[serde(default)]
    pub writes: Vec<String>,
    /// Whether it needs to reach the network at all.
    #[serde(default)]
    pub network: bool,
    /// Templates the host may run, by name.
    #[serde(default)]
    pub exec: BTreeMap<String, ExecTemplate>,
}

impl PluginManifest {
    /// What the manifest itself declares: expanded `reads` + `writes`, without operator grants.
    pub fn declared_roots(&self) -> Vec<PathBuf> {
        self.reads
            .iter()
            .chain(self.writes.iter())
            .flat_map(|p| expand_home(p))
            .collect()
    }

    /// Every root this plugin may reach: what it declared, plus what the operator granted it.
    ///
    /// A package cannot know where someone keeps their ROMs or installs their games, so the
    /// grants are how those paths become usable without the package asking for the whole disk.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.declared_roots()
            .into_iter()
            .chain(granted_roots(&self.id))
            .collect()
    }

    /// Is `candidate` inside one of the declared roots or grants? A `..` segment is refused
    /// outright. Lexical first; a path that resolves then matches through its canonical form,
    /// because `/home` may be a link (`/var/home` on Fedora Atomic) and grants are stored
    /// canonical. Windows compares the way grants are stored (`\\?\`, either slash, any case).
    pub fn confines(&self, candidate: &Path) -> bool {
        if !candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        let roots = self.roots();
        if roots
            .iter()
            .any(|root| super::access::within(candidate, root))
        {
            return true;
        }
        let Ok(real) = candidate.canonicalize() else {
            return false;
        };
        roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| super::access::within(&real, &root))
    }
}

/// `~/x` under every home it may mean ([`plugin_homes`]); any other path as written.
fn expand_home(p: &str) -> Vec<PathBuf> {
    match p.strip_prefix("~/") {
        Some(rest) => plugin_homes().into_iter().map(|h| h.join(rest)).collect(),
        None => vec![PathBuf::from(p)],
    }
}

/// The homes a manifest's `~` names. The Windows host is a service whose own profile is
/// `systemprofile`, where no launcher is ever installed, so there it is every real user profile.
fn plugin_homes() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        // `C:` joined with `Users` is drive-relative, so the separator is spelled out.
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let base = PathBuf::from(format!("{drive}\\Users"));
        let people = std::fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                !matches!(
                    name.as_str(),
                    "public" | "default" | "default user" | "all users"
                )
            })
            .map(|e| e.path());
        let mut homes: Vec<PathBuf> = people.chain(home_dir()).collect();
        homes.dedup();
        homes
    }
    #[cfg(not(windows))]
    home_dir().into_iter().collect()
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Does `value` match `kind`? Length is capped here too: every value ends up as one argv element.
pub fn param_ok(kind: ParamKind, value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    if kind == ParamKind::Args {
        return value.len() <= 256;
    }
    if value.starts_with('-') {
        return false;
    }
    match kind {
        ParamKind::Name => value.len() <= 128,
        ParamKind::Id => {
            value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }
        ParamKind::Digits => value.len() <= 32 && value.bytes().all(|b| b.is_ascii_digit()),
        ParamKind::Path => value.len() <= 4096 && Path::new(value).is_absolute(),
        // Handled above, before the leading-dash refusal.
        ParamKind::Args => true,
    }
}

/// [`granted_roots`] against a chosen store dir, so a test never opens the real config.
/// `~/` spells in a v1 grant expand through [`expand_home`], the same rule declared roots
/// follow.
pub(crate) fn granted_roots_in(id: &str, config_dir: PathBuf) -> Vec<PathBuf> {
    crate::plugins::access::AccessStore::open(config_dir)
        .grants_for(id)
        .into_iter()
        .flat_map(|g| expand_home(&g.path))
        .collect()
}

/// Extra roots the operator granted a plugin, by id: `<config>/plugin-run/plugin-grants.json` via
/// [`crate::plugins::access::AccessStore`].
///
/// Written by `punktfunk-host plugins grant` or an operator's `allow` decision, never by a
/// plugin — the file is the operator's answer to "this package may also reach here", so
/// nothing in the plugin lane may edit it.
pub fn granted_roots(id: &str) -> Vec<PathBuf> {
    granted_roots_in(id, pf_paths::config_dir())
}

/// Every installed plugin's manifest, keyed by the id it declares.
///
/// Installed means what the runner discovers: the dependencies of `<config>/plugins/package.json`,
/// not every package under `node_modules`, where a plugin's own libraries live too. Without that
/// file the tree is scanned. An id is one lowercase path component, and an id two packages claim
/// is refused for both. Read fresh: installs and updates land between launches.
pub fn installed() -> BTreeMap<String, PluginManifest> {
    installed_in(&pf_paths::config_dir().join("plugins"))
}

fn installed_in(plugins: &Path) -> BTreeMap<String, PluginManifest> {
    let modules = plugins.join("node_modules");
    let dirs = match top_level_packages(plugins) {
        Some(names) => names.iter().map(|n| modules.join(n)).collect(),
        None => package_dirs(&modules),
    };
    let mut out = BTreeMap::new();
    let mut claimed_twice = std::collections::BTreeSet::new();
    for dir in dirs {
        let Some(manifest) = read_package(&dir) else {
            continue;
        };
        if manifest.schema != 1 || !valid_id(&manifest.id) {
            tracing::warn!(
                package = %dir.display(),
                schema = manifest.schema,
                id = %manifest.id,
                "plugin manifest unusable: schema must be 1 and the id one lowercase path component"
            );
            continue;
        }
        if out.contains_key(&manifest.id) {
            claimed_twice.insert(manifest.id.clone());
            continue;
        }
        out.insert(manifest.id.clone(), manifest);
    }
    for id in claimed_twice {
        tracing::warn!(%id, "plugin manifest refused: two installed packages claim this id");
        out.remove(&id);
    }
    out
}

/// The runner's rule for the same id: it names a state dir and a socket.
pub(crate) fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The plugins dir's own `dependencies`, or `None` when it has no readable `package.json`.
fn top_level_packages(plugins: &Path) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        dependencies: BTreeMap<String, serde_json::Value>,
    }
    let text = std::fs::read_to_string(plugins.join("package.json")).ok()?;
    let root = serde_json::from_str::<Root>(&text).ok()?;
    let plain = |n: &String| {
        Path::new(n)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    };
    Some(root.dependencies.into_keys().filter(plain).collect())
}

/// The manifest declaring `id`, if one is installed.
pub fn for_provider(id: &str) -> Option<PluginManifest> {
    installed().remove(id)
}

/// The provider id package `pkg` declares, read before an uninstall takes its files away.
pub fn id_of_package(pkg: &str) -> Option<String> {
    let dir = pf_paths::config_dir()
        .join("plugins")
        .join("node_modules")
        .join(pkg);
    read_package(&dir).map(|m| m.id).filter(|id| valid_id(id))
}

/// `node_modules/<name>` plus `node_modules/@scope/<name>`.
fn package_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let scoped = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('@'));
        if scoped {
            out.extend(
                std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        p.is_dir().then_some(p)
                    }),
            );
        } else {
            out.push(path);
        }
    }
    out
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    punktfunk: Option<PluginManifest>,
}

fn read_package(dir: &Path) -> Option<PluginManifest> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    match serde_json::from_str::<PackageJson>(&text) {
        Ok(pkg) => pkg.punktfunk,
        Err(e) => {
            tracing::warn!(package = %dir.display(), error = %e, "plugin manifest: unreadable");
            None
        }
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    fn manifest(reads: &[&str]) -> PluginManifest {
        PluginManifest {
            schema: 1,
            // An id no grants file in a test environment carries.
            id: "demo-no-grants".into(),
            reads: reads.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn package(modules: &Path, name: &str, id: &str) {
        let dir = modules.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({ "name": name, "punktfunk": { "schema": 1, "id": id } });
        std::fs::write(dir.join("package.json"), body.to_string()).unwrap();
    }

    #[test]
    fn installed_means_what_the_runner_discovers() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path();
        let modules = plugins.join("node_modules");
        package(&modules, "@punktfunk/plugin-steam", "steam");
        package(&modules, "@punktfunk/plugin-kit", "kit-library");
        package(&modules, "plugin-twin-a", "twin");
        package(&modules, "plugin-twin-b", "twin");
        package(&modules, "plugin-bad", "../plugins");
        let deps = serde_json::json!({ "dependencies": {
            "@punktfunk/plugin-steam": "0.2.2",
            "plugin-twin-a": "1", "plugin-twin-b": "1", "plugin-bad": "1",
        }});
        std::fs::write(plugins.join("package.json"), deps.to_string()).unwrap();
        // A library is not a plugin, a claimed-twice id is nobody's, a path is not an id.
        assert_eq!(
            installed_in(plugins).into_keys().collect::<Vec<_>>(),
            vec!["steam"]
        );
        // Without the root package.json the tree is scanned, as the runner does.
        std::fs::remove_file(plugins.join("package.json")).unwrap();
        assert!(installed_in(plugins).contains_key("kit-library"));
    }

    #[test]
    fn params_take_only_their_own_shape() {
        assert!(param_ok(ParamKind::Name, "Hollow Knight: Silksong"));
        assert!(param_ok(ParamKind::Name, "Sam & Max #2"));
        // Shell syntax is harmless in one quoted argv element; a newline or a leading dash is not.
        assert!(param_ok(ParamKind::Name, "a; rm -rf /"));
        assert!(!param_ok(ParamKind::Name, "two\nlines"));
        assert!(!param_ok(ParamKind::Name, "--output=/etc/passwd"));
        assert!(!param_ok(ParamKind::Path, "-/games/x.sfc"));
        assert!(param_ok(ParamKind::Id, "snes9x_libretro"));
        assert!(!param_ok(ParamKind::Id, "../etc/passwd"));
        assert!(param_ok(ParamKind::Digits, "440"));
        assert!(!param_ok(ParamKind::Digits, "44 0"));
        assert!(param_ok(ParamKind::Args, "--fullscreen"));
        assert!(!param_ok(ParamKind::Args, "--x\ny"));
        assert!(param_ok(ParamKind::Path, "/games/rom.sfc"));
        assert!(!param_ok(ParamKind::Path, "rom.sfc"));
        assert!(!param_ok(ParamKind::Path, ""));
    }

    #[test]
    fn confinement_refuses_traversal_and_foreign_roots() {
        let m = manifest(&["/games", "/opt/emu"]);
        assert!(m.confines(Path::new("/games/snes/rom.sfc")));
        assert!(m.confines(Path::new("/opt/emu/cores/x.so")));
        assert!(!m.confines(Path::new("/etc/shadow")));
        assert!(!m.confines(Path::new("/games/../etc/shadow")));
        assert!(!m.confines(Path::new("relative/path")));
    }

    /// Fedora Atomic: a root spelled under the `/home` link confines the canonical `/var/home`
    /// path a plugin reports, and a canonical grant confines the linked spelling.
    #[cfg(unix)]
    #[test]
    fn confinement_sees_through_a_linked_home() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let roms = root.join("var/home/u/roms");
        std::fs::create_dir_all(&roms).unwrap();
        std::fs::write(roms.join("x.sfc"), "rom").unwrap();
        std::os::unix::fs::symlink(root.join("var/home"), root.join("home")).unwrap();
        let linked = root.join("home/u/roms");
        let m = manifest(&[linked.to_str().unwrap()]);
        assert!(m.confines(&roms.join("x.sfc")));
        let m = manifest(&[roms.to_str().unwrap()]);
        assert!(m.confines(&linked.join("x.sfc")));
        assert!(!m.confines(&root.join("var/home/u/other")));
    }

    #[test]
    fn expand_home_roots_only_a_tilde_prefix() {
        if let Some(home) = home_dir() {
            assert!(expand_home("~/legacy").contains(&home.join("legacy")));
            assert_eq!(expand_home("/abs/path"), vec![PathBuf::from("/abs/path")]);
        }
    }

    #[test]
    fn a_package_block_round_trips() {
        let json = r#"{"name":"@punktfunk/plugin-bottles","punktfunk":{
            "schema":1,"id":"bottles","reads":["~/.local/share/bottles"],"network":false,
            "exec":{"run":{"exe":"flatpak","args":["run","com.usebottles.bottles","-b","{bottle}"],
                           "params":{"bottle":"name"}}}}}"#;
        let m: PluginManifest = serde_json::from_str::<PackageJson>(json)
            .unwrap()
            .punktfunk
            .unwrap();
        assert_eq!(m.id, "bottles");
        assert_eq!(m.exec["run"].exe, "flatpak");
        assert_eq!(m.exec["run"].params["bottle"], ParamKind::Name);
    }
}
