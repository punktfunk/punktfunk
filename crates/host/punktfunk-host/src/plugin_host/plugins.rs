//! `punktfunk-host plugins …` — install plugins and opt in the runner.
//!
//! Package ops (`add`/`remove`/`list`) go to the bun runner (`sdk/src/plugins.ts`):
//! this binary locates it; it owns the vendored bun, `@punktfunk` scope, and plugins dir.
//! Service ops (`enable`/`disable`/`status`) run here — `systemctl --user` or the
//! `PunktfunkScripting` scheduled task — so they work without the runner package.
//!
//! Windows: both halves need elevation (`%ProgramData%\punktfunk` is ACL'd;
//! the task is admin-owned). Refuse unelevated rather than a bare EACCES from `bun add`.
//!
//! The task runs as `NT AUTHORITY\LocalService`, not SYSTEM. `enable` converges the
//! principal and grants LocalService read on `plugin-token` plus the TLS-pin cert
//! (`native-cert.pem` or legacy `cert.pem`) — never `mgmt-token`.
//!
//! Runner discovery is pinned in this module's tests.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Files the runner consumes through one directory bind. Binding the directory keeps atomic
/// replacements visible inside the runner's mount namespace.
pub(crate) const RUNNER_DATA_DIR: &str = "plugin-run";

pub mod access;
#[cfg(any(test, target_os = "windows"))]
mod ace_plan;
pub mod manifest;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use self::windows as plat;
#[cfg(not(target_os = "windows"))]
mod posix;
#[cfg(not(target_os = "windows"))]
use self::posix as plat;

