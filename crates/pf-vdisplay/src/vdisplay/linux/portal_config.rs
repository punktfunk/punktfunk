//! Surgical edit of a user-owned xdg-desktop-portal config: one key in one block.
//!
//! wlr backends set `chooser_cmd` in xdpw's `[screencast]`; Hyprland sets
//! `custom_picker_binary` in xdph's `screencopy { … }`. Every other line stays
//! byte-for-byte. First touch of a file we did not write takes a one-time backup;
//! a later edit must not overwrite that original.
//!
//! Restore puts the recorded prior value back, or drops a key we invented.
//! Only `NotFound` means empty — a config we cannot read is a config we refuse
//! to rewrite. Tests in this file pin the merge, the restore, and the I/O traps.

use anyhow::{Context, Result};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Block<'a> {
    /// INI `[name]` … next `[` or EOF. `key=value`.
    Ini(&'a str),
    /// hyprlang `name {` … `}`. `key = value`, conventionally indented.
    Hyprlang(&'a str),
}

/// Marker beside a key we took over: `# punktfunk: previous <key> = <value>`.
///
/// In the file, not a sidecar: it must survive SIGKILL, a reboot that empties
/// `$XDG_RUNTIME_DIR`, and an uninstall that leaves the config, and it must be
/// in the same atomic write as the change. A sidecar fails all four.
const PRIOR: &str = "# punktfunk: previous";
const PRIOR_NONE: &str = "(none)";

/// `$XDG_CONFIG_HOME`, else `$HOME/.config`: where xdpw and xdph read their config.
/// Never a path relative to the cwd.
pub(crate) fn user_config_dir() -> Result<std::path::PathBuf> {
    Some(pf_paths::xdg_home("XDG_CONFIG_HOME", ".config"))
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("neither XDG_CONFIG_HOME nor HOME is an absolute path"))
}

fn opens(line: &str, block: Block<'_>) -> bool {
    let t = line.trim();
    match block {
        Block::Ini(name) => t == format!("[{name}]"),
        // `name {` — also `name{` and extra spaces.
        Block::Hyprlang(name) => {
            t.strip_suffix('{').map(str::trim_end) == Some(name) || t == format!("{name} {{")
        }
    }
}

/// Match the key only. The value moves with `$XDG_RUNTIME_DIR`; a changed value
/// is still ours to replace.
fn assigns(line: &str, key: &str) -> bool {
    line.split('=').next().is_some_and(|lhs| lhs.trim() == key)
}

