//! `punktfunk-setup-pack`: the console-subsystem sibling that assembles the D3 sandwich
//! (`pack`), the payload-less uninstaller (`pack-uninstaller`, D6) and reads one back
//! (`inspect`). Console on purpose: PowerShell's `&` neither waits for nor captures a
//! GUI-subsystem exe (S2). Cross-platform so the step and its tests run anywhere.
//!
//! The runtime set is whatever `windows_reactor_setup::as_self_contained` staged next to the
//! built wizard: every non-cargo file and every locale dir in the wizard's target dir — which
//! is why the packer builds the wizard crate into a target dir of its own.

use std::path::{Path, PathBuf};

use punktfunk_setup::platform::windows::plan::Artifact;

use crate::overlay;
use crate::payload::{self, Manifest, Trees};

const USAGE: &str = "usage:\n  \
    punktfunk-setup-pack pack --exe <wizard.exe> --runtime <target dir> --app <dir> [--staging <dir>] --version <v> --artifact host|client --out <file>\n  \
    punktfunk-setup-pack pack-uninstaller --exe <wizard.exe> --runtime <target dir> --version <v> --artifact host|client --out <file>\n  \
    punktfunk-setup-pack inspect <file>";

/// One verb; the report line for stdout.
pub fn run(args: &[String]) -> Result<String, String> {
    let (verb, rest) = args.split_first().ok_or(USAGE)?;
    match verb.as_str() {
        "pack" => pack(&Opts::parse(rest)?, false),
        "pack-uninstaller" => pack(&Opts::parse(rest)?, true),
        "inspect" => inspect(rest.first().ok_or(USAGE)?),
        _ => Err(USAGE.into()),
    }
}

struct Opts {
    exe: PathBuf,
    runtime: PathBuf,
    app: Option<PathBuf>,
    staging: Option<PathBuf>,
    version: String,
    artifact: Artifact,
    out: PathBuf,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let get = |key: &str| -> Option<String> {
            args.iter()
                .position(|a| a == key)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let need =
            |key: &str, v: Option<String>| v.ok_or_else(|| format!("{key} missing\n{USAGE}"));
        Ok(Opts {
            exe: need("--exe", get("--exe"))?.into(),
            runtime: need("--runtime", get("--runtime"))?.into(),
            app: get("--app").map(PathBuf::from),
            staging: get("--staging").map(PathBuf::from),
            version: need("--version", get("--version"))?,
            artifact: match need("--artifact", get("--artifact"))?.as_str() {
                "host" => Artifact::Host,
                "client" => Artifact::Client,
                other => return Err(format!("--artifact {other}: host or client")),
            },
            out: need("--out", get("--out"))?.into(),
        })
    }
}

fn pack(opts: &Opts, uninstaller: bool) -> Result<String, String> {
    if !uninstaller && opts.app.is_none() {
        return Err(format!("pack needs --app\n{USAGE}"));
    }
    let exe = std::fs::read(&opts.exe).map_err(|e| format!("{}: {e}", opts.exe.display()))?;
    let runtime = tempfile::tempdir().map_err(|e| e.to_string())?;
    let staged = stage_runtime(&opts.runtime, runtime.path())?;
    let manifest = Manifest {
        artifact: opts.artifact,
        version: opts.version.clone(),
        uninstaller,
    };
    let trees = Trees {
        runtime: runtime.path(),
        app: if uninstaller {
            None
        } else {
            opts.app.as_deref()
        },
        staging: if uninstaller {
            None
        } else {
            opts.staging.as_deref()
        },
    };
    let payload = payload::build(&trees, &manifest)?;
    let assembled = overlay::assemble(&exe, &payload);
    std::fs::write(&opts.out, &assembled).map_err(|e| format!("{}: {e}", opts.out.display()))?;
    Ok(format!(
        "packed {}: exe {} + payload {} ({staged} runtime files) = {}",
        opts.out.display(),
        mb(exe.len()),
        mb(payload.len()),
        mb(assembled.len())
    ))
}

fn inspect(path: &str) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let signed = overlay::cert_table_offset(&data).unwrap_or(0) != 0;
    let payload = overlay::extract(&data)?;
    let (manifest, entries) = payload::peek(payload)?;
    Ok(format!(
        "{path}: signed={signed} payload {} ({entries} entries), sha256 verified, artifact={:?} version={} uninstaller={}",
        mb(payload.len()),
        manifest.artifact,
        manifest.version,
        manifest.uninstaller
    ))
}

/// Copy the runtime set out of a wizard target dir: cargo's own dirs and files, the exes
/// (the wizard rides in via `--exe`) and symbol files stay behind. Returns the file count.
pub fn stage_runtime(target: &Path, into: &Path) -> Result<usize, String> {
    const SKIP_DIRS: [&str; 5] = ["deps", "build", "examples", "incremental", ".fingerprint"];
    let mut count = 0;
    for entry in std::fs::read_dir(target).map_err(|e| format!("{}: {e}", target.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            count += copy_tree(&path, &into.join(name.as_ref()))?;
        } else {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with(".cargo-")
                || lower.ends_with(".exe")
                || lower.ends_with(".pdb")
                || lower.ends_with(".d")
            {
                continue;
            }
            std::fs::copy(&path, into.join(name.as_ref()))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            count += 1;
        }
    }
    if count == 0 {
        return Err(format!(
            "{}: no runtime files — build the wizard into this dir first",
            target.display()
        ));
    }
    Ok(count)
}

