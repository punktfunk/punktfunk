//! LocalSystem-user DPAPI storage for managed seat passwords.
//!
//! Each blob is `credential-<SeatId>.bin` below `SecretRoot`. DPAPI uses the
//! calling LocalSystem profile, `CRYPTPROTECT_UI_FORBIDDEN`, and the exact seat
//! ID as optional entropy; machine scope is never requested. Plaintext lives in
//! `Zeroizing` values, and DPAPI output memory is wiped before it is released.
//! Passwords have no serialization implementation and their `Debug` output is
//! always redacted. Callers can only load, store, test, or delete one seat blob.

use crate::model::SeatId;
use crate::persistence::SecretRoot;
use crate::windows::util::{backend_error, io_error, require_local_system, WinResult};
use std::fmt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use zeroize::{Zeroize as _, Zeroizing};

const MAX_CREDENTIAL_BLOB: usize = 16 * 1024;

pub(super) struct Credential(Zeroizing<String>);

impl Credential {
    pub(super) fn new(value: String) -> WinResult<Self> {
        if !(32..=256).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(backend_error(
                "credential_invalid",
                "managed seat password must be 32..=256 visible ASCII bytes",
            ));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub(super) fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub(super) fn copy_zeroizing(&self) -> Zeroizing<String> {
        Zeroizing::new(self.0.to_string())
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

#[derive(Clone)]
pub(super) struct CredentialStore {
    root: SecretRoot,
}

impl CredentialStore {
    pub(super) fn new(root: SecretRoot) -> Self {
        Self { root }
    }

    pub(super) fn exists(&self, id: &SeatId) -> WinResult<bool> {
        self.root
            .read_current(&file_name(id), MAX_CREDENTIAL_BLOB)
            .map(|value| value.is_some())
            .map_err(|error| io_error("credential_store", "read credential blob", error))
    }

    pub(super) fn store(&self, id: &SeatId, credential: &Credential) -> WinResult<()> {
        require_local_system()?;
        let entropy_bytes = id.as_str().as_bytes();
        let input = blob(credential.expose().as_bytes())?;
        let entropy = blob(entropy_bytes)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: both input slices remain live; `output` is an out-parameter freed below.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| io_error("credential_protect", "CryptProtectData failed", error))?;
        let protected = copy_dpapi_output(&output, false)?;
        self.root
            .write_atomic(&file_name(id), &protected)
            .map_err(|error| io_error("credential_store", "write credential blob", error))
    }

    pub(super) fn load(&self, id: &SeatId) -> WinResult<Credential> {
        require_local_system()?;
        let protected = self
            .root
            .read_current(&file_name(id), MAX_CREDENTIAL_BLOB)
            .map_err(|error| io_error("credential_store", "read credential blob", error))?
            .ok_or_else(|| {
                backend_error(
                    "credential_missing",
                    format!("credential blob for seat {id} is missing"),
                )
            })?;
        let entropy_bytes = id.as_str().as_bytes();
        let input = blob(&protected)?;
        let entropy = blob(entropy_bytes)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input and entropy remain live; DPAPI allocates `output`, which is wiped and freed.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| io_error("credential_unprotect", "CryptUnprotectData failed", error))?;
        let plaintext = copy_dpapi_output(&output, true)?;
        let text = String::from_utf8(plaintext.to_vec()).map_err(|_| {
            backend_error(
                "credential_invalid",
                "unprotected seat credential is not UTF-8",
            )
        })?;
        Credential::new(text)
    }

    pub(super) fn delete(&self, id: &SeatId) -> WinResult<()> {
        self.root
            .remove_file(&file_name(id))
            .map_err(|error| io_error("credential_store", "remove credential blob", error))
    }
}

fn file_name(id: &SeatId) -> String {
    format!("credential-{id}.bin")
}

fn blob(bytes: &[u8]) -> WinResult<CRYPT_INTEGER_BLOB> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| backend_error("credential_invalid", "DPAPI input is too large"))?;
    Ok(CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

fn copy_dpapi_output(output: &CRYPT_INTEGER_BLOB, wipe: bool) -> WinResult<Zeroizing<Vec<u8>>> {
    let length = output.cbData as usize;
    if output.pbData.is_null() || length == 0 || length > MAX_CREDENTIAL_BLOB {
        if !output.pbData.is_null() {
            if wipe && length != 0 {
                // SAFETY: DPAPI reports this allocation as writable for exactly `cbData` bytes.
                unsafe { std::slice::from_raw_parts_mut(output.pbData, length) }.zeroize();
            }
            // SAFETY: DPAPI returned this allocation; LocalFree consumes it exactly once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
            }
        }
        return Err(backend_error(
            "credential_invalid",
            "DPAPI returned an invalid output buffer",
        ));
    }
    // SAFETY: successful DPAPI output is readable and writable for `cbData` bytes until LocalFree.
    let source = unsafe { std::slice::from_raw_parts_mut(output.pbData, length) };
    let copy = Zeroizing::new(source.to_vec());
    if wipe {
        source.zeroize();
    }
    // SAFETY: `output.pbData` is the DPAPI LocalAlloc result and has not been freed yet.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    Ok(copy)
}