/// `(header index, one past last line)`, or `None` if the block is missing.
fn block_span(lines: &[&str], block: Block<'_>) -> Option<(usize, usize)> {
    let open_at = lines.iter().position(|l| opens(l, block))?;
    let end_at = lines
        .iter()
        .enumerate()
        .skip(open_at + 1)
        .find(|(_, l)| match block {
            Block::Ini(_) => l.trim_start().starts_with('['),
            Block::Hyprlang(_) => l.trim() == "}",
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    Some((open_at, end_at))
}

pub(crate) fn current_value(existing: &str, block: Block<'_>, key: &str) -> Option<String> {
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    (open_at + 1..end_at)
        .find(|&i| assigns(lines[i], key))
        .map(|i| {
            lines[i]
                .split_once('=')
                .map_or("", |(_, v)| v)
                .trim()
                .to_string()
        })
}

/// Prior from our marker: `Some(Some(v))` they had `v`, `Some(None)` the key
/// was absent, `None` we never took it over.
pub(crate) fn prior_value(existing: &str, block: Block<'_>, key: &str) -> Option<Option<String>> {
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    let want = format!("{PRIOR} {key} =");
    let raw = (open_at + 1..end_at)
        .map(|i| lines[i].trim())
        .find_map(|l| l.strip_prefix(&want))?
        .trim()
        .to_string();
    Some((raw != PRIOR_NONE).then_some(raw))
}

/// Put the recorded prior value back, or drop the key if there was none.
/// `None` when no marker — the file is not ours.
///
/// Do not just delete our line: the portal default is not what the user had.
pub(crate) fn restore(existing: &str, block: Block<'_>, key: &str) -> Option<String> {
    let prior = prior_value(existing, block, key)?;
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    let marker = format!("{PRIOR} {key} =");
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let inside = i > open_at && i < end_at;
        if inside && line.trim().starts_with(&marker) {
            continue;
        }
        if inside && assigns(line, key) {
            // Keep this line's indent and the grammar's separator. `None` →
            // drop, do not blank — they never had this key.
            if let Some(v) = &prior {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                let sep = match block {
                    Block::Ini(_) => "=",
                    Block::Hyprlang(_) => " = ",
                };
                out.push(format!("{indent}{key}{sep}{v}"));
            }
            continue;
        }
        out.push((*line).to_string());
    }
    let mut joined = out.join("\n");
    if existing.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    Some(joined)
}

/// The [`PRIOR`] marker is written once. A later edit (shim path moves with
/// `$XDG_RUNTIME_DIR`) must not record our previous value as the user's.
pub(crate) fn upsert(existing: &str, block: Block<'_>, key: &str, value: &str) -> String {
    let sep = match block {
        Block::Ini(_) => "=",
        Block::Hyprlang(_) => " = ",
    };
    let assignment = |indent: &str| format!("{indent}{key}{sep}{value}");

    let lines: Vec<&str> = existing.lines().collect();
    let Some((open_at, end_at)) = block_span(&lines, block) else {
        // No key was here; the marker records that so restore removes our line.
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let marker = |indent: &str| format!("{indent}{PRIOR} {key} = {PRIOR_NONE}");
        out.push_str(&match block {
            Block::Ini(name) => format!("[{name}]\n{}\n{}\n", marker(""), assignment("")),
            Block::Hyprlang(name) => format!(
                "{name} {{\n{}\n{}\n}}\n",
                marker("    "),
                assignment("    ")
            ),
        });
        return out;
    };

    // Marker already holds the user's value; re-recording would store our shim path.
    let marked = prior_value(existing, block, key).is_some();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    if let Some(i) = (open_at + 1..end_at).find(|&i| assigns(lines[i], key)) {
        let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
        let had = lines[i]
            .split_once('=')
            .map_or("", |(_, v)| v)
            .trim()
            .to_string();
        out[i] = assignment(&indent);
        if !marked {
            out.insert(i, format!("{indent}{PRIOR} {key} = {had}"));
        }
    } else {
        let indent = match block {
            Block::Ini(_) => "",
            Block::Hyprlang(_) => "    ",
        };
        if !marked {
            out.insert(end_at, format!("{indent}{PRIOR} {key} = {PRIOR_NONE}"));
            out.insert(end_at + 1, assignment(indent));
        } else {
            out.insert(end_at, assignment(indent));
        }
    }
    let mut joined = out.join("\n");
    if existing.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Backup the original once, the first time we touch a file we did not write.
/// `true` means the file changed — the caller restarts the portal only then.
///
/// Only [`ErrorKind::NotFound`] means empty. Any other read failure (non-UTF-8
/// included) is a refuse-to-rewrite: `upsert("")` would replace the user's file
/// with only our block, and the backup is skipped because there is nothing to
/// back up.
pub(crate) fn ensure_key(path: &Path, block: Block<'_>, key: &str, value: &str) -> Result<bool> {
    // Backup is owed on the bytes on disk, not on what decoded.
    let raw = match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "read {} — refusing to rewrite an unread portal config",
                    path.display()
                )
            })
        }
    };
    let existing = match &raw {
        Some(bytes) => std::str::from_utf8(bytes)
            .with_context(|| {
                format!(
                    "{} is not UTF-8 — refusing to rewrite it (the one key we own is not worth \
                     losing the rest of the file for; fix or move the file and reconnect)",
                    path.display()
                )
            })?
            .to_string(),
        None => String::new(),
    };
    let updated = upsert(&existing, block, key, value);
    if updated == existing {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    // `create_new`: a later edit must not overwrite the user's original with ours.
    if let Some(bytes) = raw.as_deref().filter(|b| !b.is_empty()) {
        let backup = path.with_extension("punktfunk-backup");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = f.write_all(bytes);
                tracing::info!(
                    backup = %backup.display(),
                    "backed up the existing portal config before editing it"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => tracing::warn!(
                backup = %backup.display(),
                error = %e,
                "existing portal config not backed up — editing it anyway"
            ),
        }
    }
    write_atomic(path, updated.as_bytes())?;
    Ok(true)
}

