//! LAN host discovery: `pf_client_core::discovery`'s browse, folded for Kotlin.
//!
//! Kotlin holds the Wi-Fi multicast lock and owns permission UX. Rust owns the browse; each poll
//! folds the events since the last one into a map keyed by host id, newest advert winning, and
//! returns a newline-delimited snapshot.
//!
//! Start returns an opaque integer key into an `Arc<Discovery>` table. Poll retains the browse while
//! it reads, rescan re-queries on the live daemon, stop removes the key, and final drop closes the
//! event channel, after which the shared worker shuts the daemon down within one 250 ms tick. This
//! makes stop-vs-poll races safe without JVM callbacks or Rust pointers crossing JNI.

use crate::session::{jni_guard, lock_recover, HandleTable};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::jlong;
use jni::EnvUnowned;
use pf_client_core::discovery::{self, Adverts, DiscoveredHost, DiscoveryEvent, Rescan};
use std::sync::Mutex;

/// Field separator inside one serialized record (ASCII Unit Separator — never in a field value).
const FIELD_SEP: char = '\u{1f}';

/// One host for Kotlin: `key␟name␟addr␟port␟fp␟pair␟mac␟os␟mgmt` (`␟` = [`FIELD_SEP`]). `mac`
/// is comma-joined; `mgmt` is `0` when the host advertised none, and Kotlin then uses 47990.
/// Records are newline-joined in a poll snapshot. New fields append (the Kotlin parser tolerates
/// both arities), never reorder.
///
/// mDNS labels and TXT values are arbitrary UTF-8 from an unauthenticated source, so every text
/// field loses the framing bytes: a smuggled `\n` or U+001F would inject or suppress picker rows.
/// Trust is still gated on connect; this only protects the list.
fn encode(h: &DiscoveredHost) -> String {
    fn clean(s: &str) -> String {
        s.replace(['\n', '\r', FIELD_SEP], "")
    }
    format!(
        "{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}{FIELD_SEP}{}",
        clean(&h.key),
        clean(&h.name),
        clean(&h.addr),
        h.port,
        clean(&h.fp_hex),
        clean(&h.pair),
        clean(&h.mac.join(",")),
        clean(&h.os),
        h.mgmt_port.unwrap_or(0),
    )
}

/// One table-owned browse: the shared worker's events, its rescan flag, and the hosts folded so far.
struct Discovery {
    events: async_channel::Receiver<DiscoveryEvent>,
    rescan: Rescan,
    hosts: Mutex<Adverts>,
}

impl Discovery {
    fn start() -> Option<Discovery> {
        let (events, rescan) = discovery::try_browse()?;
        log::info!("native mDNS discovery started");
        Some(Discovery {
            events,
            rescan,
            hosts: Mutex::default(),
        })
    }

    /// Fold the events since the last poll, then return the host set newline-joined (empty string
    /// = none), in key order so it is stable across polls. Kotlin re-sorts by display name.
    fn snapshot(&self) -> String {
        let mut hosts = lock_recover(&self.hosts);
        while let Ok(event) = self.events.try_recv() {
            discovery::fold(&mut hosts, event);
        }
        hosts.values().map(encode).collect::<Vec<_>>().join("\n")
    }

    /// Re-browse on the live daemon: a PTR back on the wire and the backoff reset. Rebuilding
    /// re-binds :5353 and re-joins the groups, and a daemon that loses that race leaves the client
    /// with no discovery at all.
    fn rescan(&self) {
        self.rescan.request();
    }
}

static DISCOVERIES: HandleTable<Discovery> = HandleTable::new(0x3000_0000_0000_0001);

/// Start `_punktfunk._udp` browsing and return an opaque table key.
/// Kotlin holds its Wi-Fi multicast lock until stop; `0` reports daemon setup failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryStart(
    _env: EnvUnowned,
    _this: JObject,
) -> jlong {
    jni_guard(0, || match Discovery::start() {
        Some(discovery) => DISCOVERIES.insert(discovery),
        None => 0,
    })
}

