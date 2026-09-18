//! PnP monitor-devnode disable. Two selectors, both on by default:
//! [`disable_connected_inactive`] (standby sinks; `pf_vdisplay::policy::standby_sink_neutralise`)
//! and [`disable_for_deactivated`] (the displays the isolate switched off; the
//! `pnp_disable_monitors` policy axis). `PUNKTFUNK_STANDBY_SINK_KEEP` vetoes both.
//!
//! An `Exclusive` isolate removes physical monitors from the CCD topology, but their PnP nodes
//! stay live, so a standby sink that wakes the link still drives PnP arrival/removal, CCD
//! re-evaluation, and DWM invalidation. This module disables those nodes for the stream
//! (`CM_Disable_DevNode` + `CM_DISABLE_PERSIST`, so a hot-plug re-arrival stays disabled) and
//! re-enables them at teardown before the CCD restore. Selectors are allowlists: third-party
//! virtual displays are never touched.
//!
//! Every disable is a [`Lease`] (immunity plan WP11), journaled to
//! `<config>/pnp-disabled-monitors.json` BEFORE the mutation with the node's prior state, the
//! selector that picked it and the topology generation that owns it; a lease is dropped only after
//! its node re-enabled. [`startup_recover`] replays leftovers on host start. If the host dies and
//! never restarts, the monitor stays disabled until Device Manager.

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Disable_DevNode, CM_Enable_DevNode, CM_Get_DevNode_Status, CM_Locate_DevNodeW,
    CM_DISABLE_PERSIST, CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CM_PROB_DISABLED,
    CR_SUCCESS, DN_HAS_PROBLEM,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME,
};
use windows::Win32::Foundation::LUID;

/// Which selector leased a devnode: `BaselineInactive` (a sink that was dark before this acquire)
/// or `DeactivatedByUs` (a display the isolate itself switched off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseSource {
    BaselineInactive,
    DeactivatedByUs,
}

impl LeaseSource {
    fn tag(self) -> &'static str {
        match self {
            Self::BaselineInactive => "baseline_inactive",
            Self::DeactivatedByUs => "deactivated_by_us",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "baseline_inactive" => Some(Self::BaselineInactive),
            "deactivated_by_us" => Some(Self::DeactivatedByUs),
            _ => None,
        }
    }
}

/// One devnode disabled for a stream. Written before the mutation, dropped after the re-enable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub id: String,
    /// The node was enabled before us. Only such nodes are leased at all — a node the operator
    /// disabled is never touched, so the exact prior state is restored by construction.
    pub was_enabled: bool,
    pub source: LeaseSource,
    /// Topology generation of the acquire transaction that owns the lease.
    pub generation: u64,
}

fn journal_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("pnp-disabled-monitors.json")
}

/// Decode a journal. Rows are lease objects; a bare string is a pre-lease id (an older host's
/// file) and reads as a lease with unknown provenance, so it is still recovered.
fn decode_journal(bytes: &[u8]) -> Vec<Lease> {
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_slice::<serde_json::Value>(bytes)
    else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| match row {
            serde_json::Value::String(id) => Some(Lease {
                id: id.clone(),
                was_enabled: true,
                source: LeaseSource::DeactivatedByUs,
                generation: 0,
            }),
            serde_json::Value::Object(o) => Some(Lease {
                id: o.get("id")?.as_str()?.to_owned(),
                was_enabled: o
                    .get("was_enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                source: o
                    .get("source")
                    .and_then(|v| v.as_str())
                    .and_then(LeaseSource::from_tag)
                    .unwrap_or(LeaseSource::DeactivatedByUs),
                generation: o.get("generation").and_then(|v| v.as_u64()).unwrap_or(0),
            }),
            _ => None,
        })
        .collect()
}