/// Non-UTF-8 and any error other than missing: a config we cannot read is a
/// config we refuse to rewrite.
fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            std::str::from_utf8(&bytes)
                .with_context(|| {
                    format!("{} is not UTF-8 — refusing to rewrite it", path.display())
                })?
                .to_string(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Both `None` if we have never touched the file (or it does not exist).
pub(crate) fn peek(
    path: &Path,
    block: Block<'_>,
    key: &str,
) -> (Option<String>, Option<Option<String>>) {
    let Ok(Some(text)) = read_config(path) else {
        return (None, None);
    };
    (
        current_value(&text, block, key),
        prior_value(&text, block, key),
    )
}

/// No marker → leave the file alone and report `false`. Safe to call
/// unconditionally at teardown, including on a box we never touched.
pub(crate) fn restore_key(path: &Path, block: Block<'_>, key: &str) -> Result<bool> {
    let Some(existing) = read_config(path)? else {
        return Ok(false);
    };
    let Some(updated) = restore(&existing, block, key) else {
        return Ok(false);
    };
    if updated == existing {
        return Ok(false);
    }
    write_atomic(path, updated.as_bytes())?;
    Ok(true)
}

/// Sibling temp file, then `rename` over `path`. Same directory: rename is
/// atomic only within one filesystem. Copy the original's mode so 0600 does
/// not come back at the umask. Do not `fs::write` — it truncates first, and a
/// crash between truncate and fill leaves the user's config empty.
///
/// Follow a symlink first. `rename(2)` replaces the link itself; stow / chezmoi
/// / home-manager keep `~/.config` files as repo links.
///
/// Do not paper over a read-only target (`/nix/store`): fail the write. Renaming
/// over the link would detach a declaratively managed file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let resolved = follow_link(path);
    let path = resolved.as_path();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    // Per-process name: two hosts editing the same config must not fill one another's temp file.
    let tmp = dir.join(format!(".{stem}.punktfunk-{}.tmp", std::process::id()));
    let write = || -> Result<()> {
        {
            let mut f =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            // The rename must not publish a name whose contents are still in the page cache only.
            f.sync_all()
                .with_context(|| format!("sync {}", tmp.display()))?;
        } // closed before the rename — Windows is far happier renaming a file nobody holds open.
        if let Ok(md) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, md.permissions());
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    };
    let r = write();
    if r.is_err() {
        // Never leave a half-written dotfile beside the user's config.
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `path` itself when it is not a link, including when it does not exist yet.
///
/// `symlink_metadata`, not `metadata`: the question is what `path` is. A
/// dangling link is resolved from its target text — `canonicalize` refuses a
/// missing target, but `fs::write` through such a link creates it.
fn follow_link(path: &Path) -> std::path::PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => std::fs::canonicalize(path)
            .or_else(|_| {
                std::fs::read_link(path).map(|target| {
                    if target.is_absolute() {
                        target
                    } else {
                        // A relative link is relative to the DIRECTORY holding it.
                        path.parent().unwrap_or_else(|| Path::new(".")).join(target)
                    }
                })
            })
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_block_is_appended_and_the_rest_survives() {
        let user = "[somethingelse]\nkeep=me\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "cat x");
        assert!(
            out.contains("[somethingelse]\nkeep=me"),
            "user content kept"
        );
        assert!(out.contains(
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x"
        ));
    }

    #[test]
    fn an_existing_key_is_replaced_in_place() {
        let user = "[screencast]\nchooser_type=simple\nchooser_cmd=OLD\noutput_name=DP-1\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "NEW");
        assert!(out.contains("chooser_cmd=NEW"));
        // OLD may live only on the restore marker, not as a live assignment.
        assert!(!out.lines().any(|l| l.trim() == "chooser_cmd=OLD"));
        assert!(out.contains("# punktfunk: previous chooser_cmd = OLD"));
        assert!(out.contains("chooser_type=simple"), "sibling key kept");
        assert!(out.contains("output_name=DP-1"), "sibling key kept");
    }

    #[test]
    fn a_key_missing_from_an_existing_block_is_inserted_into_it() {
        let user = "[screencast]\nchooser_type=simple\n\n[other]\nx=1\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "cat x");
        let cmd = out.find("chooser_cmd").expect("inserted");
        let other = out.find("[other]").expect("kept");
        assert!(
            cmd < other,
            "must land inside [screencast], not after [other]"
        );
        assert!(out.contains("x=1"), "the later section survives");
    }

    #[test]
    fn hyprlang_blocks_are_edited_in_place_too() {
        let user = "misc {\n    keep = 1\n}\n\nscreencopy {\n    custom_picker_binary = OLD\n    allow_token_by_default = true\n}\n";
        let out = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/shim.sh",
        );
        assert!(out.contains("custom_picker_binary = /run/user/1000/shim.sh"));
        assert!(!out
            .lines()
            .any(|l| l.trim() == "custom_picker_binary = OLD"));
        assert!(out.contains("# punktfunk: previous custom_picker_binary = OLD"));
        assert!(
            out.contains("allow_token_by_default = true"),
            "sibling kept"
        );
        assert!(out.contains("misc {\n    keep = 1\n}"), "other block kept");
    }

    #[test]
    fn a_hyprlang_key_missing_from_its_block_lands_before_the_brace() {
        let user = "screencopy {\n    allow_token_by_default = true\n}\n";
        let out = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        let key = out.find("custom_picker_binary").expect("inserted");
        let brace = out.find('}').expect("kept");
        assert!(key < brace, "must land inside the block");
    }

    /// Idempotence is what keeps the caller from restarting the portal on every connect.
    #[test]
    fn a_second_pass_changes_nothing() {
        let once = upsert("", Block::Ini("screencast"), "chooser_cmd", "cat x");
        let twice = upsert(&once, Block::Ini("screencast"), "chooser_cmd", "cat x");
        assert_eq!(once, twice);
        let h1 = upsert(
            "",
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        let h2 = upsert(
            &h1,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        assert_eq!(h1, h2);
    }

    #[test]
    fn an_empty_file_yields_just_the_block() {
        assert_eq!(
            upsert("", Block::Ini("screencast"), "k", "v"),
            "[screencast]\n# punktfunk: previous k = (none)\nk=v\n"
        );
    }

    #[test]
    fn an_omarchy_picker_survives_the_round_trip() {
        let user = "screencopy {\n    allow_token_by_default = true\n    custom_picker_binary = hyprland-preview-share-picker\n}\n";
        let ours = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/pf-picker.sh",
        );
        assert!(ours.contains("custom_picker_binary = /run/user/1000/pf-picker.sh"));
        assert_eq!(
            prior_value(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            Some(Some("hyprland-preview-share-picker".to_string()))
        );
        let back = restore(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary")
            .expect("a file we took over is restorable");
        assert_eq!(
            back, user,
            "the user's file must come back exactly as it was"
        );
    }

    /// A later takeover must not record our shim path as theirs — restore would
    /// then put back a dead `$XDG_RUNTIME_DIR` path.
    #[test]
    fn a_second_takeover_keeps_the_first_prior_value() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        let once = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/a",
        );
        let twice = upsert(
            &once,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/b",
        );
        assert!(twice.contains("custom_picker_binary = /run/b"));
        assert_eq!(
            restore(
                &twice,
                Block::Hyprlang("screencopy"),
                "custom_picker_binary"
            )
            .as_deref(),
            Some(user)
        );
    }

    /// Restore removes a key we invented. Blanking it would leave a setting they
    /// never had.
    #[test]
    fn a_key_we_invented_is_removed_on_restore_not_blanked() {
        let user = "screencopy {\n    allow_token_by_default = true\n}\n";
        let ours = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/a",
        );
        assert_eq!(
            prior_value(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            Some(None)
        );
        assert_eq!(
            restore(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary").as_deref(),
            Some(user)
        );
    }

    /// No marker → no-op, not a deletion. Teardown calls this unconditionally.
    #[test]
    fn a_file_without_our_marker_is_not_ours_to_restore() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        assert_eq!(
            restore(user, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            None
        );
        assert_eq!(restore("", Block::Ini("screencast"), "chooser_cmd"), None);
    }

    #[test]
    fn the_ini_grammar_reverses_too() {
        let user = "[screencast]\nchooser_type=simple\nchooser_cmd=slurp\noutput_name=DP-1\n";
        let ours = upsert(user, Block::Ini("screencast"), "chooser_cmd", "/run/a");
        assert_eq!(
            restore(&ours, Block::Ini("screencast"), "chooser_cmd").as_deref(),
            Some(user)
        );
    }

    #[test]
    fn current_value_reads_what_is_there_now() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        assert_eq!(
            current_value(user, Block::Hyprlang("screencopy"), "custom_picker_binary").as_deref(),
            Some("theirs")
        );
        assert_eq!(
            current_value(user, Block::Hyprlang("screencopy"), "nope"),
            None
        );
    }
}

