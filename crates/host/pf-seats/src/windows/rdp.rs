//! The trusted RDP pin: the SHA-256 of TermService's TLS leaf certificate, stored in the seat
//! root.
//!
//! The supervisor copies the pin into every keeper's bootstrap, and the keeper refuses any other
//! leaf certificate before CredSSP can send a credential.
//!
//! A leaf becomes the pin only when it answered on the loopback listener and the machine holds it
//! ([`machine_pins`]). Only administrators and SYSTEM write the stores `machine_pins` reads, so a
//! process that merely took the listener's port can't get its own certificate pinned.
//!
//! - Turning seats on and `punktfunk-seat-keeper trust --replace` record the leaf of that moment.
//!   The plain `trust` never replaces a different stored pin.
//! - A seat start that meets a new leaf the machine holds records it and starts. Any other new
//!   leaf refuses the start with `rdp_pin_changed`.

use super::seat_listener::{STOCK, WINSTATIONS};
use crate::persistence::SecretRoot;
use crate::pin::{accept_leaf, cert_fingerprint};
use crate::windows::util::{backend_error, io_error, WinResult};
use pf_driver_proto::seat_audio::LISTENER;
use std::path::Path;
use windows::Win32::Security::Cryptography::{
    CertCloseStore, CertEnumCertificatesInStore, CertOpenStore, CERT_CONTEXT,
    CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_LOCAL_MACHINE,
};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_BINARY};
use winreg::RegKey;

const PIN_FILE: &str = "rdp-cert-sha256";
/// Remote Desktop's own system store, which holds its self-signed certificate.
const RDP_STORE: &str = "Remote Desktop";
/// The machine's personal store, where a certificate an administrator gave a listener lives.
const MY_STORE: &str = "MY";
/// A listener's registry value that names its certificate by SHA-1 thumbprint.
const NAMED_CERTIFICATE: &str = "SSLCertificateSHA1Hash";

/// The console's sentence for a leaf that is no longer the recorded one.
const PIN_CHANGED: &str = "Remote Desktop answered with a certificate that isn't this host's, so \
    seats can't sign in. Close any other program that uses Remote Desktop's port, then try again.";

/// The pin a seat starts with: `pin`, unless TermService presents another leaf. A new leaf the
/// machine holds replaces `pin` in `root`; any other refuses with `rdp_pin_changed`. A leaf that
/// can't be read passes: the keeper's own sign-in says why.
pub(super) fn require_live(
    root: &SecretRoot,
    host_path: &Path,
    pin: [u8; 32],
) -> WinResult<[u8; 32]> {
    require_same(root, super::keeper::live_pin(host_path), pin, machine_pins)
}

/// [`require_live`] for the leaf `live`. `held` lists the machine's certificates, and runs only
/// when the leaf is new.
fn require_same(
    root: &SecretRoot,
    live: Option<[u8; 32]>,
    pin: [u8; 32],
    held: impl FnOnce() -> Vec<[u8; 32]>,
) -> WinResult<[u8; 32]> {
    let Some(live) = live.filter(|live| *live != pin) else {
        return Ok(pin);
    };
    if let Err(cause) = accept_leaf(live, &held()) {
        tracing::warn!(%cause, "new RDP leaf the machine does not hold");
        return Err(backend_error("rdp_pin_changed", PIN_CHANGED).said("rdp_pin_changed"));
    }
    store_pin(root, live, true)?;
    tracing::info!(
        from = %hex::encode(pin),
        to = %hex::encode(live),
        "RDP pin moved to the machine's new certificate"
    );
    Ok(live)
}

/// The fingerprint of each certificate the machine keeps for Remote Desktop: every one in
/// `LocalMachine\Remote Desktop`, and each one in `LocalMachine\My` that `RDP-Tcp` or the seat
/// listener names by thumbprint. Only administrators and SYSTEM write these stores.
pub fn machine_pins() -> Vec<[u8; 32]> {
    let named: Vec<[u8; 20]> = [STOCK, LISTENER]
        .into_iter()
        .filter_map(named_thumbprint)
        .collect();
    let personal = store_certificates(MY_STORE);
    let picked = personal.iter().filter(|der| {
        let sha1 = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, der);
        named.iter().any(|named| sha1.as_ref() == named)
    });
    store_certificates(RDP_STORE)
        .iter()
        .chain(picked)
        .map(|der| cert_fingerprint(der))
        .collect()
}

/// The SHA-1 thumbprint `listener`'s registry key names, when it holds one of 20 bytes.
fn named_thumbprint(listener: &str) -> Option<[u8; 20]> {
    let value = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            format!(r"{WINSTATIONS}\{listener}"),
            KEY_READ | KEY_WOW64_64KEY,
        )
        .ok()?
        .get_raw_value(NAMED_CERTIFICATE)
        .ok()?;
    (value.vtype == REG_BINARY)
        .then(|| <[u8; 20]>::try_from(&value.bytes[..]).ok())
        .flatten()
}

