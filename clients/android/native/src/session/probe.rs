//! The bandwidth speed test over an established session.
//!
//! The host bursts filler over the real data plane — the same path a stream uses, so the answer is
//! about the link the stream will actually take rather than about some generic throughput. It is
//! deliberately *measure-only*: which layer a measured bitrate belongs in (the global default, or a
//! host's bound preset) is a decision the UI makes with the user
//! (design/client-settings-profiles.md §5.3), never one this shim makes for them.
//!
//! Two calls: start, then poll. Both are cheap and non-blocking, so Kotlin can drive them from a
//! coroutine on the main thread the way it polls the stats HUD.

use super::{jni_guard, SESSIONS};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JDoubleArray, JObject};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;

/// The `DoubleArray` [`Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult`] returns. Kept in
/// one place because Kotlin indexes it positionally; see the Kotlin doc for the field order.
const PROBE_RESULT_LEN: usize = 9;

/// `NativeBridge.nativeSpeedTest(handle, targetKbps, durationMs): Boolean` — ask the host to burst
/// filler at `targetKbps` of goodput for `durationMs` (each clamped host-side to ≤ 10 Gbit/s /
/// ≤ 5 s) beside the video it is already sending. Non-blocking: poll
/// [`Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult`] until its `done` element is 1.
/// Starting a probe resets any prior measurement. `false` on a `0` handle or a closed session.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSpeedTest(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    target_kbps: jint,
    duration_ms: jint,
) -> jboolean {
    jni_guard(false, || {
        let Some(h) = SESSIONS.get(handle) else {
            return false;
        };
        let target = target_kbps.clamp(0, i32::MAX) as u32;
        let duration = duration_ms.clamp(0, i32::MAX) as u32;
        match h.client.request_probe(target, duration) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("speed test: probe request: {e:?}");
                false
            }
        }
    })
}

/// `NativeBridge.nativeNetworkCheck(handle): DoubleArray?` — run the network check over this
/// session and return its report. Blocking for ten to twenty seconds: call it off the main
/// thread. `null` on a `0` handle or when the check could not run.
///
/// Layout: `[ceilingKbps, wall, hasClean, cleanRateKbps, cleanLossPct, cleanJitterUs,
/// clientIfaceKind, clientLinkMbps, clientRcvbufKb, hasHost, hostIfaceKind, hostLinkMbps,
/// hostSndbufKb, nLegs, burstsLossPct, cappedLossPct, nFindings]` then six per finding:
/// `[id, severity, profile, n0, n1, n2]` (`profile` 0 = none, 1 capped, 2 smooth).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNetworkCheck<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JDoubleArray<'local> {
    use punktfunk_core::client::health::{self, LegShape};
    env.with_env(|env| -> jni::errors::Result<JDoubleArray<'local>> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(JDoubleArray::default());
        };
        let r = match health::health_check(&h.client, |_| {}) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("network check: {e:?}");
                return Ok(JDoubleArray::default());
            }
        };
        let clean = r.speed.clean;
        let host = r.host;
        let leg = |shape| {
            r.legs
                .iter()
                .find(|l| l.shape == shape)
                .map_or(0.0, |l| f64::from(l.outcome.loss_pct))
        };
        let mut values: Vec<f64> = vec![
            f64::from(r.speed.ceiling_kbps),
            f64::from(u8::from(r.speed.wall)),
            f64::from(u8::from(clean.is_some())),
            clean.map_or(0.0, |c| f64::from(c.rate_kbps)),
            clean.map_or(0.0, |c| f64::from(c.loss_pct)),
            clean.map_or(0.0, |c| f64::from(c.jitter_us)),
            f64::from(r.client.link.kind),
            f64::from(r.client.link.mbps),
            f64::from(r.client.rcvbuf_kb),
            f64::from(u8::from(host.is_some())),
            host.map_or(0.0, |h| f64::from(h.iface_kind)),
            host.map_or(0.0, |h| f64::from(h.link_mbps)),
            host.map_or(0.0, |h| f64::from(h.sndbuf_kb)),
            r.legs.len() as f64,
            leg(LegShape::FrameBursts),
            leg(LegShape::Capped),
            r.findings.len() as f64,
        ];
        for f in &r.findings {
            values.extend([
                f64::from(f.id as u8),
                f64::from(f.severity as u8),
                f64::from(f.profile.unwrap_or(0)),
                f64::from(f.numbers[0]),
                f64::from(f.numbers[1]),
                f64::from(f.numbers[2]),
            ]);
        }
        let arr = env.new_double_array(values.len())?;
        arr.set_region(env, 0, &values)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeProbeResult(handle): DoubleArray?` — the current measurement, partial until
/// `[0] == 1`. Safe to poll; before any probe it reports zeros. `null` on a `0` handle.
///
/// Layout (doubles so one array carries both the counts and the percentages):
/// `[done, throughputKbps, lossPct, hostDropPct, elapsedMs, recvBytes, gapP50Us, gapP99Us,
/// reorders]`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JDoubleArray<'local> {
    // `JDoubleArray::default()` is the null reference the old `JObject::null().into_raw()` returned,
    // so Kotlin still reads `null` on every failure path.
    env.with_env(|env| -> jni::errors::Result<JDoubleArray<'local>> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(JDoubleArray::default());
        };
        let r = h.client.probe_result();
        let values: [f64; PROBE_RESULT_LEN] = [
            f64::from(u8::from(r.done)),
            f64::from(r.throughput_kbps),
            f64::from(r.loss_pct),
            f64::from(r.host_drop_pct),
            f64::from(r.elapsed_ms),
            r.recv_bytes as f64,
            f64::from(r.gap_p50_us),
            f64::from(r.gap_p99_us),
            f64::from(r.reorders),
        ];
        let arr = env.new_double_array(PROBE_RESULT_LEN)?;
        arr.set_region(env, 0, &values)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}
