//! Hardened persistence below an explicit service-owned root.
//!
//! `SecretRoot` rejects symbolic links, Unix hard links, and Windows reparse
//! points before delegating modes and DACLs to `pf-paths`. Writes use an
//! owner-only sibling temporary, sync it, and replace the live file. A backup
//! plus the temporary file lets `LedgerStore` recover an interrupted replace.
//! Ledger candidates are validated before use, and the highest generation
//! wins. No helper in this module consults process-global config paths.

use crate::model::{Ledger, ValidationError};
use std::io;
use std::path::{Component, Path, PathBuf};

pub const LEDGER_FILE: &str = "ledger.json";

/// `seats` under the host config dir: `%ProgramData%\punktfunk\seats` for the SYSTEM service.
pub fn default_root() -> PathBuf {
    pf_paths::config_dir().join("seats")
}
pub const MAX_LEDGER_BYTES: usize = 64 * 1024;
const MAX_SECRET_BYTES: usize = 1_048_576;

#[derive(Clone, Debug)]
pub struct SecretRoot {
    root: PathBuf,
}

impl SecretRoot {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let requested = root.as_ref();
        if requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "secret root must not contain parent-directory components",
            ));
        }
        let root = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            std::env::current_dir()?.join(requested)
        };
        reject_links_in_path(&root)?;
        pf_paths::create_private_dir(&root)?;
        reject_links_in_path(&root)?;
        if !std::fs::metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn open_child_dir(&self, name: &str) -> io::Result<Self> {
        self.harden()?;
        let path = self.child(name)?;
        reject_link(&path)?;
        Self::open(path)
    }

    pub fn remove_file(&self, name: &str) -> io::Result<()> {
        self.harden()?;
        let mut removed = false;
        for suffix in ["", ".tmp", ".bak", ".bak.tmp"] {
            let path = self.child(&format!("{name}{suffix}"))?;
            reject_link(&path)?;
            match std::fs::remove_file(path) {
                Ok(()) => removed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if removed {
            sync_directory(&self.root)?;
        }
        Ok(())
    }

    #[cfg(windows)]
    pub(crate) fn child_path(&self, name: &str) -> io::Result<PathBuf> {
        self.harden()?;
        let path = self.child(name)?;
        reject_link(&path)?;
        Ok(path)
    }

    pub fn write_atomic(&self, name: &str, contents: &[u8]) -> io::Result<()> {
        if contents.len() > MAX_SECRET_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "secret file exceeds the 1 MiB persistence cap",
            ));
        }
        self.harden()?;
        let live = self.child(name)?;
        let temp = self.child(&format!("{name}.tmp"))?;
        let backup = self.child(&format!("{name}.bak"))?;
        let backup_temp = self.child(&format!("{name}.bak.tmp"))?;
        for path in [&live, &temp, &backup, &backup_temp] {
            reject_link(path)?;
        }

        pf_paths::write_secret_file(&temp, contents)?;
        self.replace(&live, &temp, &backup, &backup_temp)?;
        sync_directory(&self.root)?;
        Ok(())
    }

    pub fn read_current(&self, name: &str, max: usize) -> io::Result<Option<Vec<u8>>> {
        self.harden()?;
        let path = self.child(name)?;
        read_regular(&path, max)
    }

    pub(crate) fn candidates(&self, name: &str, max: usize) -> io::Result<Vec<Candidate>> {
        self.harden()?;
        let paths = [
            (CandidateKind::Live, self.child(name)?),
            (
                CandidateKind::Temporary,
                self.child(&format!("{name}.tmp"))?,
            ),
            (CandidateKind::Backup, self.child(&format!("{name}.bak"))?),
        ];
        let mut found = Vec::new();
        for (kind, path) in paths {
            if let Some(bytes) = read_regular(&path, max)? {
                found.push(Candidate { kind, bytes });
            }
        }
        Ok(found)
    }

    pub(crate) fn cleanup_temporary(&self, name: &str) -> io::Result<()> {
        for suffix in [".tmp", ".bak.tmp"] {
            let path = self.child(&format!("{name}{suffix}"))?;
            reject_link(&path)?;
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Re-checks the root is still a real directory reached through no link.
    ///
    /// The DACL is applied once in [`SecretRoot::open`]. Re-applying it per
    /// access costs two `icacls` processes each time and would only defend
    /// against someone who can already write inside a SYSTEM-only directory,
    /// while the link check is cheap and is the guard that matters.
    fn harden(&self) -> io::Result<()> {
        reject_links_in_path(&self.root)
    }

    fn child(&self, name: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        let safe_name = !matches!(name, "." | "..")
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        match (safe_name, components.next(), components.next()) {
            (true, Some(Component::Normal(_)), None) => Ok(self.root.join(name)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "secret file name must be one safe path component",
            )),
        }
    }

    fn replace(
        &self,
        live: &Path,
        temp: &Path,
        backup: &Path,
        backup_temp: &Path,
    ) -> io::Result<()> {
        if let Some(old) = read_regular(live, MAX_SECRET_BYTES)? {
            pf_paths::write_secret_file(backup_temp, &old)?;
            replace_path(backup_temp, backup)?;
        }
        replace_path(temp, live)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandidateKind {
    Live,
    Temporary,
    Backup,
}

impl CandidateKind {
    fn rank(self) -> u8 {
        match self {
            Self::Live => 3,
            Self::Temporary => 2,
            Self::Backup => 1,
        }
    }
}

pub(crate) struct Candidate {
    pub kind: CandidateKind,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LedgerStore {
    root: SecretRoot,
}

impl LedgerStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Ok(Self {
            root: SecretRoot::open(root)?,
        })
    }

    pub fn root(&self) -> &SecretRoot {
        &self.root
    }

    pub fn load(&self) -> Result<Ledger, StoreError> {
        let candidates = self.root.candidates(LEDGER_FILE, MAX_LEDGER_BYTES)?;
        if candidates.is_empty() {
            return Ok(Ledger::new());
        }

        let mut errors = Vec::new();
        let mut valid = Vec::new();
        for candidate in candidates {
            match serde_json::from_slice::<Ledger>(&candidate.bytes) {
                Ok(ledger) => match ledger.validate() {
                    Ok(()) => valid.push((ledger, candidate)),
                    Err(error) => errors.push(error.to_string()),
                },
                Err(error) => errors.push(error.to_string()),
            }
        }
        let Some((ledger, source)) = valid
            .into_iter()
            .max_by_key(|(ledger, candidate)| (ledger.generation, candidate.kind.rank()))
        else {
            return Err(StoreError::Corrupt(errors.join("; ")));
        };
        if source.kind != CandidateKind::Live {
            self.root.write_atomic(LEDGER_FILE, &source.bytes)?;
        }
        self.root.cleanup_temporary(LEDGER_FILE)?;
        Ok(ledger)
    }

    pub fn save(&self, ledger: &Ledger) -> Result<(), StoreError> {
        ledger.validate()?;
        let mut bytes = serde_json::to_vec_pretty(ledger)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_LEDGER_BYTES {
            return Err(StoreError::Corrupt(format!(
                "serialized ledger exceeds {MAX_LEDGER_BYTES} bytes"
            )));
        }
        self.root.write_atomic(LEDGER_FILE, &bytes)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("seat storage I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("seat ledger JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("seat ledger is invalid: {0}")]
    Validation(#[from] ValidationError),
    #[error("no valid seat ledger candidate remains: {0}")]
    Corrupt(String),
}

fn read_regular(path: &Path, max: usize) -> io::Result<Option<Vec<u8>>> {
    reject_link(path)?;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > max as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds {max} bytes", path.display()),
        ));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} grew beyond {max} bytes", path.display()),
        ));
    }
    Ok(Some(bytes))
}

#[cfg(windows)]
fn reject_links_in_path(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    let components: Vec<_> = path.components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_link(&metadata) => return Err(link_error(&current)),
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a directory", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn reject_links_in_path(path: &Path) -> io::Result<()> {
    reject_link(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn reject_link(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link(&metadata) => Err(link_error(path)),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file() && metadata.nlink() > 1 {
            return true;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn link_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is a link or reparse point", path.display()),
    )
}

#[cfg(windows)]
fn replace_path(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both vectors are live NUL-terminated paths in the same directory.
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR(source.as_ptr()),
            windows::core::PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

#[cfg(not(windows))]
fn replace_path(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
