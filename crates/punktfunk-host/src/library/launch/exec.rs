//! `launch.kind == "exec"`: a command the HOST builds from the owning plugin's manifest.
//!
//! The entry names a template and supplies values for its parameters. Nothing a plugin says at
//! runtime widens what can run: the program and the argv shape come from the installed package's
//! manifest, every value is checked against the parameter's character class, and a `path` value
//! must sit under a root that manifest declares.
//!
//! This replaces the older `plugin` kind, where the host dialled the plugin at launch and ran the
//! command line it answered with (security-review 2026-08-15 §10, 2026-08-31 H-1).

use super::*;
use crate::plugins::manifest::{param_ok, ParamKind, PluginManifest};
use std::collections::BTreeMap;

/// Ceiling on the values one list parameter may carry.
const MAX_ARGS: usize = 32;

/// A resolved command: program plus its argv, ready for the platform to spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRecipe {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Resolve `entry` against the manifest of the plugin that published it.
///
/// `None` — with one warning naming the reason — whenever the template is unknown, a parameter is
/// missing or malformed, or a path escapes the declared roots.
pub fn recipe(entry: &GameEntry) -> Option<ExecRecipe> {
    let spec = entry.launch.as_ref()?;
    if spec.kind != "exec" {
        return None;
    }
    let provider = entry.provider.as_deref()?;
    let manifest = crate::plugins::manifest::for_provider(provider)?;
    build(&manifest, &spec.value, spec.args.as_ref())
        .map_err(|reason| {
            tracing::warn!(
                provider,
                id = %entry.id,
                template = %spec.value,
                reason,
                "exec launch: refusing the entry"
            );
        })
        .ok()
}

/// Whether `spec` would resolve, without touching the filesystem beyond the manifest and its
/// grants. The publish routes use this to drop a bad entry at write time rather than at launch.
pub fn spec_is_valid(manifest: &PluginManifest, spec: &LaunchSpec) -> Result<(), &'static str> {
    build(manifest, &spec.value, spec.args.as_ref()).map(|_| ())
}

fn build(
    manifest: &PluginManifest,
    template: &str,
    args: Option<&Vec<LaunchArg>>,
) -> Result<ExecRecipe, &'static str> {
    let tmpl = manifest.exec.get(template).ok_or("no such exec template")?;
    let empty = Vec::new();
    let mut given: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for arg in args.unwrap_or(&empty) {
        let kind = *tmpl
            .params
            .get(&arg.name)
            .ok_or("argument is not a parameter of this template")?;
        let slot = given.entry(arg.name.as_str()).or_default();
        if kind != ParamKind::Args && !slot.is_empty() {
            return Err("the same argument was given twice");
        }
        if slot.len() >= MAX_ARGS {
            return Err("too many values for one argument");
        }
        if !param_ok(kind, &arg.value) {
            return Err("argument does not match its parameter kind");
        }
        if kind == ParamKind::Path && !manifest.confines(Path::new(&arg.value)) {
            return Err("path argument is outside the plugin's declared paths");
        }
        slot.push(arg.value.as_str());
    }
    // A single-valued parameter must be there; a list one may legitimately be empty.
    let mut values: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, kind) in &tmpl.params {
        match given.get(name.as_str()) {
            Some(vs) if *kind != ParamKind::Args => {
                values.insert(name.as_str(), vs[0]);
            }
            Some(_) => {}
            None if *kind == ParamKind::Args => {}
            None => return Err("missing argument"),
        }
    }
    let program = program_for(manifest, &substitute(&tmpl.exe, &values)?)?;
    let mut argv = Vec::with_capacity(tmpl.args.len());
    for arg in &tmpl.args {
        // An element that is exactly one list placeholder expands in place; anything else is one
        // element with its single-valued placeholders filled in.
        if let Some(name) = arg.strip_prefix('{').and_then(|a| a.strip_suffix('}')) {
            if tmpl.params.get(name) == Some(&ParamKind::Args) {
                argv.extend(
                    given
                        .get(name)
                        .into_iter()
                        .flatten()
                        .map(|v| (*v).to_string()),
                );
                continue;
            }
        }
        argv.push(substitute(arg, &values)?);
    }
    let cwd = match tmpl.cwd.as_deref() {
        None => None,
        Some(dir) => {
            let dir = substitute(dir, &values)?;
            let path = PathBuf::from(&dir);
            if !manifest.confines(&path) {
                return Err("cwd is outside the plugin's declared paths");
            }
            Some(path)
        }
    };
    Ok(ExecRecipe {
        program,
        args: argv,
        cwd,
    })
}

