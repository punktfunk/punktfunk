//! `%ProgramData%\punktfunk\host.env`: loading it into the service, writing its default, and
//! the line edits `service install` makes.

use super::*;

pub(super) fn host_env_path() -> PathBuf {
    pf_paths::config_dir().join("host.env")
}

/// Load host.env into this process so the host child inherits `PUNKTFUNK_*` / `RUST_LOG`.
pub(super) fn load_host_env() {
    let path = host_env_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        tracing::info!(path = %path.display(), "no host.env (using defaults)");
        return;
    };
    let mut n = 0;
    for (k, v) in pf_paths::env_file::parse(&contents) {
        // Allow-list matches `interactive::merged_env_block`. A planted host.env must not
        // override `SystemRoot` — `icacls_path` / the powershell warner resolve through it.
        // `PUNKTFUNK_HOST_CMD` still passes; a non-admin-owned host.env is rejected at install.
        // Credentials are excluded outright: they live in their own owner-only files, and an
        // environment copy is what every child process inherits.
        let secret = k.contains("TOKEN") || k.contains("PASSWORD");
        let allowed = (k.starts_with("PUNKTFUNK_") || k == "RUST_LOG") && !secret;
        if allowed {
            // SAFETY: std documents `set_var` as always safe on Windows: the OS serializes
            // environment access, so the SCM dispatcher thread cannot see a torn write.
            unsafe { std::env::set_var(k, v) };
            n += 1;
        } else {
            tracing::warn!(key = %k, "host.env: ignoring non-allow-listed key");
        }
    }
    tracing::info!(path = %path.display(), vars = n, "loaded host.env");
}

/// Write a default `host.env` if none exists. Encoder default is `auto` (vendor GPU).
pub(super) fn ensure_default_host_env() -> Result<()> {
    let path = host_env_path();
    // Non-admin-owned host.env was planted before this elevated install (`ProgramData` grants
    // Users add-subdirectory + CREATOR OWNER). Check before `create_private_dir` re-owns the
    // dir and erases that signal. Administrators-owned files from a prior install pass.
    let planted = pf_paths_win::planted_by_non_admin(&path);
    if planted {
        // Rename-aside is best-effort; the guarantee is the `!planted` skip overwrites anyway.
        match pf_paths_win::rename_aside(&path) {
            Ok(aside) => tracing::warn!(
                path = %path.display(), aside = %aside.display(),
                "host.env was owned by a non-admin account (planted before install) — renamed aside; writing the default"
            ),
            Err(e) => tracing::error!(
                error = %e, path = %path.display(),
                "host.env is non-admin-owned and could not be renamed aside — overwriting it with the default"
            ),
        }
    }
    // Harden the dir first, before the `exists()` check — not only in the create-file branch.
    // `ProgramData` grants Users add-subdirectory + CREATOR OWNER; a planted dir must still lock.
    // The error is the junction refusal: `write_secret_file` only rejects the FILE being a link,
    // so a junctioned parent would still redirect this write. Refuse the install step instead.
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir)
            .with_context(|| format!("lock down {} before writing host.env", dir.display()))?;
    }
    if path.exists() && !planted {
        // Re-lock the file: an owner can rewrite the DACL it inherited. `planted` files fall
        // through and are overwritten even if the rename-aside failed.
        pf_paths::restrict_existing_secret_file(&path);
        name_web_console_bind(&path);
        return Ok(());
    }
    let default = "# punktfunk host configuration (read by the Windows service).\n\
        # KEY=VALUE per line; '#' comments. Restart the service after editing:\n\
        #   punktfunk-host service stop && punktfunk-host service start\n\
        \n\
        # Encode backend: auto (default) detects the GPU vendor — NVIDIA->nvenc, AMD->amf, Intel->qsv.\n\
        # Force one with nvenc | amf | qsv | mf (no software encoder: the driver encodes). amf/qsv need an FFmpeg-built\n\
        # host; mf is Media Foundation, any vendor's hardware encoder, 8-bit 4:2:0 only.\n\
        PUNKTFUNK_ENCODER=auto\n\
        PUNKTFUNK_VIDEO_SOURCE=virtual\n\
        # The virtual display is the bundled pf-vdisplay driver, which also encodes; there is no\n\
        # other capture path, and the secure desktop (UAC / lock / login) is always captured.\n\
        RUST_LOG=info\n\
        \n\
        # The host subcommand the service launches. GameStream (Moonlight) is a setting in the web\n\
        # console; `serve --gamestream` here would lock it on.\n\
        PUNKTFUNK_HOST_CMD=serve\n\
        \n\
        # The web management console (https://<this-PC>:47992) runs as a child of the service.\n\
        # Set to off to disable it:\n\
        # PUNKTFUNK_WEB_CONSOLE=off\n\
        \n\
        # Where that console listens: 0.0.0.0 (your network, never the internet), 127.0.0.1 (this\n\
        # PC only), or one address, e.g. a VPN interface. The plugin-UI origin on 47993 follows it.\n\
        PUNKTFUNK_UI_BIND=0.0.0.0\n\
        \n\
        # Force a specific render GPU by name substring (multi-GPU boxes only):\n\
        # PUNKTFUNK_RENDER_ADAPTER=4090\n\
        \n\
        # The name this host shows up under in Moonlight and the Punktfunk clients\n\
        # (default: the machine's own computer name):\n\
        # PUNKTFUNK_HOST_NAME=Living Room\n";
    // DACL-locked: host.env is the SYSTEM service's environment and launched command line.
    pf_paths::write_secret_file(&path, default.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    println!("Wrote default config: {}", path.display());
    Ok(())
}

