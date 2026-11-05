//! Windows Hello as this device's profile unlock keys ([`DeviceKeys`]): one RSA 2048 key per
//! profile under `KeyCredentialManager`, made and used behind Hello's own prompt, which sits on
//! the shell's window where Windows can parent it. The private key never leaves Hello.
//!
//! Hello signs with RSA PKCS#1 v1.5 over SHA-256 of the data, and its `Pkcs1RsaPublicKey`
//! blob is the DER `RSAPublicKey` the host verifies with.

use pf_client_core::unlock::DeviceKeys;
use windows::core::HSTRING;
use windows::Security::Credentials::{
    KeyCredential, KeyCredentialCreationOption, KeyCredentialManager, KeyCredentialStatus,
};
use windows::Security::Cryptography::Core::CryptographicPublicKeyBlobType;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::Streams::IBuffer;

/// The Hello key store. Every call blocks on Hello: run them off the UI thread.
pub(crate) struct Hello;

/// Hello is set up for this Windows account, so the prompt offers it.
pub(crate) fn supported() -> bool {
    KeyCredentialManager::IsSupportedAsync()
        .and_then(|op| op.join())
        .unwrap_or(false)
}

/// The key named `name`, when Hello holds one for this account and app.
fn open(name: &str) -> Option<KeyCredential> {
    let found = KeyCredentialManager::OpenAsync(&HSTRING::from(name))
        .ok()?
        .join()
        .ok()?;
    (found.Status().ok()? == KeyCredentialStatus::Success)
        .then(|| found.Credential().ok())
        .flatten()
}

fn bytes(buffer: &IBuffer) -> Option<Vec<u8>> {
    let mut out = windows::core::Array::<u8>::new();
    CryptographicBuffer::CopyToByteArray(buffer, &mut out).ok()?;
    Some(out.to_vec())
}

/// The shell's window, for Hello's prompt. A top-level window's id is its handle.
fn window() -> Option<windows::UI::WindowId> {
    crate::shell_window::raw().map(|h| windows::UI::WindowId { Value: h as u64 })
}

impl DeviceKeys for Hello {
    fn holds(&self, name: &str) -> bool {
        open(name).is_some()
    }

    fn create(&self, name: &str) -> Option<Vec<u8>> {
        let (name, option) = (
            HSTRING::from(name),
            KeyCredentialCreationOption::ReplaceExisting,
        );
        // The window-parented call needs a recent Windows; an older one takes the plain call.
        let made = window()
            .and_then(|w| {
                let op = KeyCredentialManager::RequestCreateForWindowAsync(w, &name, option);
                op.ok()?.join().ok()
            })
            .or_else(|| {
                let op = KeyCredentialManager::RequestCreateAsync(&name, option);
                op.ok()?.join().ok()
            })?;
        let status = made.Status().ok()?;
        if status != KeyCredentialStatus::Success {
            tracing::info!(status = status.0, "windows hello key not created");
            return None;
        }
        let key = made.Credential().ok()?;
        bytes(
            &key.RetrievePublicKeyWithBlobType(CryptographicPublicKeyBlobType::Pkcs1RsaPublicKey)
                .ok()?,
        )
    }

    fn sign(&self, name: &str, message: &[u8]) -> Option<Vec<u8>> {
        let key = open(name)?;
        let data = CryptographicBuffer::CreateFromByteArray(message).ok()?;
        let signed = window()
            .and_then(|w| key.RequestSignForWindowAsync(w, &data).ok()?.join().ok())
            .or_else(|| key.RequestSignAsync(&data).ok()?.join().ok())?;
        let status = signed.Status().ok()?;
        if status != KeyCredentialStatus::Success {
            tracing::info!(status = status.0, "windows hello did not sign");
            return None;
        }
        bytes(&signed.Result().ok()?)
    }

    fn delete(&self, name: &str) {
        let gone = KeyCredentialManager::DeleteAsync(&HSTRING::from(name)).and_then(|op| op.join());
        if let Err(e) = gone {
            tracing::info!(error = %e, "windows hello key not deleted");
        }
    }
}
