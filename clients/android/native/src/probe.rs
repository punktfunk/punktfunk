//! JNI seam for the reachability probe: a bounded QUIC handshake to a saved host
//! (`punktfunk_core::client::NativeClient::probe_identity`), reporting WHICH certificate
//! answered. Like [`crate::wol`] it takes no session handle and links into the host workspace
//! build (pure `jni` + `punktfunk_core`). Kotlin calls it periodically, off the main thread, to
//! light the "online" pip for saved hosts, including ones that never advertise on mDNS (reached
//! over Tailscale / VPN / another subnet).

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jint, jstring};
use jni::EnvUnowned;
use punktfunk_core::client::NativeClient;
use std::time::Duration;

/// `NativeBridge.nativeProbe(host, port, timeoutMs): String?` — the lowercase-hex SHA-256 of
/// the certificate `host:port` presented within `timeoutMs`, or null if nothing answered.
/// mDNS-independent; the handshake is unpinned, so the answer names whoever holds the address
/// and the caller compares it against the record's pin.
/// Blocking (builds its own runtime) — Kotlin runs it off the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeProbe<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    timeout_ms: jint,
) -> jstring {
    env.with_env(|env| -> jni::errors::Result<jstring> {
        let host: String = host.try_to_string(env)?;
        let port = port.clamp(0, u16::MAX as jint) as u16;
        let timeout = Duration::from_millis(timeout_ms.max(0) as u64);
        match NativeClient::probe_identity(&host, port, timeout) {
            Some(fp) => Ok(env.new_string(punktfunk_core::fp::hex(&fp))?.into_raw()),
            None => Ok(std::ptr::null_mut()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}