fn encode_journal(leases: &[Lease]) -> Vec<u8> {
    let rows: Vec<serde_json::Value> = leases
        .iter()
        .map(|l| {
            serde_json::json!({
                "id": l.id,
                "was_enabled": l.was_enabled,
                "source": l.source.tag(),
                "generation": l.generation,
            })
        })
        .collect();
    serde_json::to_vec_pretty(&rows).unwrap_or_default()
}

fn read_journal() -> Vec<Lease> {
    match std::fs::read(journal_path()) {
        Ok(bytes) => decode_journal(&bytes),
        Err(_) => Vec::new(),
    }
}

/// The outstanding leases (devnodes disabled and not yet re-enabled), for the operator surface.
pub fn leases() -> Vec<Lease> {
    read_journal()
}

/// Persist `leases` as the outstanding set (union is the caller's job). Failure is logged, not
/// fatal — the feature degrades to "no crash journal", not "no feature".
fn write_journal(leases: &[Lease]) {
    let path = journal_path();
    if leases.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = pf_paths::create_private_dir(dir);
    }
    if let Err(e) = std::fs::write(&path, encode_journal(leases)) {
        tracing::warn!(error = %e, "PnP-disable: crash-recovery journal not written");
    }
}

// `display_events` applies the same transform to DBT_DEVICEARRIVAL interface paths.
pub fn instance_id_from_interface_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let cut = rest.rfind("#{")?;
    Some(rest[..cut].replace('#', "\\"))
}

fn utf16z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn monitor_instance(adapter: LUID, target_id: u32) -> Option<(String, String)> {
    let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    req.header.adapterId = adapter;
    req.header.id = target_id;
    // SAFETY: `req` is a properly-sized DISPLAYCONFIG_TARGET_DEVICE_NAME local whose header
    // (type/size/adapterId/id) is fully initialised; the API writes only within the struct.
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut req.header) };
    if rc != 0 {
        return None;
    }
    let id = instance_id_from_interface_path(&utf16z(&req.monitorDevicePath))?;
    Some((id, utf16z(&req.monitorFriendlyDeviceName)))
}

/// Whether the devnode is currently enabled: `None` when it cannot be located (departed), else
/// `false` only for a node whose problem code is "disabled" — the operator's own Device Manager
/// state, which we must not lease.
fn devnode_enabled(id: &str) -> Option<bool> {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    let mut devinst = 0u32;
    // SAFETY: `wide` is a live NUL-terminated UTF-16 instance id outliving the call; `devinst` is
    // a valid out-param.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let mut status = Default::default();
    let mut problem = Default::default();
    // SAFETY: `devinst` is the devnode the locate above resolved; both out-params are live locals.
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS {
        return None;
    }
    Some(!((status.0 & DN_HAS_PROBLEM.0) != 0 && problem == CM_PROB_DISABLED))
}

fn set_devnode(id: &str, disable: bool) -> bool {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    let mut devinst = 0u32;
    // A disabled or departed devnode may not be in the live tree — PHANTOM on enable so recovery
    // still finds it; disable requires a present device.
    let flags = if disable {
        CM_LOCATE_DEVNODE_NORMAL
    } else {
        CM_LOCATE_DEVNODE_PHANTOM
    };
    // SAFETY: `wide` is a live NUL-terminated UTF-16 instance id outliving the call; `devinst` is
    // a valid out-param.
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, PCWSTR(wide.as_ptr()), flags) };
    if cr != CR_SUCCESS {
        tracing::warn!(id, cr = cr.0, "PnP-disable: CM_Locate_DevNodeW failed");
        return false;
    }
    // SAFETY: `devinst` is the devnode the locate above resolved; plain value flags.
    let cr = unsafe {
        if disable {
            // PERSIST is the point: a standby monitor's hot-plug re-arrival must stay disabled.
            CM_Disable_DevNode(devinst, CM_DISABLE_PERSIST)
        } else {
            CM_Enable_DevNode(devinst, 0)
        }
    };
    if cr != CR_SUCCESS {
        tracing::warn!(
            id,
            cr = cr.0,
            disable,
            "PnP-disable: CM_{}_DevNode failed",
            if disable { "Disable" } else { "Enable" }
        );
        return false;
    }
    true
}

