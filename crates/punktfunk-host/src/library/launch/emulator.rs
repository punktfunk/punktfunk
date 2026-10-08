//! `launch.kind == "emulator"`: a catalog emulator this host found or installed, its command
//! rendered by hermir. The plugin names no program at all: only the emulator's catalog id, the
//! platform, the game file under a root it may reach, a libretro core and extra arguments.

use super::*;
use crate::plugins::manifest::{param_ok, ParamKind, PluginManifest};

/// Ceiling on extra arguments.
const MAX_EXTRA: usize = 32;

/// The arguments of an `emulator` entry, checked.
struct EmulatorArgs<'a> {
    emulator: &'a str,
    platform: &'a str,
    file: &'a Path,
    core: Option<&'a str>,
    extra: Vec<&'a str>,
}

/// Each argument once and in its shape, and the file inside the plugin's roots.
fn args<'a>(
    manifest: &PluginManifest,
    spec: &'a LaunchSpec,
) -> Result<EmulatorArgs<'a>, &'static str> {
    if !crate::emulators::in_catalog(&spec.value) {
        return Err("not an emulator in the catalog");
    }
    let (mut platform, mut file, mut core, mut extra) = (None, None, None, Vec::new());
    for a in spec.args.iter().flatten() {
        let v = a.value.as_str();
        match a.name.as_str() {
            "platform" if platform.is_none() && param_ok(ParamKind::Id, v) => platform = Some(v),
            "file" if file.is_none() && param_ok(ParamKind::Path, v) => file = Some(Path::new(v)),
            "core" if core.is_none() && crate::emulators::valid_core(v) => core = Some(v),
            "extra" if extra.len() < MAX_EXTRA && param_ok(ParamKind::Args, v) => extra.push(v),
            _ => return Err("an argument is unknown, repeated or malformed"),
        }
    }
    let (Some(platform), Some(file)) = (platform, file) else {
        return Err("`platform` and `file` are required");
    };
    if !manifest.confines(file) {
        return Err("the game file is outside the plugin's declared paths");
    }
    Ok(EmulatorArgs {
        emulator: &spec.value,
        platform,
        file,
        core,
        extra,
    })
}

/// Whether `spec` would resolve, short of finding a copy: the publish routes' check.
pub fn spec_is_valid(manifest: &PluginManifest, spec: &LaunchSpec) -> Result<(), &'static str> {
    args(manifest, spec).map(|_| ())
}

/// The command for `entry` from the emulator's best copy. `None`, with one warning naming the
/// reason, when an argument is wrong or this host has no copy.
pub fn recipe(entry: &GameEntry) -> Option<ExecRecipe> {
    let spec = entry.launch.as_ref().filter(|s| s.kind == "emulator")?;
    let provider = entry.provider.as_deref()?;
    let manifest = crate::plugins::manifest::for_provider(provider)?;
    build(&manifest, spec)
        .map_err(|reason| {
            tracing::warn!(
                provider,
                id = %entry.id,
                emulator = %spec.value,
                reason,
                "emulator launch: refusing the entry"
            );
        })
        .ok()
}

fn build(manifest: &PluginManifest, spec: &LaunchSpec) -> Result<ExecRecipe, String> {
    let a = args(manifest, spec)?;
    let launch = crate::emulators::launch_spec(a.emulator, a.platform, a.file, a.core)
        .map_err(|e| e.to_string())?;
    // hermir's SDL hints for a copy that is no Flatpak; `env` sets them and execs the copy.
    let mut argv = Vec::new();
    if !launch.env.is_empty() {
        if cfg!(windows) {
            return Err("the command needs environment variables this launch can't set".into());
        }
        argv.push("env".to_string());
        argv.extend(launch.env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    argv.extend(launch.argv());
    let mut argv = argv.into_iter();
    let program = argv.next().ok_or("the command is empty")?;
    Ok(ExecRecipe {
        program,
        args: argv.chain(a.extra.iter().map(|s| s.to_string())).collect(),
        cwd: launch.cwd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(args: &[(&str, &str)]) -> LaunchSpec {
        LaunchSpec {
            kind: "emulator".into(),
            value: "retroarch".into(),
            args: Some(
                args.iter()
                    .map(|(n, v)| LaunchArg {
                        name: (*n).into(),
                        value: (*v).into(),
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn an_emulator_entry_names_a_catalog_emulator_and_a_file_it_may_reach() {
        let roms = tempfile::tempdir().unwrap();
        let manifest = PluginManifest {
            schema: 1,
            id: "rom-manager".into(),
            reads: vec![roms.path().to_string_lossy().into_owned()],
            ..Default::default()
        };
        let file = roms.path().join("snes/Chrono Trigger.sfc");
        let file = file.to_str().unwrap();
        let ok = |s: &LaunchSpec| spec_is_valid(&manifest, s);
        assert_eq!(ok(&spec(&[("platform", "snes"), ("file", file)])), Ok(()));
        assert_eq!(
            ok(&spec(&[
                ("platform", "snes"),
                ("file", file),
                ("core", "snes9x"),
                ("extra", "--verbose"),
                ("extra", "--menu"),
            ])),
            Ok(())
        );
        assert!(ok(&spec(&[("platform", "snes")])).is_err());
        assert!(ok(&spec(&[("platform", "snes"), ("file", "/etc/passwd")])).is_err());
        assert!(ok(&spec(&[
            ("platform", "snes"),
            ("file", file),
            ("file", file)
        ]))
        .is_err());
        assert!(ok(&spec(&[("platform", "-x"), ("file", file)])).is_err());
        assert!(ok(&spec(&[
            ("platform", "snes"),
            ("file", file),
            ("core", "a/b")
        ]))
        .is_err());
        assert!(ok(&spec(&[
            ("platform", "snes"),
            ("file", file),
            ("exe", "/bin/sh")
        ]))
        .is_err());
        let mut unknown = spec(&[("platform", "snes"), ("file", file)]);
        unknown.value = "bash".into();
        assert!(ok(&unknown).is_err());
    }
}