/// [`copy_tree`] onto a live install: a file Windows refuses to overwrite because a process
/// still maps it (error 5 or 32) lands beside it as `<name>.pf-new`, queued to replace it at
/// the next boot, and goes on `deferred` instead of aborting the tree mid-way.
pub fn deploy_tree(from: &Path, to: &Path, deferred: &mut Vec<PathBuf>) -> Result<usize, String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut count = 0;
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            count += deploy_tree(&entry.path(), &dest, deferred)?;
            continue;
        }
        match std::fs::copy(entry.path(), &dest) {
            Ok(_) => count += 1,
            Err(e) if matches!(e.raw_os_error(), Some(5 | 32)) => {
                let staged = dest.with_extension(match dest.extension() {
                    Some(ext) => format!("{}.pf-new", ext.to_string_lossy()),
                    None => "pf-new".into(),
                });
                std::fs::copy(entry.path(), &staged)
                    .map_err(|e| format!("{}: {e}", staged.display()))?;
                if !punktfunk_setup::platform::windows::sys::replace_on_reboot(&staged, &dest) {
                    return Err(format!(
                        "{}: {e} (and no reboot-time replace)",
                        dest.display()
                    ));
                }
                deferred.push(dest);
            }
            Err(e) => return Err(format!("{}: {e}", dest.display())),
        }
    }
    Ok(count)
}

pub fn copy_tree(from: &Path, to: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut count = 0;
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            count += copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            count += 1;
        }
    }
    Ok(count)
}

fn mb(len: usize) -> String {
    format!("{:.1} MB", len as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// A wizard target dir as reactor-setup + cargo leave it.
    fn fake_target(root: &Path) {
        write(
            root,
            "punktfunk-setup-win.exe",
            &crate::overlay::tests::fake_pe(0, 32),
        );
        write(root, "punktfunk-setup-win.pdb", b"symbols");
        write(root, "punktfunk-setup-win.d", b"deps");
        write(root, ".cargo-lock", b"");
        write(root, "microsoft.ui.xaml.dll", b"xaml");
        write(root, "microsoft.ui.xaml", b"no extension, still runtime");
        write(root, "resources.pri", b"pri");
        write(root, "en-us/microsoft.ui.xaml.dll.mui", b"mui");
        write(root, "deps/libfoo.rlib", b"rlib");
        write(root, "build/x/out/app.manifest", b"m");
        write(root, ".fingerprint/x/lib", b"f");
    }

    #[test]
    fn the_runtime_set_is_everything_but_cargo_exes_and_symbols() {
        let target = tempfile::tempdir().unwrap();
        fake_target(target.path());
        let into = tempfile::tempdir().unwrap();
        assert_eq!(stage_runtime(target.path(), into.path()).unwrap(), 4);
        for present in [
            "microsoft.ui.xaml.dll",
            "microsoft.ui.xaml",
            "resources.pri",
            "en-us/microsoft.ui.xaml.dll.mui",
        ] {
            assert!(into.path().join(present).is_file(), "{present}");
        }
        for absent in [
            "punktfunk-setup-win.exe",
            "punktfunk-setup-win.pdb",
            ".cargo-lock",
            "deps",
            "build",
            ".fingerprint",
        ] {
            assert!(!into.path().join(absent).exists(), "{absent}");
        }
    }

    #[test]
    fn pack_then_inspect_and_the_uninstaller_carries_no_app() {
        let target = tempfile::tempdir().unwrap();
        fake_target(target.path());
        let app = tempfile::tempdir().unwrap();
        write(app.path(), "punktfunk-host.exe", b"host");
        let out = tempfile::tempdir().unwrap();
        let setup = out.path().join("punktfunk-host-setup-0.35.0.exe");
        let unins = out.path().join("unins000.exe");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let exe = s(&target.path().join("punktfunk-setup-win.exe"));
        let base = |out: &Path| {
            vec![
                "--exe".into(),
                exe.clone(),
                "--runtime".into(),
                s(target.path()),
                "--version".into(),
                "0.35.0".into(),
                "--artifact".into(),
                "host".into(),
                "--out".into(),
                s(out),
            ]
        };
        let mut args = vec!["pack".to_string()];
        args.extend(base(&setup));
        args.extend(["--app".to_string(), s(app.path())]);
        assert!(run(&args).unwrap().starts_with("packed"));
        let report = run(&["inspect".to_string(), s(&setup)]).unwrap();
        assert!(
            report.contains("signed=false")
                && report.contains("artifact=Host")
                && report.contains("uninstaller=false"),
            "{report}"
        );

        let mut args = vec!["pack-uninstaller".to_string()];
        args.extend(base(&unins));
        run(&args).unwrap();
        let report = run(&["inspect".to_string(), s(&unins)]).unwrap();
        assert!(report.contains("uninstaller=true"), "{report}");
        let dst = tempfile::tempdir().unwrap();
        let data = std::fs::read(&unins).unwrap();
        payload::extract(overlay::extract(&data).unwrap(), dst.path()).unwrap();
        assert!(dst.path().join("runtime/microsoft.ui.xaml.dll").is_file());
        assert!(!dst.path().join("app").exists());
        // The installer is bigger than its uninstaller by exactly the app tree's worth.
        assert!(std::fs::metadata(&setup).unwrap().len() > data.len() as u64);
    }

    #[test]
    fn pack_without_an_app_tree_is_refused() {
        let args: Vec<String> = [
            "pack",
            "--exe",
            "x",
            "--runtime",
            "y",
            "--version",
            "1",
            "--artifact",
            "host",
            "--out",
            "z",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(run(&args).unwrap_err().contains("--app"));
    }
}
