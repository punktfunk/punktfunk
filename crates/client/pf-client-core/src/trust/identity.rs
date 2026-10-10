//! This client's mTLS identity: the key pair it presents, the owner-only key file, the
//! label hosts file it under, and the SPAKE2 PIN ceremony that pins a host.

use super::config_dir;
#[cfg(windows)]
use anyhow::Context as _;
use anyhow::{anyhow, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::endpoint;

/// Persistent mTLS identity, generated once and presented on every connect.
///
/// The private key is owner-only wherever the directory is. Unix uses mode 0600.
/// Windows drops inherited ACEs and grants the owner, including when
/// `PUNKTFUNK_CONFIG_DIR` points outside `%APPDATA%`.
pub fn load_or_create_identity() -> Result<(String, String)> {
    let dir = config_dir()?;
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        // Older Unix builds left the key world-readable. Re-lock on load. Best-effort:
        // a read-only store still returns the key it already has. Windows keys only
        // ever lived in the per-user `%APPDATA%`, so nothing there needs a re-lock.
        #[cfg(unix)]
        lock_identity_perms(&dir, &kp);
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    // Unix: the directory is 0700 before the key is written. Windows locks the key
    // file alone in `write_private_key`; the directory keeps the ACL it came with.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(&cp, &c)?;
    write_private_key(&kp, k.as_bytes())?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

/// Write the mTLS private key owner-only.
///
/// Unix creates it mode 0600. Writing first and then chmod would expose the
/// bytes at the umask default. Windows creates an empty file, locks it to the
/// owner, then writes the bytes. A failed lock removes the file.
fn write_private_key(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(windows)]
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::write(path, [])?;
        if let Err(e) = restrict_to_owner(path) {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// One `icacls` call with no console window: the WinUI shell has no console, so a
/// plain spawn would open one. Failure is `restrict client key`.
#[cfg(windows)]
fn run_icacls(path: &std::path::Path, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = std::process::Command::new(crate::paths::system32("icacls.exe"))
        .arg(path)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("restrict client key")?;
    if status.success() {
        return Ok(());
    }
    anyhow::bail!("restrict client key: {status}");
}

/// Owner-only ACL on one file. `/reset` runs first because `/inheritance:r`
/// leaves an explicit grant in place.
#[cfg(windows)]
fn restrict_to_owner(path: &std::path::Path) -> Result<()> {
    run_icacls(path, &["/reset"])?;
    run_icacls(path, &["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
}

/// Best-effort dir 0700 / key 0600 on an existing store. Errors ignored: this never
/// loosens perms, so a failure leaves what was already there.
#[cfg(unix)]
fn lock_identity_perms(dir: &std::path::Path, key: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600));
}

/// Label a host files this client under. Re-export of `punktfunk_core::client::device_name`.
pub fn device_name() -> String {
    punktfunk_core::client::device_name()
}

/// SPAKE2 PIN ceremony. `device_name` is the label the host stores; 90 s covers a
/// human-typed PIN. Returns the verified host certificate fingerprint.
pub fn pair_with_host(
    addr: &str,
    port: u16,
    identity: &(String, String),
    pin: &str,
    device_name: &str,
) -> std::result::Result<[u8; 32], punktfunk_core::PunktfunkError> {
    NativeClient::pair(
        addr,
        port,
        (&identity.0, &identity.1),
        pin.trim(),
        device_name,
        std::time::Duration::from_secs(90),
    )
}

/// The host waits 180 s for its operator to decide; this outlasts it so the client hears
/// the host's own timeout.
pub const REQUEST_ACCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(185);

/// Request access without a PIN: blocks until the host's operator approves this device or
/// refuses it. `pin` is the advertised fingerprint (`None` trusts on first use); setting
/// `cancel` withdraws the request. Returns the host certificate fingerprint. Never streams.
pub fn request_access_to_host(
    addr: &str,
    port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    device_name: &str,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> std::result::Result<[u8; 32], punktfunk_core::PunktfunkError> {
    NativeClient::request_access(
        addr,
        port,
        (&identity.0, &identity.1),
        pin,
        device_name,
        REQUEST_ACCESS_TIMEOUT,
        cancel,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key file is owner-only on create. Unix is mode 0600. Windows is an
    /// owner ACE with inherited access removed.
    #[test]
    fn the_private_key_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-client-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("client-key.pem");
        write_private_key(&key, b"secret").unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // `icacls` prints localized account names; `/save` writes SDDL (UTF-16),
        // which does not follow the display language.
        #[cfg(windows)]
        {
            let saved = dir.join("key-acl.txt");
            let out = std::process::Command::new(crate::paths::system32("icacls.exe"))
                .arg(&key)
                .arg("/save")
                .arg(&saved)
                .output()
                .expect("save key acl");
            assert!(out.status.success(), "{out:?}");
            let raw = std::fs::read(&saved).unwrap();
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let text = String::from_utf16_lossy(&units);
            let dacl = text.lines().find(|l| l.starts_with("D:")).unwrap_or("");
            assert_eq!(dacl.trim(), "D:PAI(A;;FA;;;OW)", "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