/// Journals before disabling. Returns the instance ids to re-enable at teardown. `generation` is
/// the topology generation of the acquire transaction the leases belong to.
pub fn disable_for_deactivated(
    saved: &crate::win_display::SavedConfig,
    keep: crate::win_display::CcdTargetKey,
    generation: u64,
) -> Vec<String> {
    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
    let mut targets: Vec<(String, String)> = Vec::new();
    for p in &saved.0 {
        if crate::win_display::path_target_key(p) == keep
            || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
        {
            continue;
        }
        match monitor_instance(p.targetInfo.adapterId, p.targetInfo.id) {
            Some(hit) => {
                if !targets.contains(&hit) {
                    targets.push(hit);
                }
            }
            None => tracing::debug!(
                target_id = p.targetInfo.id,
                "PnP-disable: no monitor device name for deactivated target — skipping"
            ),
        }
    }
    journal_and_disable(targets, LeaseSource::DeactivatedByUs, generation)
}

/// Disable the devnodes of every EXTERNAL PHYSICAL monitor that is connected but NOT part of the
/// desktop — regardless of who deactivated it. This is the standby-TV case the deactivated-set
/// selection above structurally misses: a TV that was never active has no pre-isolate active path,
/// yet its standby wake events (auto input scan, Instant-On HPD cycling) drive the same Windows
/// reaction cascade. Selection stays allowlist-precise via
/// [`crate::win_display::TargetInventory::external_physical`] — internal panels and
/// indirect/virtual targets (ours or third-party) can never be picked, and `keep_target_ids`
/// (the managed virtual set) is excluded belt-and-braces. Runs AFTER the topology action so the
/// active flags it reads are the settled ones. Journals like [`disable_for_deactivated`]; the
/// caller merges the returned ids into the same teardown list.
/// `baseline_active` is the caller's PRE-MUTATION snapshot — the targets that were part of the
/// desktop BEFORE this acquire touched the topology. It is what tells a genuine standby sink
/// (inactive before us too) from an operator display the isolate itself just switched off: the
/// post-isolate inventory alone cannot (immunity plan WP3a — the selector used to run after the
/// Exclusive isolate and disable the operator's freshly-deactivated panel as a "sink").
pub fn disable_connected_inactive(
    keep: &[crate::win_display::CcdTargetKey],
    baseline_active: &[crate::win_display::CcdTargetKey],
    generation: u64,
) -> Vec<String> {
    let targets = select_connected_inactive(
        &crate::win_display::target_inventory(),
        keep,
        baseline_active,
    );
    journal_and_disable(targets, LeaseSource::BaselineInactive, generation)
}

/// The pure selection half of [`disable_connected_inactive`], split from the PnP mutation so the
/// baseline rule is testable without a live CCD.
fn select_connected_inactive(
    inventory: &[crate::win_display::TargetInventory],
    keep: &[crate::win_display::CcdTargetKey],
    baseline_active: &[crate::win_display::CcdTargetKey],
) -> Vec<(String, String)> {
    let mut targets: Vec<(String, String)> = Vec::new();
    for t in inventory {
        if t.active || !t.external_physical || keep.contains(&t.key) {
            continue;
        }
        // Active before this acquire touched the topology ⇒ an operator display we (or a racing
        // actor) just switched off — NEVER a standby sink. A target absent from the baseline was
        // dark before us (or arrived dark) and stays a sink candidate.
        if baseline_active.contains(&t.key) {
            continue;
        }
        let Some(id) = instance_id_from_interface_path(&t.monitor_device_path) else {
            continue;
        };
        let hit = (id, format!("{} ({})", t.friendly, t.tech));
        if !targets.contains(&hit) {
            targets.push(hit);
        }
    }
    targets
}