/// Return the current newline-delimited host snapshot for one retained browse.
/// Missing or concurrently stopped keys produce an empty string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryPoll<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    // `with_env` subsumes the `jni_guard` this used to carry: it catches panics at the boundary and
    // `LogErrorAndDefault` logs then yields `JString::default()` — the null reference the old
    // `std::ptr::null_mut()` default returned. Kotlin still sees a null String on failure.
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = DISCOVERIES
            .get(handle)
            .map(|discovery| discovery.snapshot())
            .unwrap_or_default();
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

/// Re-query on a live browse. A missing or concurrently stopped key is a no-op.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryRescan(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(discovery) = DISCOVERIES.get(handle) {
            discovery.rescan();
        }
    })
}

/// Remove one browse key; final drop closes its event channel and the shared worker then ends.
/// Zero, stale, duplicate, and concurrent stops are no-ops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDiscoveryStop(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || drop(DISCOVERIES.remove(handle)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> DiscoveredHost {
        DiscoveredHost {
            key: "host-123".into(),
            fullname: "home-worker-2._punktfunk._udp.local.".into(),
            name: "home-worker-2".into(),
            addr: "192.168.1.70".into(),
            port: 9777,
            fp_hex: "ab".repeat(32),
            pair: "required".into(),
            mgmt_port: Some(47991),
            mac: vec!["aa:bb:cc:dd:ee:ff".into(), "11:22:33:44:55:66".into()],
            os: "linux/fedora/bazzite".into(),
            wire: Vec::new(),
        }
    }

    #[test]
    fn encode_round_trips_all_fields_with_unit_separator() {
        let encoded = encode(&host());
        let fields: Vec<&str> = encoded.split(FIELD_SEP).collect();
        assert_eq!(fields.len(), 9);
        assert_eq!(fields[0], "host-123");
        assert_eq!(fields[1], "home-worker-2");
        assert_eq!(fields[2], "192.168.1.70");
        assert_eq!(fields[3], "9777");
        assert_eq!(fields[4], "ab".repeat(32));
        assert_eq!(fields[5], "required");
        assert_eq!(fields[6], "aa:bb:cc:dd:ee:ff,11:22:33:44:55:66");
        assert_eq!(fields[7], "linux/fedora/bazzite");
        // A NON-default port on purpose: the whole point of carrying this field is the host that
        // moved off 47990, so a test pinned to the default would pass against a hardcoded value.
        assert_eq!(fields[8], "47991");
        assert!(
            !encoded.contains('\n'),
            "a record must never contain the record separator"
        );

        let older = DiscoveredHost {
            mgmt_port: None,
            ..host()
        };
        assert_eq!(encode(&older).split(FIELD_SEP).nth(8), Some("0"));
    }

    #[test]
    fn encode_strips_injected_separators_from_a_hostile_advert() {
        // A rogue advert could carry framing bytes in its instance label / TXT; encode must strip
        // them so the snapshot stays exactly one record of exactly nine fields.
        let h = DiscoveredHost {
            key: "k\u{1f}injected".into(),
            name: "evil\nhost\r".into(),
            addr: "10.0.0.5".into(),
            fp_hex: "ab\u{1f}cd".into(),
            pair: "required\n".into(),
            mac: vec!["aa:bb\u{1f}cc".into()],
            os: "linux\u{1f}evil/arch".into(),
            ..host()
        };
        let encoded = encode(&h);
        assert_eq!(encoded.matches(FIELD_SEP).count(), 8, "exactly nine fields");
        assert!(!encoded.contains('\n') && !encoded.contains('\r'));
        let fields: Vec<&str> = encoded.split(FIELD_SEP).collect();
        assert_eq!(fields[0], "kinjected");
        assert_eq!(fields[1], "evilhost");
        assert_eq!(fields[4], "abcd");
        assert_eq!(fields[5], "required");
        assert_eq!(fields[6], "aa:bbcc");
        assert_eq!(fields[7], "linuxevil/arch");
    }
}