/// A bare program name (resolved from `PATH` by the spawn) or an absolute path inside the roots
/// this plugin may reach — which is how a template runs the game itself (`"exe": "{game}"`, a
/// `path` parameter under an install dir the operator granted). Never a relative path: that would
/// resolve against whatever cwd we launch in.
fn program_for(manifest: &PluginManifest, exe: &str) -> Result<String, &'static str> {
    if exe.is_empty() || exe.chars().any(char::is_control) {
        return Err("exe is empty or has control characters");
    }
    let path = Path::new(exe);
    if path.is_absolute() {
        return manifest
            .confines(path)
            .then(|| exe.to_string())
            .ok_or("absolute exe is outside the plugin's declared paths");
    }
    if exe.contains('/') || exe.contains('\\') {
        return Err("exe must be a program name or an absolute path");
    }
    Ok(exe.to_string())
}

/// Replace `{param}` occurrences. An unknown placeholder is an error, not a literal: a template
/// that misspells its own parameter must fail loudly rather than pass `{rom}` to an emulator.
fn substitute(arg: &str, values: &BTreeMap<&str, &str>) -> Result<String, &'static str> {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}').ok_or("unclosed { in a template")? + start;
        let name = &rest[start + 1..end];
        out.push_str(
            values
                .get(name)
                .ok_or("unknown placeholder in a template")?,
        );
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl ExecRecipe {
    /// One `sh -c` string, run from `cwd` when the template names one. Every element is
    /// single-quoted, so a value can never become syntax.
    #[cfg(not(windows))]
    pub fn shell_command(&self) -> String {
        let command = std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|s| sh_quote(s))
            .collect::<Vec<_>>()
            .join(" ");
        match &self.cwd {
            Some(dir) => format!("cd {} && {command}", sh_quote(&dir.to_string_lossy())),
            None => command,
        }
    }

    /// One `CreateProcess` command line, quoted the way the CRT parses it back.
    #[cfg(windows)]
    pub fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|s| win_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// POSIX single-quoting: wrap, and end/reopen the quote around each `'`.