/// What `plugins <args…>` asks for. [`parse`] decides it; [`main`] runs it.
#[derive(Debug, PartialEq)]
enum Action<'a> {
    /// `add`, `remove`, `list`: the runner does the work. `removing` is the packages whose
    /// library titles leave with them.
    Package {
        listing: bool,
        removing: Vec<&'a str>,
    },
    Enable,
    Disable,
    Status,
    Spawn(&'a [String]),
    /// The plugin and folder operands, then the flags after them.
    Grant(Option<&'a str>, Option<&'a str>, &'a [String]),
    Revoke(Option<&'a str>, Option<&'a str>, &'a [String]),
    Access(&'a [String]),
    Help,
}

/// The action `args` names. An empty `args` is `Help`; a missing operand stays `None` for the
/// command's own usage line.
fn parse(args: &[String]) -> Result<Action<'_>> {
    let word = |i: usize| args.get(i).map(String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    let flags = args.get(3..).unwrap_or_default();
    Ok(match word(0) {
        Some("add") => Action::Package {
            listing: false,
            removing: Vec::new(),
        },
        Some("remove" | "rm" | "uninstall") => Action::Package {
            listing: false,
            removing: operands(rest),
        },
        Some("list" | "ls") => Action::Package {
            listing: true,
            removing: Vec::new(),
        },
        Some("enable") => Action::Enable,
        Some("disable") => Action::Disable,
        Some("status") => Action::Status,
        Some("spawn") => Action::Spawn(rest),
        Some("grant") => Action::Grant(word(1), word(2), flags),
        Some("access") => Action::Access(rest),
        Some("revoke") => Action::Revoke(word(1), word(2), flags),
        Some("-h" | "--help" | "help") | None => Action::Help,
        Some(other) => bail!("unknown plugins command '{other}' (try `plugins --help`)"),
    })
}

/// The package names in `args`: what the runner's `positionals()` keeps. Flags go, and so does
/// the value after `--plugins`, `--scripts` and `--registry`.
fn operands(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut args = args.iter().map(String::as_str);
    while let Some(a) = args.next() {
        if matches!(a, "--plugins" | "--scripts" | "--registry") {
            args.next();
        } else if !a.starts_with('-') {
            out.push(a);
        }
    }
    out
}

/// The plugins dir a package op acts on: the value of `--plugins`, else the default.
fn plugins_dir_of(args: &[String]) -> std::path::PathBuf {
    args.iter()
        .position(|a| a == "--plugins")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::store::plugins_dir)
}

/// Run `plugins <args…>`. `delete_titles` clears a removed plugin's library titles.
pub fn main(args: &[String], delete_titles: super::DeleteTitles) -> Result<()> {
    match parse(args)? {
        Action::Package { listing, removing } => {
            package_op(args, listing, &removing, delete_titles)
        }
        Action::Enable => {
            plat::require_elevation("enabling the plugin runner")?;
            plat::enable()
        }
        Action::Disable => {
            plat::require_elevation("disabling the plugin runner")?;
            plat::disable()
        }
        Action::Status => status(),
        Action::Spawn(rest) => spawn(rest),
        Action::Grant(plugin, dir, flags) => grant(plugin, dir, flags),
        Action::Access(flags) => access_list(flags),
        Action::Revoke(plugin, dir, flags) => revoke(plugin, dir, flags),
        Action::Help => {
            print_usage();
            Ok(())
        }
    }
}

/// `add`, `remove` or `list` through the runner, then what an install or removal leaves to do
/// here: the removed plugins' titles, the plugins' credentials, the runner's restart.
fn package_op(
    args: &[String],
    listing: bool,
    removing: &[&str],
    delete_titles: super::DeleteTitles,
) -> Result<()> {
    if !listing {
        plat::require_elevation("installing or removing plugins")?;
    }
    // A removed plugin's titles leave with it; its id is read from the dir the runner removes
    // from, before its files are gone.
    let dir = plugins_dir_of(args);
    let providers: Vec<String> = removing
        .iter()
        .filter_map(|pkg| manifest::id_of_package(&dir, pkg))
        .collect();
    forward_to_runner(args)?;
    for provider in providers {
        if let Err(e) = delete_titles(&provider) {
            println!("Couldn't remove the library titles of {provider}: {e:#}");
        }
    }
    if listing {
        return Ok(());
    }
    settle_runtime(
        crate::mgmt_token::load_or_generate_per_plugin,
        restart_runtime,
    )
}

/// The tail of an install or removal: the plugins' credentials, then the runner's restart. The
/// runner discovers units at startup, so it restarts even when no token was issued; that
/// failure is the error returned after.
fn settle_runtime<T>(
    issue_tokens: impl FnOnce() -> Result<T>,
    restart: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    let issued = issue_tokens();
    match restart() {
        Ok(true) => println!("Plugin runner restarted."),
        Ok(false) => println!("The plugin runner is off — `plugins enable` starts it."),
        Err(e) => println!("Couldn't restart the plugin runner: {e:#}"),
    }
    issued
        .map(drop)
        .context("issue the plugins' API credentials")
}

/// `plugins grant <plugin> <dir> [--write]` — the operator's answer to "this package may also
/// reach here". Grants bind read-only; `--write` is the exception and says so on the record.
///
/// A package declares the standard locations it knows; where someone keeps their ROMs or installs
/// their games is not something it can know, and this is how that path becomes usable without the
/// package asking for the home directory.
fn grant(plugin: Option<&str>, dir: Option<&str>, flags: &[String]) -> Result<()> {
    let mut write = false;
    for flag in flags {
        match flag.as_str() {
            "--write" => write = true,
            other => bail!("unknown flag '{other}' (the only flag is --write)"),
        }
    }
    let (Some(plugin), Some(dir)) = (
        plugin.map(str::trim).filter(|s| !s.is_empty()),
        dir.map(str::trim).filter(|s| !s.is_empty()),
    ) else {
        bail!("usage: punktfunk-host plugins grant <plugin> <dir> [--write]");
    };
    let path = std::path::Path::new(dir);
    // A typo must not report success: the grant would name a directory nothing ever reads.
    if !path.is_dir() {
        bail!("'{dir}' is not a directory (grant the folder, not a file inside it)");
    }
    let store = access::AccessStore::open(pf_paths::config_dir());
    let roots = store
        .grant(plugin, path, write, "cli")
        .with_context(|| format!("record the grant for '{plugin}'"))?;
    println!("{plugin} may now reach:");
    for root in roots {
        println!(
            "  {} ({})",
            root.path,
            if root.write {
                "read and write"
            } else {
                "read only"
            }
        );
    }
    converge_after_decision();
    println!("The plugin restarts with the new folder within a few seconds.");
    Ok(())
}

/// `plugins access` — every plugin's grants, pending requests, and denials.
fn access_list(flags: &[String]) -> Result<()> {
    if let Some(flag) = flags.first() {
        bail!("unknown flag '{flag}' (`plugins access` takes none)");
    }
    let store = access::AccessStore::open(pf_paths::config_dir());
    let snapshots = store.snapshot().context("read plugin access records")?;
    if snapshots
        .iter()
        .all(|s| s.grants.is_empty() && s.pending.is_empty() && s.denied.is_empty())
    {
        println!("No plugin folder access is recorded.");
        return Ok(());
    }
    for s in snapshots {
        println!("{plugin}:", plugin = s.plugin);
        for g in &s.grants {
            println!(
                "  granted  {} ({}, by {})",
                g.path,
                if g.write {
                    "read and write"
                } else {
                    "read only"
                },
                g.by
            );
        }
        for p in &s.pending {
            println!(
                "  pending  {} ({})",
                p.path,
                if p.write {
                    "read and write"
                } else {
                    "read only"
                }
            );
        }
        for d in &s.denied {
            println!("  denied   {d}");
        }
    }
    Ok(())
}

/// `plugins spawn --package <id> -- <exe> [args…]`: the runner's way into a plugin's
/// AppContainer. Runs as the runner's account, which owns the profile; exits with the
/// plugin's own code. The capabilities come from the installed manifest, never from argv.
#[cfg(windows)]
fn spawn(args: &[String]) -> Result<()> {
    let (id, rest) = match args {
        [flag, id, rest @ ..] if flag == "--package" => (id.as_str(), rest),
        _ => bail!("usage: punktfunk-host plugins spawn --package <id> -- <exe> [args…]"),
    };
    let rest = rest.strip_prefix(&["--".to_string()][..]).unwrap_or(rest);
    let Some((exe, exe_args)) = rest.split_first() else {
        bail!("usage: punktfunk-host plugins spawn --package <id> -- <exe> [args…]");
    };
    let Some(manifest) = manifest::installed().remove(id) else {
        bail!("{id} is not an installed plugin with a manifest");
    };
    let code = crate::windows::app_container::spawn(
        id,
        manifest.network,
        std::path::Path::new(exe),
        exe_args,
    )?;
    std::process::exit(i32::try_from(code).unwrap_or(1));
}

#[cfg(not(windows))]
fn spawn(_args: &[String]) -> Result<()> {
    bail!("`plugins spawn` is the Windows runner's AppContainer entry; Linux uses bubblewrap")
}

/// `plugins revoke <plugin> <dir>` — remove one grant by its recorded path.
fn revoke(plugin: Option<&str>, dir: Option<&str>, flags: &[String]) -> Result<()> {
    if let Some(flag) = flags.first() {
        bail!("unknown flag '{flag}' (`plugins revoke` takes none)");
    }
    let (Some(plugin), Some(dir)) = (
        plugin.map(str::trim).filter(|s| !s.is_empty()),
        dir.map(str::trim).filter(|s| !s.is_empty()),
    ) else {
        bail!("usage: punktfunk-host plugins revoke <plugin> <dir>");
    };
    let store = access::AccessStore::open(pf_paths::config_dir());
    let roots = store
        .revoke(plugin, std::path::Path::new(dir))
        .with_context(|| format!("revoke the grant for '{plugin}'"))?;
    converge_after_decision();
    println!("{plugin} may now reach:");
    for root in roots {
        println!(
            "  {} ({})",
            root.path,
            if root.write {
                "read and write"
            } else {
                "read only"
            }
        );
    }
    Ok(())
}

fn print_usage() {
    eprintln!(
        "punktfunk-host plugins — install and run host plugins

USAGE:
    punktfunk-host plugins add <name…>       install a plugin (playnite, rom-manager, …)
    punktfunk-host plugins remove <name…>    uninstall a plugin
    punktfunk-host plugins list              list installed plugins
    punktfunk-host plugins enable            enable + start the plugin runner (opt-in)
    punktfunk-host plugins disable           stop + disable the plugin runner
    punktfunk-host plugins status            is the runner enabled/running?
    punktfunk-host plugins grant <plugin> <dir> [--write]
                                             let one plugin reach a directory of yours (a ROM
                                             library, a game install dir) — nothing else on
                                             this box changes; read-only unless --write
    punktfunk-host plugins access            list every plugin's granted, pending and denied folders
    punktfunk-host plugins revoke <plugin> <dir>
                                             take one granted directory back

NAMES:
    A bare first-party name resolves into the @punktfunk scope: `playnite` installs
    @punktfunk/plugin-playnite, `rom-manager` installs @punktfunk/plugin-rom-manager —
    always from Punktfunk's own package registry. Any other name (`punktfunk-plugin-*`,
    a foreign @scope) installs from the PUBLIC npm registry and is refused unless you
    pass --allow-public-registry.

NOTES:
    Plugins run under the runner, which is OPT-IN — `plugins add` installs, `plugins enable`
    turns the runner on. Plugins are operator-installed code that runs with operator
    privileges; install only plugins you trust.
"
    );
    plat::usage_note();
}

// ---- package ops: forward to the bun runner ---------------------------------------------------

fn forward_to_runner(args: &[String]) -> Result<()> {
    // `bun add` walks up to the nearest `package.json`, so seed the plugins dir first or a
    // stray `~/package.json` captures the install (exit 0). The installed runner may predate
    // this binary (`store::ensure_plugin_root`).
    if args.first().map(String::as_str) == Some("add") {
        let dir = plugins_dir_of(args);
        crate::store::ensure_plugin_root(&dir)
            .with_context(|| format!("prepare {}", dir.display()))?;
    }
    let (program, prefix) = runner_command()?;
    let status = Command::new(&program)
        .args(&prefix)
        .args(args)
        .status()
        .with_context(|| format!("run the plugin runner ({})", program.display()))?;
    if !status.success() {
        // The runner already printed the reason; do not add a second error line.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// The runner's launch line. Also the store job executor ([`crate::store::jobs`]): console
/// installs use this same invocation so the box has one "install a plugin" path.
pub(crate) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    plat::runner_command()
}

// ---- service ops ------------------------------------------------------------------------------

fn status() -> Result<()> {
    let st = runtime_status();
    println!(
        "runner:  {}\nstate:   {}\nenabled: {}",
        st.unit,
        if !st.installed {
            "not installed"
        } else if st.running {
            "running"
        } else {
            "stopped"
        },
        st.enabled
    );
    if let Some(principal) = &st.principal {
        println!("runs as: {principal}");
    }
    if st.installed && !st.running {
        println!("\nStart it with: punktfunk-host plugins enable");
    } else if !st.installed {
        println!("\n{}", st.detail);
    }
    Ok(())
}

// ---- runtime state, shared by the CLI and the plugin store's mgmt API --------------------------

/// Data for the store console (offer enable before first install; explain why a
/// just-installed plugin is not running). Not formatted for stdout.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeStatus {
    pub installed: bool,
    /// systemd `enabled`, or a non-`Disabled` scheduled task.
    pub enabled: bool,
    pub running: bool,
    pub unit: &'static str,
    pub principal: Option<String>,
    pub detail: String,
}

pub(crate) fn runtime_status() -> RuntimeStatus {
    plat::runtime_status()
}

/// Has the operator turned the per-plugin sandbox off for the runner?
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn runner_sandbox_off() -> bool {
    plat::runner_sandbox_off()
}

/// Why the runner stopped at its last start instead of running a plugin unsandboxed, in its own
/// words. It leaves `plugin-state/runner.refused` then and removes it at the next start that
/// gets past its probe. The host can't repeat that probe: it runs outside the runner's unit.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn sandbox_refusal() -> Option<String> {
    sandbox_refusal_in(&pf_paths::config_dir())
}

