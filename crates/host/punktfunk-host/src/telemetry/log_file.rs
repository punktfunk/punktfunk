//! `<config_dir>/logs/host.log` for a serving Mac host, where launchd sends stderr nowhere.
//! The file holds at most [`CAP`] bytes and keeps one predecessor, `host.log.old`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

/// 10 MiB, the Windows service log's cap.
const CAP: u64 = 10 << 20;

static FILE: OnceLock<Mutex<Rotating>> = OnceLock::new();

struct Rotating {
    path: PathBuf,
    file: File,
    len: u64,
    cap: u64,
}

impl Rotating {
    fn open(path: PathBuf, cap: u64) -> std::io::Result<Rotating> {
        let file = append(&path)?;
        let len = file.metadata()?.len();
        Ok(Rotating {
            path,
            file,
            len,
            cap,
        })
    }

    /// Rotates first when `line` would take the file past the cap.
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.len > 0 && self.len + line.len() as u64 > self.cap {
            let mut old = self.path.clone().into_os_string();
            old.push(".old");
            std::fs::rename(&self.path, &old)?;
            self.file = append(&self.path)?;
            self.len = 0;
        }
        self.file.write_all(line)?;
        self.len += line.len() as u64;
        Ok(())
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Start writing the file. Lines logged before this reach stderr alone.
pub(crate) fn open() {
    let dir = pf_paths::config_dir().join("logs");
    let opened =
        pf_paths::create_private_dir(&dir).and_then(|()| Rotating::open(dir.join("host.log"), CAP));
    match opened {
        Ok(file) => {
            let _ = FILE.set(Mutex::new(file));
        }
        Err(error) => tracing::warn!(%error, dir = %dir.display(), "host log file not opened"),
    }
}

/// The `tracing` writer: one formatted event per write, dropped while no file is open.
pub(crate) struct Writer;

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(file) = FILE.get() {
            let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = file.write_line(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The panic hook's line. Skipped when the panic struck inside a write that holds the file.
pub(crate) fn write_panic(line: &str) {
    if let Some(Ok(mut file)) = FILE.get().map(Mutex::try_lock) {
        let _ = file.write_line(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file never passes its cap, and only the newest predecessor stays.
    #[test]
    fn the_log_rotates_at_its_cap_and_keeps_one_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.log");
        std::fs::write(&path, "x".repeat(90)).unwrap();
        let mut log = Rotating::open(path.clone(), 100).unwrap();
        log.write_line(b"0123456789\n").unwrap();
        let old = dir.path().join("host.log.old");
        assert_eq!(std::fs::read_to_string(&old).unwrap().len(), 90);
        log.write_line(b"second\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0123456789\nsecond\n"
        );
        for _ in 0..20 {
            log.write_line(b"0123456789\n").unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        }
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["host.log", "host.log.old"]);
    }
}
