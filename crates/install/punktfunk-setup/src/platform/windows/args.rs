//! The Inno Setup command-line dialect.
//!
//! Winget, `punktfunk-host`'s auto-update (`update/windows.rs`), and rollback spawn this
//! installer with these flags, and an already-installed box hands the new installer the
//! old flags on its first update. This parser is a compatibility floor:
//! `design/installer-v2-windows.md` D5.
//!
//! Inno ignores flags it does not know. A strict exit here bricks a future updater
//! passing a newer flag to an older cached installer, so unknown `/`-flags warn, never fail.

use std::path::PathBuf;

/// `Silent` is progress-only. Both silent modes never ask; with `/SUPPRESSMSGBOXES`
/// every dialog takes its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silence {
    Interactive,
    Silent,
    VerySilent,
}

impl Silence {
    pub fn is_silent(self) -> bool {
        self != Silence::Interactive
    }
}

/// `!name` deselects. Names match case-insensitively, as Inno does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFlag {
    pub name: String,
    pub selected: bool,
}

/// Non-slash arguments pass through in `rest` so the engine's `--flags` share argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnoArgs {
    pub silence: Silence,
    pub suppress_msgboxes: bool,
    pub log: Option<PathBuf>,
    /// Merged over the derived defaults.
    pub merge_tasks: Vec<TaskFlag>,
    /// Replaces the defaults entirely (Inno's semantics).
    pub tasks: Option<Vec<TaskFlag>>,
    /// Fresh installs only; upgrades follow the ARP location.
    pub dir: Option<PathBuf>,
    /// `/WEBBIND=ADDR` — where the web console listens. Unvalidated here; `WinChoices::apply`
    /// warns on a value it cannot read rather than failing an unattended install.
    pub web_bind: Option<String>,
    /// Unknown `/`-flags: one warning line, never an error.
    pub unknown: Vec<String>,
    pub rest: Vec<String>,
}

impl InnoArgs {
    pub fn parse(args: &[String]) -> InnoArgs {
        let mut out = InnoArgs {
            silence: Silence::Interactive,
            suppress_msgboxes: false,
            log: None,
            merge_tasks: Vec::new(),
            tasks: None,
            dir: None,
            web_bind: None,
            unknown: Vec::new(),
            rest: Vec::new(),
        };
        for arg in args {
            if !arg.starts_with('/') {
                out.rest.push(arg.clone());
                continue;
            }
            let upper = arg.to_ascii_uppercase();
            let value = || arg.split_once('=').map(|(_, v)| unquote(v));
            match upper.split('=').next().unwrap_or(&upper) {
                "/VERYSILENT" => out.silence = Silence::VerySilent,
                // /SILENT never downgrades a /VERYSILENT that came first.
                "/SILENT" => {
                    if out.silence == Silence::Interactive {
                        out.silence = Silence::Silent;
                    }
                }
                "/SUPPRESSMSGBOXES" => out.suppress_msgboxes = true,
                "/LOG" => out.log = value().map(PathBuf::from),
                "/MERGETASKS" => out
                    .merge_tasks
                    .extend(task_list(&value().unwrap_or_default())),
                "/TASKS" => out.tasks = Some(task_list(&value().unwrap_or_default())),
                "/DIR" => out.dir = value().map(PathBuf::from),
                "/WEBBIND" => out.web_bind = value(),
                // No-ops we still accept: this installer never restarts, and we show no `/SP-` prompt.
                "/NORESTART" | "/SP-" => {}
                _ => out.unknown.push(arg.clone()),
            }
        }
        out
    }
}

/// Inno accepts a `*` glob; we never emit one, so a literal `*` is a name and warns
/// downstream.
fn task_list(value: &str) -> Vec<TaskFlag> {
    value
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| match t.strip_prefix('!') {
            Some(name) => TaskFlag {
                name: name.to_ascii_lowercase(),
                selected: false,
            },
            None => TaskFlag {
                name: t.to_ascii_lowercase(),
                selected: true,
            },
        })
        .collect()
}

/// Quotes survive some spawners and not others.
fn unquote(v: &str) -> String {
    v.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> InnoArgs {
        InnoArgs::parse(&args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
    }

    // The update/windows.rs SILENT_ARGS + /LOG spawn. Do not break it.
    #[test]
    fn the_fielded_updater_spawn_parses_clean() {
        let a = parse(&[
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/SP-",
            r"/LOG=C:\ProgramData\punktfunk\logs\update-0.36.0.log",
        ]);
        assert_eq!(a.silence, Silence::VerySilent);
        assert!(a.suppress_msgboxes);
        assert_eq!(
            a.log.unwrap().to_str().unwrap(),
            r"C:\ProgramData\punktfunk\logs\update-0.36.0.log"
        );
        assert!(a.unknown.is_empty());
        assert!(a.rest.is_empty());
    }

    #[test]
    fn flags_are_case_insensitive_and_silent_never_downgrades() {
        let a = parse(&["/verysilent", "/silent"]);
        assert_eq!(a.silence, Silence::VerySilent);
        let b = parse(&["/Silent"]);
        assert_eq!(b.silence, Silence::Silent);
    }

    #[test]
    fn mergetasks_parses_selection_and_deselection() {
        let a = parse(&[r#"/MERGETASKS="allowpublicfw,!trayicon""#]);
        assert_eq!(
            a.merge_tasks,
            vec![
                TaskFlag {
                    name: "allowpublicfw".into(),
                    selected: true
                },
                TaskFlag {
                    name: "trayicon".into(),
                    selected: false
                },
            ]
        );
    }

    #[test]
    fn tasks_replaces_while_mergetasks_merges() {
        let a = parse(&["/TASKS=gamestream"]);
        assert_eq!(a.tasks.unwrap().len(), 1);
        assert!(a.merge_tasks.is_empty());
    }

    // Inno ignores unknown flags; a future updater flag must warn, not fail.
    #[test]
    fn an_unknown_slash_flag_is_collected_never_fatal() {
        let a = parse(&["/VERYSILENT", "/FUTUREFLAG=2", "/NOCANCEL"]);
        assert_eq!(a.silence, Silence::VerySilent);
        assert_eq!(a.unknown, ["/FUTUREFLAG=2", "/NOCANCEL"]);
    }

    #[test]
    fn engine_flags_pass_through_in_rest() {
        let a = parse(&["--dry-run", "/VERYSILENT", "--facts", "box.json"]);
        assert_eq!(a.rest, ["--dry-run", "--facts", "box.json"]);
        assert_eq!(a.silence, Silence::VerySilent);
    }

    #[test]
    fn dir_and_quoted_log_values_unquote() {
        let a = parse(&[r#"/DIR="C:\Games\pf""#, r#"/LOG="C:\a b.log""#]);
        assert_eq!(a.dir.unwrap().to_str().unwrap(), r"C:\Games\pf");
        assert_eq!(a.log.unwrap().to_str().unwrap(), r"C:\a b.log");
    }
}