/// One line, no control characters, 300 at most: the runner's text ends up in the console.
#[cfg(any(test, target_os = "linux", windows))]
fn sandbox_refusal_in(config_dir: &std::path::Path) -> Option<String> {
    let file = config_dir.join("plugin-state").join("runner.refused");
    let text = std::fs::read_to_string(file).ok()?;
    let line = text.lines().next().unwrap_or_default().trim();
    let plain = |c: char| if c.is_control() { ' ' } else { c };
    Some(line.chars().map(plain).take(300).collect())
}

/// [`enable`]/[`disable`], also `POST /store/runtime`. Windows: the SYSTEM service
/// already clears the elevation bar the CLI checks.
pub(crate) fn set_runtime_enabled(enabled: bool) -> Result<()> {
    if enabled {
        plat::enable()
    } else {
        plat::disable()
    }
}

/// The ACL half of a grant, as `io::Error` for [`access::AccessStore`]: the runner's account,
/// and `plugin`'s files capability when it is named, in one walk of the folder. POSIX needs
/// none: the runner is the operator's own user unit.
pub(crate) fn grant_acl(
    plugin: Option<&str>,
    dir: &std::path::Path,
    write: bool,
) -> std::io::Result<()> {
    plat::grant(plugin, dir, write).map_err(|e| std::io::Error::other(e.to_string()))
}