/// Lease, journal, then disable; return what actually disabled (the teardown re-enable list). A
/// node the operator already disabled is not ours: no lease, no mutation, and the teardown never
/// enables it — the exact prior PnP state survives the stream.
fn journal_and_disable(
    targets: Vec<(String, String)>,
    source: LeaseSource,
    generation: u64,
) -> Vec<String> {
    if targets.is_empty() {
        tracing::debug!("PnP-disable: no physical monitor devnodes to disable");
        return Vec::new();
    }
    let mut ours = Vec::with_capacity(targets.len());
    for (id, name) in targets {
        match devnode_enabled(&id) {
            Some(true) => ours.push((id, name)),
            Some(false) => tracing::info!(
                id,
                monitor = name,
                "PnP-disable: devnode is already disabled (operator state) — not leased"
            ),
            None => tracing::debug!(id, monitor = name, "PnP-disable: devnode not locatable"),
        }
    }
    // Journal first (union with outstanding leases): a crash between here and the disable
    // over-recovers instead of leaking a disabled monitor.
    let mut journal = read_journal();
    for (id, _) in &ours {
        if !journal.iter().any(|l| &l.id == id) {
            journal.push(Lease {
                id: id.clone(),
                was_enabled: true,
                source,
                generation,
            });
        }
    }
    write_journal(&journal);
    let mut disabled = Vec::new();
    for (id, name) in ours {
        if set_devnode(&id, true) {
            tracing::info!(
                id,
                monitor = name,
                source = source.tag(),
                generation,
                "PnP-disable: monitor devnode disabled"
            );
            disabled.push(id);
        }
    }
    disabled
}

/// Re-enable `ids` and drop the leases that actually re-enabled. A failed re-enable must keep its
/// lease: that is the only record the devnode is still disabled, and the next [`startup_recover`]
/// is the only retry.
pub fn enable_instances(ids: &[String]) -> u32 {
    let mut ok = 0u32;
    let mut reenabled: Vec<&String> = Vec::with_capacity(ids.len());
    for id in ids {
        if set_devnode(id, false) {
            tracing::info!(id, "PnP-disable: monitor devnode re-enabled");
            reenabled.push(id);
            ok += 1;
        } else {
            tracing::warn!(
                id,
                "PnP-disable: monitor devnode re-enable FAILED — keeping its crash-journal \
                 entry so the next host start retries (until then this monitor stays disabled)"
            );
        }
    }
    let journal: Vec<Lease> = read_journal()
        .into_iter()
        .filter(|l| !reenabled.contains(&&l.id))
        .collect();
    write_journal(&journal);
    ok
}