/// Name the console's bind in a host.env that predates `PUNKTFUNK_UI_BIND`, once.
///
/// The value is the default the console already runs with, so nothing moves; the line is there so
/// the operator finds the setting. `--web-bind` (applied after this) changes it in the same run.
pub(super) fn name_web_console_bind(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("PUNKTFUNK_UI_BIND="))
    {
        return;
    }
    let mut next = text;
    next.push_str(concat!(
        "\n# Where the web console listens: 0.0.0.0 (your network, never the internet), 127.0.0.1\n",
        "# (this PC only), or one address, e.g. a VPN interface.\n",
        "PUNKTFUNK_UI_BIND=0.0.0.0\n",
    ));
    match pf_paths::write_secret_file(path, next.as_bytes()) {
        Ok(()) => println!("PUNKTFUNK_UI_BIND=0.0.0.0 → {}", path.display()),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "name the console bind in host.env")
        }
    }
}

/// Record the GameStream choice in the settings store, where the console reads and changes it.
///
/// A host command of `serve --gamestream`, or none (which ran that), becomes `serve` plus a stored
/// `true`: the flag would lock the console's toggle. The store is written before host.env, so a
/// failed write never turns GameStream off. A custom command stays. Best-effort.
pub(super) fn apply_gamestream_choice(choice: Option<bool>) {
    let path = host_env_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!(
            "warning: {} not read, so the GameStream choice is unapplied",
            path.display()
        );
        return;
    };
    let (store, host_env, pinned) = gamestream_migration(&text, choice);
    if let Some(on) = store {
        let patch = serde_json::Map::from_iter([("gamestream".into(), on.into())]);
        if let Err(e) = pf_host_config::save(&patch) {
            eprintln!("warning: the GameStream choice was not saved: {e}");
            return;
        }
        println!(
            "GameStream (Moonlight) compatibility: {}",
            if on { "on" } else { "off" }
        );
    }
    if let Some(next) = host_env {
        // `write_secret_file` re-asserts the SYSTEM/Administrators DACL.
        match pf_paths::write_secret_file(&path, next.as_bytes()) {
            Ok(()) => println!("PUNKTFUNK_HOST_CMD=serve → {}", path.display()),
            Err(e) => eprintln!("warning: {} not written: {e}", path.display()),
        }
    }
    if pinned && choice == Some(false) {
        println!("host.env's PUNKTFUNK_HOST_CMD passes --gamestream, which keeps GameStream on");
    }
}

/// `(store value, host.env rewrite, custom command passes --gamestream)` for `service install`.
pub(super) fn gamestream_migration(
    text: &str,
    choice: Option<bool>,
) -> (Option<bool>, Option<String>, bool) {
    let cmd = text
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("PUNKTFUNK_HOST_CMD="))
        .map(|v| v.trim().trim_matches('"'));
    match cmd {
        None | Some("serve --gamestream") => (
            choice.or(Some(true)),
            Some(with_env_line(text, "PUNKTFUNK_HOST_CMD", "serve")),
            false,
        ),
        Some(cmd) => (
            choice,
            None,
            cmd.split_whitespace()
                .any(|a| a == "--gamestream" || a == "--moonlight"),
        ),
    }
}

/// `text` with its last live `key=` line set to `value`, else one appended. Last wins, as
/// [`load_host_env`] and [`mgmt_port`] read it.
pub(super) fn with_env_line(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let line = format!("{key}={value}");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match lines
        .iter()
        .rposition(|l| l.trim_start().starts_with(&prefix))
    {
        Some(i) => lines[i] = line,
        None => lines.push(line),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Write `key=value` into the existing host.env, keeping every other line.
pub(super) fn set_host_env_line(key: &str, value: &str) -> Result<()> {
    let path = host_env_path();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    // `write_secret_file` re-asserts the SYSTEM/Administrators DACL.
    pf_paths::write_secret_file(&path, with_env_line(&text, key, value).as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    println!("{key}={value} → {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer's `--gamestream` moves to the store without turning GameStream off.
    #[test]
    fn gamestream_moves_from_the_host_command_to_the_store() {
        let legacy = "# PUNKTFUNK_HOST_CMD=serve\nPUNKTFUNK_HOST_CMD=serve --gamestream\n";
        let (store, text, pinned) = gamestream_migration(legacy, None);
        assert_eq!(store, Some(true));
        assert_eq!(
            text.as_deref(),
            Some("# PUNKTFUNK_HOST_CMD=serve\nPUNKTFUNK_HOST_CMD=serve\n")
        );
        assert!(!pinned);
        // No line ran `serve --gamestream`.
        let (store, text, _) = gamestream_migration("RUST_LOG=info\n", None);
        assert_eq!(store, Some(true));
        assert_eq!(
            text.as_deref(),
            Some("RUST_LOG=info\nPUNKTFUNK_HOST_CMD=serve\n")
        );
        assert_eq!(gamestream_migration(legacy, Some(false)).0, Some(false));
        // `serve` keeps whatever the store holds unless the installer chose.
        assert_eq!(
            gamestream_migration("PUNKTFUNK_HOST_CMD=serve\n", None),
            (None, None, false)
        );
        assert_eq!(
            gamestream_migration("PUNKTFUNK_HOST_CMD=serve\n", Some(true)),
            (Some(true), None, false)
        );
        assert_eq!(
            gamestream_migration(
                "PUNKTFUNK_HOST_CMD=serve --gamestream --open\n",
                Some(false)
            ),
            (Some(false), None, true)
        );
    }
}