/// A managed emulator runs in the player's session and, portable, writes beside its exe: on
/// Windows the folder gets `Users: Modify`. Best-effort; POSIX needs none.
#[cfg(windows)]
pub(crate) fn open_for_players(dir: &std::path::Path) {
    let ok = std::process::Command::new(windows::icacls_path())
        .arg(dir)
        .args(["/grant", "*S-1-5-32-545:(OI)(CI)M"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        tracing::warn!(dir = %dir.display(), "emulator folder not opened to players");
    }
}

#[cfg(not(windows))]
pub(crate) fn open_for_players(_dir: &std::path::Path) {}

/// Take the runner's ACE and `plugin`'s capability ACE off a folder no plugin holds any more, in
/// one walk. POSIX has none to take.
pub(crate) fn revoke_acl(plugin: &str, dir: &std::path::Path) -> std::io::Result<()> {
    plat::revoke(plugin, dir).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Windows: the runner grants `plugins enable` applies, which a fresh install never ran.
/// `serve` calls this; it acts once per install. POSIX needs none.
pub(crate) fn converge_runner_acls(status: &RuntimeStatus) {
    plat::converge_runner_acls(status);
}

/// Windows: close the ingest inbox and the tray token to seat accounts. `serve` calls this, and
/// so does a seat's creation, which can make the group first. No-op off Windows.
pub(crate) fn converge_seat_denies() {
    #[cfg(target_os = "windows")]
    plat::deny_seats();
}

/// Windows: installs before 0.44 granted `BUILTIN\Users` read on every config subdirectory.
/// `serve` strips it once per install. POSIX dirs are 0700 from birth.
#[cfg(windows)]
pub(crate) fn converge_config_dir_acls() {
    plat::strip_users_read_once();
}

/// Keep a rewritten runner credential readable by the enabled Windows service. POSIX runners
/// inherit access from the operator and need no ACL adjustment.
pub(crate) fn converge_runner_credential(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let st = runtime_status();
        if st.installed && st.enabled {
            plat::grant_runner_credential(path)?;
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = path;
    Ok(())
}

/// Restore the enabled Windows runner's directory ACE after secret-directory hardening. POSIX
/// uses the operator's own account for both processes.
pub(crate) fn converge_runner_data_dir(dir: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let st = runtime_status();
        if st.installed && st.enabled {
            plat::grant_runner_data_dir(dir)?;
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = dir;
    Ok(())
}

/// Restart so the runner rediscovers units. `false` when it is off — not an error;
/// the store reports "installed, but off".
///
/// Discovery runs once at runner startup ([`sdk/src/runner.ts`]); this restart is
/// how a newly installed plugin becomes active. An enabled runner that is not running
/// (installed after login, crashed out) is started, not skipped. The unit's roots are
/// converged first: a new manifest may name paths the runner cannot see yet.
pub(crate) fn restart_runtime() -> Result<bool> {
    converge_runner_roots();
    let st = runtime_status();
    if !st.installed || !st.enabled {
        return Ok(false);
    }
    plat::restart_runtime()?;
    Ok(true)
}

/// [`restart_runtime`] off the caller's thread, for a start the running runner predates: one
/// that minted its token or moved the management port. A runner already up holds what it saw
/// at its own start: on Linux its unit bound only the token, TLS pin and endpoint file that
/// existed then, and a rename leaves it reading the old ones; on Windows it had none of its
/// grants.
pub(crate) fn restart_stale_runtime() {
    let spawned = std::thread::Builder::new()
        .name("runner-restart".into())
        .spawn(|| match restart_runtime() {
            Ok(true) => {
                tracing::info!("plugin runner restarted onto the host's credentials and port")
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "plugin runner not restarted"),
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "plugin runner not restarted");
    }
}

/// The serving host's first converge places every runner ACE again, not only the changed ones.
#[cfg(windows)]
pub(crate) fn recheck_runner_roots() {
    plat::recheck_next();
}

/// Tell the runner whether `host.env` turned the sandbox off. The serving host calls this
/// before it converges roots, because the grantee depends on the answer.
#[cfg(windows)]
pub(crate) fn publish_sandbox_override() {
    plat::publish_sandbox_override();
}

/// Give the runner every root a plugin declared or was granted: on Linux the unit's drop-in,
/// which bwrap binds from and whose empty home hides the rest, restarting the runner when the
/// set changed; on Windows an ACE per root for the runner's account, stripped when its plugin
/// goes. `true` when a root or ACE changed.
pub(crate) fn converge_runner_roots() -> bool {
    // Tests never touch the operator's systemd config or a folder's ACL; macOS has no runner.
    if cfg!(test) || !(cfg!(target_os = "linux") || cfg!(windows)) || !runtime_status().installed {
        return false;
    }
    // Two quick decisions must not race: the later one reads the grants the earlier wrote.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(home) = manifest::home_dir() else {
        return false;
    };
    let manifests = manifest::runner_plugins();
    let roots = access::AccessStore::open(pf_paths::config_dir()).runner_roots(&manifests);
    match plat::converge_runner_roots(&roots, &home, &manifests) {
        Ok(true) => {
            tracing::info!(roots = roots.len(), "plugin runner roots updated");
            true
        }
        Ok(false) => false,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "plugin runner roots not updated");
            false
        }
    }
}

/// [`converge_runner_roots`] after the operator grants or takes a folder. On Windows a grant
/// places its plugin's ACE before the grants file changes, so the runner's own restart of that
/// plugin finds the folder open. Only an ACE this converge had to place itself — a package the
/// grant could not name — lands after that restart, so then the runner restarts once more.
pub(crate) fn converge_after_decision() {
    #[cfg(windows)]
    plat::take_walked();
    let changed = converge_runner_roots();
    #[cfg(windows)]
    if changed && plat::take_walked() && runtime_status().running {
        match plat::restart_runtime() {
            Ok(()) => tracing::info!("plugin runner restarted onto a changed folder grant"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "plugin runner not restarted"),
        }
    }
    #[cfg(not(windows))]
    let _ = changed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::args as argv;

    #[test]
    fn the_runners_refusal_reads_as_one_bounded_line() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sandbox_refusal_in(dir.path()), None);
        let state = dir.path().join("plugin-state");
        std::fs::create_dir_all(&state).unwrap();
        let long = "x".repeat(400);
        std::fs::write(
            state.join("runner.refused"),
            format!("no user\tnamespaces\u{1b}[0m {long}\nsecond line\n"),
        )
        .unwrap();
        let shown = sandbox_refusal_in(dir.path()).unwrap();
        assert!(shown.starts_with("no user namespaces [0m xxx"), "{shown}");
        assert_eq!(shown.chars().count(), 300);
        // An empty file is still a refusal: the runner wrote it, whatever the reason was.
        std::fs::write(state.join("runner.refused"), "").unwrap();
        assert_eq!(sandbox_refusal_in(dir.path()), Some(String::new()));
    }

    fn run(args: &[&str]) -> Result<()> {
        main(&argv(args), |_| Ok(0))
    }

    fn refusal(args: &[&str]) -> String {
        run(args).unwrap_err().to_string()
    }

    /// `add` and `remove` need the runner, so `main` stops before this step in a test: the
    /// step takes its two effects as arguments.
    #[test]
    fn an_install_that_issues_no_credentials_still_restarts_the_runner_then_fails() {
        let mut restarted = false;
        let err = settle_runtime(
            || Err::<(), _>(anyhow::anyhow!("disk full")),
            || {
                restarted = true;
                Ok(true)
            },
        )
        .unwrap_err();
        assert!(restarted, "the runner restarts onto the plugins it has");
        assert_eq!(
            format!("{err:#}"),
            "issue the plugins' API credentials: disk full"
        );
        assert!(settle_runtime(|| Ok(()), || Ok(false)).is_ok());
    }

    #[test]
    fn help_is_the_default_and_never_fails() {
        for args in [&[][..], &["-h"], &["--help"], &["help"]] {
            run(args).unwrap();
        }
    }

    #[test]
    fn a_command_that_stops_before_any_effect_says_why() {
        const GRANT: &str = "usage: punktfunk-host plugins grant <plugin> <dir> [--write]";
        const REVOKE: &str = "usage: punktfunk-host plugins revoke <plugin> <dir>";
        let rows: [(&[&str], &str); 10] = [
            (
                &["bogus"],
                "unknown plugins command 'bogus' (try `plugins --help`)",
            ),
            (
                &["--allow-public-registry", "add", "x"],
                "unknown plugins command '--allow-public-registry' (try `plugins --help`)",
            ),
            // A missing operand reaches the usage line instead of slicing past the end.
            (&["grant"], GRANT),
            (&["grant", "p"], GRANT),
            (&["grant", "p", " "], GRANT),
            (
                &["grant", "p", "d", "--bad"],
                "unknown flag '--bad' (the only flag is --write)",
            ),
            (&["revoke"], REVOKE),
            (&["revoke", "p"], REVOKE),
            (
                &["revoke", "p", "d", "--x"],
                "unknown flag '--x' (`plugins revoke` takes none)",
            ),
            (
                &["access", "--x"],
                "unknown flag '--x' (`plugins access` takes none)",
            ),
        ];
        for (args, want) in rows {
            assert_eq!(refusal(args), want, "{args:?}");
        }
    }

    #[test]
    fn package_verbs_name_what_they_remove() {
        let rows: [(&[&str], bool, &[&str]); 12] = [
            (&["add", "playnite"], false, &[]),
            (&["add"], false, &[]),
            (&["list"], true, &[]),
            (&["ls", "x"], true, &[]),
            (&["remove", "playnite"], false, &["playnite"]),
            (
                &["rm", "a", "@punktfunk/plugin-b"],
                false,
                &["a", "@punktfunk/plugin-b"],
            ),
            (&["uninstall", "a"], false, &["a"]),
            (&["remove"], false, &[]),
            (
                &["remove", "--exact", "a", "--allow-public-registry"],
                false,
                &["a"],
            ),
            // The runner skips the value of --plugins, --scripts and --registry, wherever they sit.
            (&["remove", "a", "--plugins", "/dir"], false, &["a"]),
            (
                &["rm", "--scripts", "/s", "a", "--registry", "@x=y", "b"],
                false,
                &["a", "b"],
            ),
            // `add` and `list` never look at their names.
            (&["add", "-x", "--plugins", "/dir"], false, &[]),
        ];
        for (args, listing, removing) in rows {
            let args = argv(args);
            let want = Action::Package {
                listing,
                removing: removing.to_vec(),
            };
            assert_eq!(parse(&args).unwrap(), want, "{args:?}");
        }
    }

    #[test]
    fn a_package_op_acts_on_the_dir_it_is_given() {
        let a = argv(&["remove", "a", "--plugins", "/dir"]);
        assert_eq!(plugins_dir_of(&a), std::path::Path::new("/dir"));
    }

    #[test]
    fn the_other_verbs_parse_to_their_actions() {
        let a = argv(&["grant", "p", "d", "--write"]);
        assert_eq!(
            parse(&a).unwrap(),
            Action::Grant(Some("p"), Some("d"), &a[3..])
        );
        let a = argv(&["revoke", "p"]);
        assert_eq!(parse(&a).unwrap(), Action::Revoke(Some("p"), None, &[]));
        let a = argv(&["grant"]);
        assert_eq!(parse(&a).unwrap(), Action::Grant(None, None, &[]));
        let a = argv(&["access", "--x"]);
        assert_eq!(parse(&a).unwrap(), Action::Access(&a[1..]));
        let a = argv(&["spawn", "--package", "x", "--", "e"]);
        assert_eq!(parse(&a).unwrap(), Action::Spawn(&a[1..]));
        for (word, want) in [
            ("enable", Action::Enable),
            ("disable", Action::Disable),
            ("status", Action::Status),
            ("help", Action::Help),
            ("-h", Action::Help),
            ("--help", Action::Help),
        ] {
            assert_eq!(parse(&argv(&[word])).unwrap(), want, "{word}");
        }
        assert_eq!(parse(&[]).unwrap(), Action::Help);
        assert!(parse(&argv(&["Enable"])).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn spawn_is_the_windows_runners_entry() {
        assert!(refusal(&["spawn", "--package", "x"]).contains("Windows runner"));
    }
}