/// The DER of every certificate in the machine's system store `name`, opened read-only. Empty
/// when the store doesn't exist or doesn't open.
fn store_certificates(name: &str) -> Vec<Vec<u8>> {
    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let flags = CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE)
        | CERT_STORE_OPEN_EXISTING_FLAG
        | CERT_STORE_READONLY_FLAG;
    // SAFETY: the system provider reads `name_w`, a NUL-terminated UTF-16 store name that
    // outlives the call; it takes no encoding type and no crypto provider.
    let opened = unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            flags,
            Some(name_w.as_ptr().cast()),
        )
    };
    let store = match opened {
        Ok(store) => store,
        Err(error) => {
            tracing::debug!(store = name, %error, "certificate store did not open");
            return Vec::new();
        }
    };
    let mut certificates = Vec::new();
    let mut context: *const CERT_CONTEXT = std::ptr::null();
    loop {
        // SAFETY: `store` is open, and `context` is null or the context the previous call
        // returned, which this call frees.
        context = unsafe { CertEnumCertificatesInStore(store, Some(context)) };
        // SAFETY: a non-null context is a valid CERT_CONTEXT until the next call frees it.
        let Some(certificate) = (unsafe { context.as_ref() }) else {
            break;
        };
        // SAFETY: a live context's `pbCertEncoded` points at its `cbCertEncoded` encoded bytes,
        // which are copied out before the next call frees them.
        let der = unsafe {
            std::slice::from_raw_parts(
                certificate.pbCertEncoded,
                certificate.cbCertEncoded as usize,
            )
        };
        certificates.push(der.to_vec());
    }
    // SAFETY: `store` was opened above and is closed once. The enumeration ended on null, so it
    // left no context to free.
    let _ = unsafe { CertCloseStore(Some(store), 0) };
    certificates
}

/// A pin as the keeper prints it: 64 hexadecimal digits.
pub(super) fn parse_pin(text: &str) -> Option<[u8; 32]> {
    hex::decode(text.trim()).ok()?.try_into().ok()
}

/// Record `pin`, refusing to replace a different stored pin unless `replace` is set. With
/// `replace` the stored file is never read, so one that no longer parses is replaced too.
pub fn store_pin(root: &SecretRoot, pin: [u8; 32], replace: bool) -> WinResult<()> {
    if !replace
        && let Some(existing) = load_pin_optional(root)?
        && existing != pin
    {
        return Err(backend_error(
            "rdp_pin_changed",
            format!(
                "RDP leaf changed from {} to {}; rerun with --replace only after verification",
                hex::encode(existing),
                hex::encode(pin)
            ),
        ));
    }
    root.write_atomic(PIN_FILE, format!("{}\n", hex::encode(pin)).as_bytes())
        .map_err(|error| io_error("rdp_pin_store", "write RDP certificate pin", error))
}

pub(super) fn load_pin(root: &SecretRoot) -> WinResult<[u8; 32]> {
    load_pin_optional(root)?.ok_or_else(|| {
        backend_error(
            "rdp_pin_missing",
            "RDP certificate pin is missing; run 'punktfunk-seat-keeper trust' elevated",
        )
    })
}

pub(super) fn load_pin_optional(root: &SecretRoot) -> WinResult<Option<[u8; 32]>> {
    let Some(bytes) = root
        .read_current(PIN_FILE, 128)
        .map_err(|error| io_error("rdp_pin_store", "read RDP certificate pin", error))?
    else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| backend_error("rdp_pin_invalid", "RDP certificate pin is not UTF-8"))?
        .trim();
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(backend_error(
            "rdp_pin_invalid",
            "RDP certificate pin must be 64 lowercase hexadecimal characters",
        ));
    }
    let decoded = hex::decode(text)
        .map_err(|_| backend_error("rdp_pin_invalid", "RDP certificate pin is invalid"))?;
    let mut pin = [0_u8; 32];
    pin.copy_from_slice(&decoded);
    Ok(Some(pin))
}

#[cfg(test)]
mod tests {
    use super::{load_pin_optional, parse_pin, require_same, store_pin, PIN_FILE};
    use crate::persistence::SecretRoot;

    /// Turning seats on replaces a pin file that no longer parses; the plain `trust` refuses it.
    #[test]
    fn a_replace_overwrites_an_unreadable_pin() {
        let dir = tempfile::tempdir().unwrap();
        let root = SecretRoot::open(dir.path()).unwrap();
        root.write_atomic(PIN_FILE, b"\xEF\xBB\xBFNOT-A-PIN\n")
            .unwrap();
        assert_eq!(
            store_pin(&root, [7; 32], false).unwrap_err().code,
            "rdp_pin_invalid"
        );
        store_pin(&root, [7; 32], true).unwrap();
        assert_eq!(load_pin_optional(&root).unwrap(), Some([7; 32]));
    }

    /// A leaf that can't be read passes, and the keeper's own check covers it. A moved leaf the
    /// machine holds becomes the pin; one it doesn't hold refuses and leaves the pin alone.
    #[test]
    fn only_a_moved_leaf_the_machine_does_not_hold_refuses_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let root = SecretRoot::open(dir.path()).unwrap();
        store_pin(&root, [1; 32], true).unwrap();
        let held = || vec![[2; 32]];
        assert_eq!(
            require_same(&root, Some([1; 32]), [1; 32], held),
            Ok([1; 32])
        );
        assert_eq!(require_same(&root, None, [1; 32], held), Ok([1; 32]));
        let moved = require_same(&root, Some([3; 32]), [1; 32], held).unwrap_err();
        assert_eq!(moved.code, "rdp_pin_changed");
        assert_eq!(load_pin_optional(&root).unwrap(), Some([1; 32]));
        assert_eq!(
            require_same(&root, Some([2; 32]), [1; 32], held),
            Ok([2; 32])
        );
        assert_eq!(load_pin_optional(&root).unwrap(), Some([2; 32]));
        let none_held = require_same(&root, Some([3; 32]), [2; 32], Vec::new).unwrap_err();
        assert_eq!(none_held.code, "rdp_pin_changed");
    }

    #[test]
    fn the_keeper_prints_a_pin_as_64_hex_digits() {
        let printed = hex::encode([7_u8; 32]);
        assert_eq!(parse_pin(&format!("{printed}\r\n")), Some([7; 32]));
        assert_eq!(parse_pin(&printed[2..]), None);
        assert_eq!(parse_pin("not a pin"), None);
        assert_eq!(parse_pin(""), None);
    }
}
