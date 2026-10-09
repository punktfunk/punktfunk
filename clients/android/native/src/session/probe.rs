//! The network check over an established session.
//!
//! The host bursts filler over the real data plane — the same path a stream uses, so the answer is
//! about the link the stream will actually take rather than about some generic throughput. It is
//! *measure-only*: which layer a measured bitrate belongs in (the global default, or a host's bound
//! preset) is a decision the UI makes with the user (design/client-settings-profiles.md §5.3).

use super::SESSIONS;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JDoubleArray, JObject};
use jni::sys::jlong;
use jni::EnvUnowned;

/// `NativeBridge.nativeNetworkCheck(handle): DoubleArray?` — run the network check over this
/// session and return its report. Blocking for ten to twenty seconds: call it off the main
/// thread. `null` on a `0` handle or when the check could not run.
///
/// Layout: `[ceilingKbps, wall, hasClean, cleanRateKbps, cleanLossPct, cleanJitterUs,
/// clientIfaceKind, clientLinkMbps, clientRcvbufKb, hostIfaceKind, hostLinkMbps,
/// hostSndbufKb, nLegs, burstsLossPct, cappedLossPct, nFindings]` then five per finding:
/// `[id, severity, n0, n1, n2]`.
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
            f64::from(host.iface_kind),
            f64::from(host.link_mbps),
            f64::from(host.sndbuf_kb),
            r.legs.len() as f64,
            leg(LegShape::FrameBursts),
            leg(LegShape::Capped),
            r.findings.len() as f64,
        ];
        for f in &r.findings {
            values.extend([
                f64::from(f.id as u8),
                f64::from(f.severity as u8),
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
