//! Persistent client log file: `%LOCALAPPDATA%\punktfunk\logs\client.log`.
//!
//! The shell is a `windows_subsystem` binary and spawns `punktfunk-session` with
//! `CREATE_NO_WINDOW` — a normal GUI/MSIX launch has NO console, so without a file every log
//! line (the shell's and the session's whole receive/decode/present trail) would be lost.
//!
//! Mirrors the host's convention (`%ProgramData%\punktfunk\logs`, size-capped): a file over
//! 10 MB is rotated to `.old` at the next client start, one generation kept. Everything is
//! best-effort — a missing/locked directory degrades to plain stderr, never a startup failure.
//!
//! Two paths, deliberately: [`log_dir`] is what we open files through, [`real_dir`] is where
//! they actually land. Under MSIX those differ, and only the second one is fit to show a user
//! or hand to Explorer.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Rotate at the next start once the file exceeds this (the host's cap).
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

static SINK: OnceLock<Option<Arc<Mutex<File>>>> = OnceLock::new();

/// The log directory we WRITE through: `%LOCALAPPDATA%\punktfunk\logs`.
///
/// Correct to open files under, but NOT necessarily where the bytes land — see [`real_dir`].
/// Anything shown to a user or handed to another process wants that one instead.
fn log_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"punktfunk\logs"))
}

/// The log directory as it exists ON DISK — Settings ▸ About's "Open log folder" opens this in
/// Explorer, and [`path`] names it in the startup line and the failed-spawn banner.
///
/// A packaged (MSIX) client is full-trust, and Windows redirects a packaged app's
/// `%LOCALAPPDATA%` writes into its private `…\Packages\<family>\LocalCache\Local\…`. We create
/// and append through that redirection without ever seeing it, so [`log_dir`] is the right path
/// to WRITE to yet names a directory that never exists on disk. Explorer runs OUTSIDE the
/// container: it resolves the literal path, finds nothing, and silently falls back to the user's
/// Documents folder. An unpackaged run creates the literal directory for real.
///
/// Canonicalizing the directory we just created resolves through the redirection on a packaged
/// run and changes nothing on an unpackaged one, so there is no package identity to detect.
pub(crate) fn real_dir() -> Option<PathBuf> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(std::fs::canonicalize(&dir).map_or(dir, strip_verbatim))
}

/// Undo the `\\?\` that [`std::fs::canonicalize`] always prefixes. Explorer refuses a verbatim
/// path — it would take the very same silent Documents fallback [`real_dir`] exists to avoid —
/// and it is noise in a line a user is meant to read and act on.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    // Scoped so the borrow ends before the `return p` below can move it.
    let head = match p.components().next() {
        Some(Component::Prefix(pre)) => match pre.kind() {
            // `\\?\C:\…` → `C:\…`
            Prefix::VerbatimDisk(drive) => Some(PathBuf::from(format!(r"{}:\", drive as char))),
            // `\\?\UNC\server\share\…` → `\\server\share\…` (a roaming profile on a share).
            // Built through `OsString`, which appends verbatim — `PathBuf::push` would apply
            // separator logic to the bare `\\` and mangle it.
            Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from(r"\\");
                unc.push(server);
                unc.push(r"\");
                unc.push(share);
                Some(PathBuf::from(unc))
            }
            // Already a plain path — nothing to undo.
            _ => None,
        },
        _ => None,
    };
    let Some(mut out) = head else { return p };
    // `skip(1)` drops the prefix; the `RootDir` that follows it is already in `head`.
    out.extend(
        p.components()
            .skip(1)
            .filter(|c| !matches!(c, Component::RootDir)),
    );
    out
}

/// The log file's path, for the "logs land here" startup line and the failed-spawn banner.
/// Resolved like [`real_dir`] — a path a user is told to check has to be the one on disk.
pub(crate) fn path() -> Option<PathBuf> {
    Some(real_dir()?.join("client.log"))
}

/// Open (rotating first) and cache the sink. Called once at startup, before the tracing
/// subscriber installs; every later [`tee`] shares the handle.
pub(crate) fn init() {
    SINK.get_or_init(|| {
        let dir = log_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("client.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            let old = dir.join("client.log.old");
            // Windows `rename` refuses an existing destination — drop the old generation first.
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Arc::new(Mutex::new(file)))
    });
}

/// A writer that duplicates onto stderr (dev runs from a terminal keep their interleaved
/// output) and the log file (a GUI launch has no console, so the file is what it keeps). The
/// tracing subscriber's `with_writer` factory and the session-stderr forwarder both use it.
pub(crate) struct Tee;

/// `with_writer` factory (`fn() -> Tee` satisfies `MakeWriter`).
pub(crate) fn tee() -> Tee {
    Tee
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(Some(f)) = SINK.get() {
            let _ = f.lock().unwrap().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(Some(f)) = SINK.get() {
            let _ = f.lock().unwrap().flush();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `canonicalize` actually returns for a local profile. Explorer treats a `\\?\`
    /// path as unresolvable and opens Documents instead, so the prefix has to come off.
    #[test]
    fn verbatim_disk_prefix_comes_off() {
        let p = PathBuf::from(r"\\?\C:\Users\ada\AppData\Local\punktfunk\logs");
        assert_eq!(
            strip_verbatim(p),
            PathBuf::from(r"C:\Users\ada\AppData\Local\punktfunk\logs")
        );
    }

    /// The MSIX-redirected form: same treatment, longer path.
    #[test]
    fn verbatim_disk_prefix_comes_off_for_the_package_local_cache() {
        let p = PathBuf::from(
            r"\\?\C:\Users\ada\AppData\Local\Packages\io.unom.punktfunk_8wekyb3d8bbwe\LocalCache\Local\punktfunk\logs",
        );
        assert_eq!(
            strip_verbatim(p),
            PathBuf::from(
                r"C:\Users\ada\AppData\Local\Packages\io.unom.punktfunk_8wekyb3d8bbwe\LocalCache\Local\punktfunk\logs"
            )
        );
    }

    /// A roaming profile on a share canonicalizes to `\\?\UNC\…`; the plain UNC form is what
    /// Explorer takes. `\\server\share` must survive intact — dropping either half, or letting
    /// `PathBuf::push`'s separator logic at the bare `\\`, yields a path that opens nothing.
    #[test]
    fn verbatim_unc_prefix_becomes_a_plain_unc_path() {
        let p = PathBuf::from(r"\\?\UNC\fileserv\profiles\ada\AppData\Local\punktfunk\logs");
        assert_eq!(
            strip_verbatim(p),
            PathBuf::from(r"\\fileserv\profiles\ada\AppData\Local\punktfunk\logs")
        );
    }

    /// An unpackaged dev run resolves to a path that was never verbatim — leave it alone.
    #[test]
    fn plain_path_is_untouched() {
        let p = PathBuf::from(r"C:\Users\ada\AppData\Local\punktfunk\logs");
        assert_eq!(strip_verbatim(p.clone()), p);
    }

    /// Whatever the run, the resolved directory is one Explorer can open: it exists, and it
    /// carries no verbatim prefix. This is the button's actual precondition.
    #[test]
    fn real_dir_is_an_openable_directory() {
        let Some(dir) = real_dir() else {
            return; // no LOCALAPPDATA (not a normal user session) — nothing to assert
        };
        assert!(dir.is_dir(), "{} is not a directory", dir.display());
        assert!(
            !dir.to_string_lossy().starts_with(r"\\?\"),
            "{} kept its verbatim prefix",
            dir.display()
        );
    }
}