/// Filesystem-only pins for [`ensure_key`]. No compositor, so they run on every platform.
#[cfg(test)]
mod io_tests {
    use super::*;

    /// This crate does not depend on `tempfile`; the dir is removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("pf-vd-portalcfg-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn backup_of(p: &Path) -> std::path::PathBuf {
        p.with_extension("punktfunk-backup")
    }

    /// An undecodable config is left byte-identical. Do not treat a read failure
    /// as empty — `upsert("")` would replace the file with only our block.
    #[test]
    fn a_non_utf8_config_is_refused_not_replaced() {
        let s = Scratch::new("nonutf8");
        let p = s.path("config");
        // 0xff in a comment: otherwise ordinary INI, not UTF-8.
        let raw: &[u8] = b"[screencast]\n# r\xffgler\nchooser_type=simple\noutput_name=DP-1\n";
        std::fs::write(&p, raw).expect("seed");
        let err = ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x")
            .expect_err("an unreadable config must not be rewritten");
        assert!(
            format!("{err:#}").contains("not UTF-8"),
            "the error must name the real cause: {err:#}"
        );
        assert_eq!(
            std::fs::read(&p).expect("still there"),
            raw,
            "byte-identical"
        );
        assert!(
            !backup_of(&p).exists(),
            "nothing was edited, so nothing is owed a backup"
        );
    }

    #[test]
    fn a_missing_file_is_created_without_a_backup() {
        let s = Scratch::new("missing");
        let p = s.path("nested").join("config");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write"));
        assert_eq!(
            std::fs::read_to_string(&p).expect("created"),
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x\n"
        );
        assert!(!backup_of(&p).exists());
    }

    #[test]
    fn restore_key_puts_the_users_picker_back_byte_for_byte() {
        let s = Scratch::new("restore");
        let p = s.path("xdph.conf");
        let user = "screencopy {\n    allow_token_by_default = true\n    custom_picker_binary = hyprland-preview-share-picker\n}\n";
        std::fs::write(&p, user).expect("seed");
        let block = Block::Hyprlang("screencopy");
        assert!(
            ensure_key(&p, block, "custom_picker_binary", "/run/user/1000/pf.sh").expect("take")
        );
        assert_eq!(
            peek(&p, block, "custom_picker_binary"),
            (
                Some("/run/user/1000/pf.sh".to_string()),
                Some(Some("hyprland-preview-share-picker".to_string()))
            )
        );
        assert!(restore_key(&p, block, "custom_picker_binary").expect("restore"));
        assert_eq!(std::fs::read_to_string(&p).expect("restored"), user);
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("second restore"));
        assert_eq!(std::fs::read_to_string(&p).expect("unchanged"), user);
    }

    #[test]
    fn restore_key_is_a_no_op_on_a_config_we_never_took_over() {
        let s = Scratch::new("restore-noop");
        let p = s.path("xdph.conf");
        let block = Block::Hyprlang("screencopy");
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("absent file"));
        assert!(!p.exists(), "restoring must not CREATE a config");
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        std::fs::write(&p, user).expect("seed");
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("not ours"));
        assert_eq!(std::fs::read_to_string(&p).expect("intact"), user);
    }

    /// After a second edit the backup still holds the user's original, not our
    /// previous output. That is what `create_new` buys.
    #[test]
    fn the_backup_holds_the_original_across_two_edits() {
        let s = Scratch::new("backup");
        let p = s.path("config");
        let pristine = "[screencast]\nchooser_type=simple\noutput_name=DP-1\n";
        std::fs::write(&p, pristine).expect("seed");
        assert!(
            ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat /run/a").expect("1st")
        );
        assert!(
            ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat /run/b").expect("2nd")
        );
        assert_eq!(
            std::fs::read_to_string(backup_of(&p)).expect("backup"),
            pristine
        );
        let now = std::fs::read_to_string(&p).expect("edited");
        assert!(
            now.contains("chooser_cmd=cat /run/b"),
            "the second value won"
        );
        assert!(
            now.contains("output_name=DP-1"),
            "the user's other keys survived"
        );
    }

    /// Already-correct file reports `false` and is not rewritten. The caller
    /// restarts the portal on `true`.
    #[test]
    fn an_unchanged_file_returns_false_and_does_not_rewrite() {
        let s = Scratch::new("unchanged");
        let p = s.path("config");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("1st"));
        let after_first = std::fs::read_to_string(&p).expect("written");
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .expect("mtime");
        assert!(
            !ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("2nd"),
            "an unchanged config must report no change"
        );
        assert_eq!(
            std::fs::read_to_string(&p).expect("still there"),
            after_first
        );
        assert_eq!(
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .expect("mtime"),
            mtime,
            "the file must not have been touched at all"
        );
    }

    #[test]
    fn the_write_is_atomic_and_leaves_no_temp_behind() {
        let s = Scratch::new("atomic");
        let p = s.path("config");
        std::fs::write(&p, "[other]\nkeep=me\n").expect("seed");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write"));
        let names: Vec<String> = std::fs::read_dir(&s.0)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".tmp")),
            "temp file left behind: {names:?}"
        );
        assert!(std::fs::read_to_string(&p)
            .expect("edited")
            .contains("keep=me"));
    }

    /// Edit through the symlink; do not `rename` over it. stow / chezmoi /
    /// home-manager keep `~/.config` files as repo links.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_edited_through_the_link() {
        let s = Scratch::new("symlink");
        let repo = s.path("dotfiles");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let real = repo.join("xdph.conf");
        std::fs::write(
            &real,
            "screencopy {\n    allow_token_by_default = true\n}\n",
        )
        .expect("seed");
        let link = s.path("xdph.conf");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(ensure_key(
            &link,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/shim.sh",
        )
        .expect("write"));

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the dotfiles link was replaced by a detached regular file"
        );
        let target = std::fs::read_to_string(&real).expect("the repo file");
        assert!(
            target.contains("custom_picker_binary = /run/user/1000/shim.sh"),
            "the edit never reached the repo file: {target}"
        );
        assert!(
            target.contains("allow_token_by_default = true"),
            "the user's own keys survived"
        );
    }

    /// A dangling link is written through to its target, as `fs::write` would.
    /// Replacing the link would detach it.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_written_through_to_its_target() {
        let s = Scratch::new("dangling");
        let repo = s.path("dotfiles");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let real = repo.join("config");
        let link = s.path("config");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(
            ensure_key(&link, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write")
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the link was replaced instead of written through"
        );
        assert_eq!(
            std::fs::read_to_string(&real).expect("target created"),
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x\n"
        );
    }
}
