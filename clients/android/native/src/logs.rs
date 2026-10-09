//! JNI seam for "Send logs to host": hand Kotlin the client's recent log ring (fed by the
//! [`crate::RingTee`] logcat tee) rendered as one text bundle. The UPLOAD stays on the
//! Kotlin side — its mTLS OkHttp client (`mtlsHttpClient`, the library/art path) already
//! owns HTTPS-to-the-pinned-host on this platform, and `logring::send_to_host`'s ureq
//! agent is deliberately desktop-only. Android-gated (unlike [`crate::wol`]/[`crate::probe`])
//! with the logcat tee that fills the ring.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::jint;
use jni::EnvUnowned;

/// `NativeBridge.nativeRenderLogs(header): String` — the ring as one text bundle, oldest
/// first, prefixed by `header` (the Kotlin side's identity line) and an eviction note when
/// the ring wrapped. Never empty (the header line is always present); cheap enough for any
/// thread, though the caller is about to do network anyway.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRenderLogs<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    header: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| {
        let header: String = header.try_to_string(env)?;
        env.new_string(pf_client_core::logring::render(&header))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogWifiLink(reason, rssiDbm, txMbps, rxMbps, freqMhz, standard)` — one
/// `pf.wifi` line in the ring. Kotlin reads the link and decides when (`WifiLinkLog`); its own
/// `Log` reaches logcat but never a "Send logs" bundle. `-1` is unknown; `freqMhz` ≤ 0 is off Wi-Fi.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogWifiLink<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    reason: JString<'local>,
    rssi_dbm: jint,
    tx_mbps: jint,
    rx_mbps: jint,
    freq_mhz: jint,
    standard: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let reason = reason.try_to_string(env)?;
        log::info!(
            target: "pf.wifi",
            "link reason={reason} rssiDbm={rssi_dbm} txMbps={tx_mbps} rxMbps={rx_mbps} \
             freqMhz={freq_mhz} standard={}",
            wifi_standard(standard)
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogDisplay(line)` — one `pf.display` line in the ring: the displays and
/// fold features a stream sees, so a bundle says which dual-screen shape a device reports.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogDisplay<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    line: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let line = line.try_to_string(env)?;
        log::info!(target: "pf.display", "{line}");
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `WifiInfo.getWifiStandard()` by its IEEE name (`ScanResult.WIFI_STANDARD_*`).
fn wifi_standard(code: jint) -> &'static str {
    match code {
        1 => "legacy",
        4 => "11n",
        5 => "11ac",
        6 => "11ax",
        7 => "11ad",
        8 => "11be",
        _ => "?",
    }
}