#[cfg(not(windows))]
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Windows command-line quoting (the rules `CommandLineToArgvW` parses): wrap in `"`, double the
/// backslashes that precede a quote or end the element, and escape embedded quotes.
#[cfg(windows)]
pub(crate) fn win_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod exec_tests {
    use super::*;
    use crate::plugins::manifest::ExecTemplate;

    fn manifest() -> PluginManifest {
        let mut exec = BTreeMap::new();
        exec.insert(
            "retroarch".to_string(),
            ExecTemplate {
                exe: "retroarch".into(),
                args: vec![
                    "-f".into(),
                    "-L".into(),
                    "/opt/emu/cores/{core}.so".into(),
                    "{rom}".into(),
                ],
                params: BTreeMap::from([
                    ("core".to_string(), ParamKind::Id),
                    ("rom".to_string(), ParamKind::Path),
                ]),
                cwd: None,
            },
        );
        PluginManifest {
            schema: 1,
            id: "rom-manager".into(),
            reads: vec!["/games".into(), "/opt/emu".into()],
            exec,
            ..Default::default()
        }
    }

    fn args(pairs: &[(&str, &str)]) -> Vec<LaunchArg> {
        pairs
            .iter()
            .map(|(name, value)| LaunchArg {
                name: (*name).to_string(),
                value: (*value).to_string(),
            })
            .collect()
    }

    #[test]
    fn a_declared_template_resolves_to_its_argv() {
        let r = build(
            &manifest(),
            "retroarch",
            Some(&args(&[
                ("core", "snes9x"),
                ("rom", "/games/snes/Mario.sfc"),
            ])),
        )
        .unwrap();
        assert_eq!(r.program, "retroarch");
        assert_eq!(
            r.args,
            vec![
                "-f",
                "-L",
                "/opt/emu/cores/snes9x.so",
                "/games/snes/Mario.sfc"
            ]
        );
    }

    #[test]
    fn a_rom_outside_the_declared_roots_is_refused() {
        let err = build(
            &manifest(),
            "retroarch",
            Some(&args(&[("core", "snes9x"), ("rom", "/etc/shadow")])),
        )
        .unwrap_err();
        assert_eq!(err, "path argument is outside the plugin's declared paths");
    }

    #[test]
    fn shell_metacharacters_never_reach_the_shell() {
        // Refused as a value...
        assert!(build(
            &manifest(),
            "retroarch",
            Some(&args(&[("core", "a; rm -rf /"), ("rom", "/games/x.sfc")])),
        )
        .is_err());
        // ...and even a legal name with a quote in it stays one argv element.
        #[cfg(not(windows))]
        {
            let r = ExecRecipe {
                program: "bottles-cli".into(),
                args: vec!["-b".into(), "Don't Starve".into()],
                cwd: None,
            };
            assert_eq!(r.shell_command(), r"'bottles-cli' '-b' 'Don'\''t Starve'");
            // A game that loads its data by relative path starts in its own folder.
            let game = ExecRecipe {
                program: "/games/it's here/game".into(),
                args: vec![],
                cwd: Some("/games/it's here".into()),
            };
            assert_eq!(
                game.shell_command(),
                r"cd '/games/it'\''s here' && '/games/it'\''s here/game'"
            );
        }
    }

    #[test]
    fn unknown_templates_and_stray_arguments_are_refused() {
        assert_eq!(
            build(&manifest(), "nope", None).unwrap_err(),
            "no such exec template"
        );
        assert_eq!(
            build(
                &manifest(),
                "retroarch",
                Some(&args(&[
                    ("core", "snes9x"),
                    ("rom", "/games/x.sfc"),
                    ("extra", "1")
                ]))
            )
            .unwrap_err(),
            "argument is not a parameter of this template"
        );
        assert_eq!(
            build(&manifest(), "retroarch", Some(&args(&[("core", "snes9x")]))).unwrap_err(),
            "missing argument"
        );
    }

    #[test]
    fn a_path_parameter_can_be_the_program_itself() {
        let mut m = manifest();
        m.exec.insert(
            "game".to_string(),
            ExecTemplate {
                exe: "{game}".into(),
                args: vec![],
                params: BTreeMap::from([("game".to_string(), ParamKind::Path)]),
                cwd: None,
            },
        );
        assert_eq!(
            build(
                &m,
                "game",
                Some(&args(&[("game", "/games/itch/quail/quail")]))
            )
            .unwrap()
            .program,
            "/games/itch/quail/quail"
        );
        // ...but only inside the roots the plugin may reach.
        assert!(build(&m, "game", Some(&args(&[("game", "/usr/bin/sudo")]))).is_err());
    }

    #[test]
    fn a_list_parameter_expands_to_its_own_argv_elements() {
        let mut m = manifest();
        m.exec.insert(
            "with-flags".to_string(),
            ExecTemplate {
                exe: "retroarch".into(),
                args: vec!["{rom}".into(), "{extra}".into()],
                params: BTreeMap::from([
                    ("rom".to_string(), ParamKind::Path),
                    ("extra".to_string(), ParamKind::Args),
                ]),
                cwd: None,
            },
        );
        let r = build(
            &m,
            "with-flags",
            Some(&args(&[
                ("rom", "/games/x.sfc"),
                ("extra", "--fullscreen"),
                ("extra", "--verbose"),
            ])),
        )
        .unwrap();
        assert_eq!(r.args, vec!["/games/x.sfc", "--fullscreen", "--verbose"]);
        // Absent is fine for a list; absent is not fine for the rom.
        assert!(build(&m, "with-flags", Some(&args(&[("rom", "/games/x.sfc")]))).is_ok());
        assert!(build(&m, "with-flags", Some(&args(&[("extra", "--x")]))).is_err());
    }

    #[test]
    fn an_exe_outside_the_declared_roots_is_refused() {
        let mut m = manifest();
        m.exec.get_mut("retroarch").unwrap().exe = "/usr/bin/sudo".into();
        assert_eq!(
            build(
                &m,
                "retroarch",
                Some(&args(&[("core", "x"), ("rom", "/games/x.sfc")]))
            )
            .unwrap_err(),
            "absolute exe is outside the plugin's declared paths"
        );
    }
}