/// Replay leftover leases from a previous host that crashed, was killed, or lost power. Call
/// once, early in `serve`.
pub fn startup_recover() {
    let leftovers = read_journal();
    if leftovers.is_empty() {
        return;
    }
    tracing::warn!(
        count = leftovers.len(),
        leases = ?leftovers,
        "PnP-disable: found monitor devnodes a previous host left disabled — re-enabling"
    );
    let ids: Vec<String> = leftovers.into_iter().map(|l| l.id).collect();
    enable_instances(&ids);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win_display::{CcdTargetKey, TargetInventory};

    fn target(luid: i64, id: u32, active: bool, external: bool) -> TargetInventory {
        TargetInventory {
            key: CcdTargetKey::new(luid, id),
            target_id: id,
            active,
            external_physical: external,
            internal_panel: false,
            tech: "HDMI",
            friendly: "ACME TV".into(),
            monitor_device_path: format!(r"\\?\DISPLAY#ACM{id:04}#5&1&0&UID{id}#{{guid}}"),
            ours: false,
            gdi_name: String::new(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            refresh_mhz: 0,
            primary: false,
            hdr: None,
            sdr_white_level: None,
            source_id: 0,
            source_adapter_luid: 0,
        }
    }

    /// WP3a's whole point: an operator display that was ACTIVE before this acquire touched the
    /// topology is never devnode-disabled by the default standby-sink treatment, however
    /// inactive it reads after the isolate — while a genuinely pre-dark sink still is.
    #[test]
    fn a_display_active_before_the_acquire_is_never_a_standby_sink() {
        let keep = [CcdTargetKey::new(9, 257)]; // the virtual display
                                                // Post-isolate view: the operator's panel (100) AND the standby TV (200) both read
                                                // connected-but-inactive — indistinguishable without the baseline.
        let inventory = [
            target(1, 100, false, true), // operator panel, deactivated by OUR isolate
            target(1, 200, false, true), // standby TV, dark before us too
            target(9, 257, true, false), // ours
        ];
        let baseline_active = [CcdTargetKey::new(1, 100)];
        let picked = select_connected_inactive(&inventory, &keep, &baseline_active);
        assert_eq!(picked.len(), 1, "only the pre-dark sink is selected");
        assert!(picked[0].0.contains("ACM0200"), "picked: {picked:?}");
        // Same-numbered target on ANOTHER adapter must not shadow the baseline entry.
        let alias_baseline = [CcdTargetKey::new(2, 100)];
        let picked = select_connected_inactive(&inventory, &keep, &alias_baseline);
        assert_eq!(
            picked.len(),
            2,
            "adapter 1's target 100 was NOT in this baseline"
        );
    }

    /// The exclusive re-assert passes an EMPTY baseline: a panel that re-lit itself while the
    /// isolate held it off is a sink from then on, operator display or not.
    #[test]
    fn an_empty_baseline_selects_the_re_lit_operator_panel() {
        let keep = [CcdTargetKey::new(9, 257)];
        let inventory = [
            target(1, 100, false, true), // the operator's panel, re-lit and re-deactivated
            target(9, 257, true, false), // ours
        ];
        let picked = select_connected_inactive(&inventory, &keep, &[]);
        assert_eq!(picked.len(), 1, "picked: {picked:?}");
        assert!(picked[0].0.contains("ACM0100"), "picked: {picked:?}");
    }

    /// The lease journal (WP11) round-trips prior state, selector and generation, and an older
    /// host's bare-id file still reads as recoverable leases.
    #[test]
    fn lease_journal_round_trips_and_reads_the_old_id_list() {
        let leases = vec![
            Lease {
                id: r"DISPLAY\ACM0200\5&1&0&UID200".into(),
                was_enabled: true,
                source: LeaseSource::BaselineInactive,
                generation: 7,
            },
            Lease {
                id: r"DISPLAY\ACM0100\5&1&0&UID100".into(),
                was_enabled: true,
                source: LeaseSource::DeactivatedByUs,
                generation: 7,
            },
        ];
        assert_eq!(decode_journal(&encode_journal(&leases)), leases);
        let legacy = br#"["DISPLAY\\ACM0300\\5&1&0&UID300"]"#;
        let decoded = decode_journal(legacy);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, r"DISPLAY\ACM0300\5&1&0&UID300");
        assert!(
            decoded[0].was_enabled,
            "a bare id is still owed a re-enable"
        );
        assert_eq!(decoded[0].generation, 0);
        assert!(decode_journal(b"not json").is_empty());
    }

    /// LIVE (elevated console session, `PUNKTFUNK_CONFIG_DIR` pointing at a journal a previous
    /// run left behind): the replay re-enables every leased devnode and empties the journal —
    /// the WP11 restart gate. A box with no leftover passes trivially.
    #[test]
    #[ignore = "live: needs an elevated console session and a leftover journal"]
    fn live_startup_recover_replays_the_lease_journal() {
        let before = read_journal();
        eprintln!("leftover leases before: {before:?}");
        startup_recover();
        let after = read_journal();
        eprintln!("leftover leases after: {after:?}");
        assert!(
            after.is_empty(),
            "every leftover lease must re-enable on replay: {after:?}"
        );
        for lease in before {
            assert_eq!(
                devnode_enabled(&lease.id),
                Some(true),
                "{} must read enabled after the replay",
                lease.id
            );
        }
    }
}
