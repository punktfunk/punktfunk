//! Runtime display-management knobs: linger and per-monitor topology action.
//! Readers of [`crate::policy`] plus legacy env fallbacks — no manager state.

use crate::policy::Linger;

/// Linger for a monitor `client_fp` created.
pub(super) fn linger_for(client_fp: Option<[u8; 32]>) -> Linger {
    crate::policy::prefs().linger(crate::policy::fp_hex(client_fp).as_deref())
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

/// Topology for a freshly-created monitor (never `Auto`): the session's `carried` one, else
/// console [`effective_topology`](crate::effective_topology) when configured, else
/// `PUNKTFUNK_NO_ISOLATE` on (`env_on`, so `=0` is off) → `Extend`, otherwise `Exclusive`.
pub(super) fn topology_action(
    carried: Option<crate::policy::Topology>,
    client: Option<[u8; 32]>,
) -> crate::policy::Topology {
    if let Some(t) = carried {
        return t;
    }
    let configured = crate::policy::prefs()
        .configured_effective()
        .map(|_| crate::effective_topology(client));
    resolve_topology_action(configured, no_isolate())
}

/// What an unconfigured host applies. [`crate::effective_topology`] answers with it, so a
/// readout names the topology the manager uses.
pub(crate) fn unconfigured_topology() -> crate::policy::Topology {
    resolve_topology_action(None, no_isolate())
}

fn no_isolate() -> bool {
    pf_host_config::env_on("PUNKTFUNK_NO_ISOLATE") == Some(true)
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
    use super::resolve_topology_action;
    use crate::policy::Topology;

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
