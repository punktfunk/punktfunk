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
pub mod manifest;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use self::windows as plat;
#[cfg(not(target_os = "windows"))]
mod posix;
#[cfg(not(target_os = "windows"))]
use self::posix as plat;

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") | Some("remove") | Some("rm") | Some("uninstall") | Some("list")
        | Some("ls") => {
            let listing = matches!(args.first().map(String::as_str), Some("list") | Some("ls"));
            if !listing {
                plat::require_elevation("installing or removing plugins")?;
            }
            // A removed plugin's titles leave with it; its id is gone once its files are.
            let removing: Vec<String> = match args.first().map(String::as_str) {
                Some("remove") | Some("rm") | Some("uninstall") => args[1..]
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .filter_map(|pkg| manifest::id_of_package(pkg))
                    .collect(),
                _ => Vec::new(),
            };
            forward_to_runner(args)?;
            for provider in removing {
                if let Err(e) = crate::library::delete_provider(&provider) {
                    println!("Couldn't remove the library titles of {provider}: {e:#}");
                }
            }
            if !listing {
                // The runner hands each plugin its token from this file; a running host picks
                // the new set up on the plugin's first request.
                if let Err(e) = crate::mgmt_token::load_or_generate_per_plugin() {
                    println!("Couldn't issue the plugins' API credentials: {e:#}");
                }
                // The runner discovers units at startup; without this the change is dormant.
                match restart_runtime() {
                    Ok(true) => println!("Plugin runner restarted."),
                    Ok(false) => println!("The plugin runner is off — `plugins enable` starts it."),
                    Err(e) => println!("Couldn't restart the plugin runner: {e:#}"),
                }
            }
            Ok(())
        }
        Some("enable") => {
            plat::require_elevation("enabling the plugin runner")?;
            plat::enable()
        }
        Some("disable") => {
            plat::require_elevation("disabling the plugin runner")?;
            plat::disable()
        }
        Some("status") => status(),
        Some("grant") => grant(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
            &args[3..],
        ),
        Some("access") => access_list(&args[1..]),
        Some("revoke") => revoke(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
            &args[3..],
        ),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => bail!("unknown plugins command '{other}' (try `plugins --help`)"),
    }
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
    converge_runner_roots();
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
    converge_runner_roots();
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
        let dir = args
            .iter()
            .position(|a| a == "--plugins")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::store::plugins_dir);
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

/// Has the operator turned the per-plugin sandbox off for the runner? Linux only.
pub(crate) fn runner_sandbox_off() -> bool {
    plat::runner_sandbox_off()
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

/// The ACL half of a grant, as `io::Error` for [`access::AccessStore`]. POSIX needs none:
/// the runner is the operator's own user unit.
pub(crate) fn grant_acl(dir: &std::path::Path, write: bool) -> std::io::Result<()> {
    plat::grant(dir, write).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Take the runner's ACE off a folder no plugin holds any more. POSIX has none to take.
pub(crate) fn revoke_acl(dir: &std::path::Path) -> std::io::Result<()> {
    plat::revoke(dir).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Windows: the runner grants `plugins enable` applies, which a fresh install never ran.
/// `serve` calls this; it acts once per install. POSIX needs none.
pub(crate) fn converge_runner_acls(status: &RuntimeStatus) {
    plat::converge_runner_acls(status);
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

/// Re-apply every recorded grant's ACL at `serve`. A launcher rewrite can drop the ACE its
/// grant depended on; a failure warns and the rest still converge. No-op off Windows.
pub(crate) fn converge_grants() {
    #[cfg(target_os = "windows")]
    {
        let store = access::AccessStore::open(pf_paths::config_dir());
        match store.snapshot() {
            Ok(snapshots) => {
                for s in snapshots {
                    for g in s.grants {
                        if let Err(e) = plat::grant(std::path::Path::new(&g.path), g.write) {
                            tracing::warn!(plugin = %s.plugin, path = %g.path, error = %e,
                                "plugin grant ACL could not be re-applied");
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "plugin grants could not be read for ACL convergence")
            }
        }
    }
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

/// Give the runner's unit every root a sandbox binds, restarting the runner when that set
/// changed. bwrap binds from the runner's own view, and the unit empties the home, so a grant
/// or manifest read missing from the unit never reaches the plugin.
pub(crate) fn converge_runner_roots() {
    // Tests never write the operator's systemd config; only a Linux unit hides the home.
    if cfg!(test) || !cfg!(target_os = "linux") || !runtime_status().installed {
        return;
    }
    // Two quick decisions must not race: the later one reads the grants the earlier wrote.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(home) = manifest::home_dir() else {
        return;
    };
    let roots =
        access::AccessStore::open(pf_paths::config_dir()).runner_roots(&manifest::installed());
    match plat::converge_runner_roots(&roots, &home) {
        Ok(true) => tracing::info!(roots = roots.len(), "plugin runner roots updated"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "plugin runner roots not updated"),
    }
}
