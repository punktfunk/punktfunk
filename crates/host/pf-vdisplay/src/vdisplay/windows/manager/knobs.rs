//! Runtime display-management knobs: linger and per-monitor topology action.
//! Readers of [`crate::policy`] plus legacy env fallbacks — no manager state.

use crate::policy::{DisplayPolicy, Linger};
use std::time::Duration;

/// 10 s: an unconfigured host's linger when `PUNKTFUNK_MONITOR_LINGER_MS` is unset.
const DEFAULT_LINGER_MS: u64 = 10_000;

/// Linger for a monitor `client_fp` created: the console's `keep_alive` with that
/// device's overlay applied (§6.1), so the TV can keep its screen forever while the
/// tablet's goes at once.
pub(super) fn linger_for(client_fp: Option<[u8; 32]>) -> Linger {
    resolve_linger(
        crate::policy::prefs().configured().as_ref(),
        crate::policy::fp_hex(client_fp).as_deref(),
        std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
            .ok()
            .and_then(|s| s.parse().ok()),
    )
}

/// Console policy outranks the env knob: an operator who set the console
/// must not have it silently overridden by a leftover
/// `PUNKTFUNK_MONITOR_LINGER_MS`. Unconfigured: the env knob, else 10 s.
/// Unparseable env arrives as `None` (`parse().ok()`), i.e. unset, not zero.
fn resolve_linger(
    configured: Option<&DisplayPolicy>,
    fp: Option<&str>,
    env_ms: Option<u64>,
) -> Linger {
    match configured {
        Some(p) => p.effective_for(fp).keep_alive.linger(),
        None => Linger::For(Duration::from_millis(env_ms.unwrap_or(DEFAULT_LINGER_MS))),
    }
}

/// Exclusive-topology re-assert cadence. Default 2000 ms; `0` disables.
/// A verified isolate is not durable — see
/// `VirtualDisplayManager::ensure_exclusive_watch`.
pub(super) fn exclusive_reassert_ms() -> u64 {
    std::env::var("PUNKTFUNK_EXCLUSIVE_REASSERT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000)
}

/// Topology for a freshly-created monitor (never `Auto`): console
/// [`effective_topology`](crate::effective_topology) when configured, else
/// `PUNKTFUNK_NO_ISOLATE` on (`env_on`, so `=0` is off) → `Extend`, otherwise `Exclusive`.
pub(super) fn topology_action(client: Option<[u8; 32]>) -> crate::policy::Topology {
    let configured = crate::policy::prefs()
        .configured_effective()
        .map(|_| crate::effective_topology(client));
    resolve_topology_action(
        configured,
        pf_host_config::env_on("PUNKTFUNK_NO_ISOLATE") == Some(true),
    )
}

/// Unconfigured host: `PUNKTFUNK_NO_ISOLATE` → `Extend`, else `Exclusive`.
/// A configured answer is passed through; the env knob does not override it.
fn resolve_topology_action(
    configured: Option<crate::policy::Topology>,
    no_isolate_env: bool,
) -> crate::policy::Topology {
    use crate::policy::Topology;
    match configured {
        Some(t) => t,
        None if no_isolate_env => Topology::Extend,
        None => Topology::Exclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_linger, resolve_topology_action, DEFAULT_LINGER_MS};
    use crate::policy::{ClientOverlay, DisplayPolicy, KeepAlive, Linger, Topology};
    use std::time::Duration;

    /// The device overlay decides its own linger; everyone else follows the host,
    /// and a configured host outranks the legacy env knob.
    #[test]
    fn a_device_overlay_decides_its_own_linger() {
        let mut p = DisplayPolicy {
            keep_alive: KeepAlive::Off,
            ..DisplayPolicy::default()
        };
        p.clients.insert(
            "aa11".into(),
            ClientOverlay {
                keep_alive: Some(KeepAlive::Forever),
                ..ClientOverlay::default()
            },
        );
        assert_eq!(
            resolve_linger(Some(&p), Some("aa11"), None),
            Linger::Forever
        );
        assert_eq!(
            resolve_linger(Some(&p), Some("bb22"), Some(60_000)),
            Linger::Immediate
        );
        assert_eq!(resolve_linger(Some(&p), None, None), Linger::Immediate);
    }

    /// Unparseable env reaches here as `None` (`parse().ok()`), so it reads as
    /// unset, not zero. A zero linger would tear the monitor down on every
    /// disconnect.
    #[test]
    fn an_unconfigured_host_honours_the_env_knob_then_the_default() {
        assert_eq!(
            resolve_linger(None, Some("aa11"), Some(250)),
            Linger::For(Duration::from_millis(250))
        );
        assert_eq!(
            resolve_linger(None, None, None),
            Linger::For(Duration::from_millis(DEFAULT_LINGER_MS))
        );
    }

    /// Unconfigured rungs are `Exclusive` by default, `Extend` under the legacy
    /// opt-out — never `Auto`, which the manager's `match` would treat as extend
    /// without saying so.
    #[test]
    fn the_unconfigured_topology_rungs_never_yield_auto() {
        assert_eq!(resolve_topology_action(None, false), Topology::Exclusive);
        assert_eq!(resolve_topology_action(None, true), Topology::Extend);
        assert_eq!(
            resolve_topology_action(Some(Topology::Primary), true),
            Topology::Primary
        );
    }
}
