//! `pf-l10n`: checks the message files and writes them out for Android and Apple.
//!
//! ```text
//! pf-l10n check   [--root <l10n dir>] [--missing]
//! pf-l10n android [--root <l10n dir>] --out <res dir> --parts common,client,android
//! pf-l10n apple   [--root <l10n dir>] --out <dir> --parts common,client,apple
//! ```
//!
//! `--root` defaults to the directory this binary was built from. `check` prints one line per
//! problem and exits 1 on any; `--missing` adds the untranslated keys, which never fail. The
//! generators refuse a tree that does not compile, as the build does.

#[path = "../android.rs"]
mod android;
#[path = "../apple.rs"]
mod apple;
#[path = "../catalog.rs"]
mod catalog;
#[path = "../check.rs"]
mod check;
#[path = "../json.rs"]
mod json;
#[allow(dead_code)]
#[path = "../pack.rs"]
mod pack;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use pf_l10n::plural;

struct Args {
    command: String,
    root: PathBuf,
    out: Option<PathBuf>,
    parts: Vec<String>,
    missing: bool,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let command = argv
        .next()
        .ok_or("no command: use check, android or apple")?;
    let mut args = Args {
        command,
        root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        out: None,
        parts: Vec::new(),
        missing: false,
    };
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--root" => args.root = value()?.into(),
            "--out" => args.out = Some(value()?.into()),
            "--parts" => args.parts = value()?.split(',').map(str::to_owned).collect(),
            "--missing" => args.missing = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    match run(parse_args(env::args().skip(1))) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pf-l10n: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Result<Args, String>) -> Result<ExitCode, String> {
    let args = args?;
    if args.command == "check" {
        let report = check::run(&args.root);
        for line in &report.problems {
            println!("{line}");
        }
        if args.missing {
            for line in &report.missing {
                println!("missing: {line}");
            }
        }
        return Ok(if report.problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let generate = match args.command.as_str() {
        "android" => android::generate,
        "apple" => apple::generate,
        other => {
            return Err(format!(
                "unknown command {other}: use check, android or apple"
            ))
        }
    };
    let out = args.out.ok_or("--out is required")?;
    if args.parts.is_empty() {
        return Err("--parts is required".into());
    }
    let (cat, problems) = catalog::load(&args.root, false);
    if !problems.is_empty() {
        for line in &problems {
            eprintln!("{line}");
        }
        return Ok(ExitCode::FAILURE);
    }
    for (path, body) in generate(&cat, &args.parts)? {
        write(&out.join(path), &body)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn write(path: &Path, body: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Compares `actual` with `tests/golden/<name>`. `UPDATE_GOLDEN=1` writes it instead.
#[cfg(test)]
fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if env::var_os("UPDATE_GOLDEN").is_some() {
        write(&path, actual).expect("write golden");
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden {name}; run UPDATE_GOLDEN=1 cargo test -p pf-l10n"));
    assert_eq!(
        actual, expected,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}
