//! `pf-vdisplay` under this crate's name, plus what needs the host's session table: the
//! screen a device gets, checked against the monitors other sessions stream.
//!
//! One live mirror per monitor (`screen-pick.md` D9): absolute input aims at one head per
//! host process, so a second device set to a monitor another one streams gets its own screen.

pub(crate) use pf_vdisplay::*;

use pf_vdisplay::monitors::PhysicalMonitor;
use pf_vdisplay::policy::{DisplayTarget, ScreenPick, ScreenReason};
use punktfunk_core::quic::v2::hello::Role;

/// The screen `client` gets now: `wanted`, else what the operator set it to ([`screen_target`]
/// without its own pick), or its own screen when that monitor is off, missing, or streamed by
/// another device.
pub(crate) fn resolve_screen(
    client: Option<[u8; 32]>,
    wanted: Option<DisplayTarget>,
) -> ScreenPick {
    pick(None, policy::fp_hex(client).as_deref(), wanted)
}

/// Open the display `client` gets on `compositor` ([`resolve_screen`]), and anchor absolute
/// input at it: the monitor it mirrors, or no monitor for its own screen.
pub(crate) fn open_for(
    compositor: Compositor,
    client: Option<[u8; 32]>,
    wanted: Option<DisplayTarget>,
) -> anyhow::Result<Box<dyn VirtualDisplay>> {
    let pick = pick(Some(compositor), policy::fp_hex(client).as_deref(), wanted);
    let vd = pf_vdisplay::open_for(compositor, pick.target.clone())?;
    #[cfg(target_os = "linux")]
    crate::aim_capture_anchor(pick.target.into_monitor(), "session open");
    Ok(vd)
}

/// The topology the own screen of a session of `client` opens under on `compositor`: `wanted`,
/// then the first-session rule against every other device's screen here, and whether that rule
/// changed it. `None` leaves the backend its own read. Logs when an earlier device decided it.
pub(crate) fn session_topology(
    client: Option<[u8; 32]>,
    wanted: Option<policy::Topology>,
    compositor: Compositor,
) -> (Option<policy::Topology>, bool) {
    let Some(wanted) = wanted else {
        return (None, false);
    };
    let me = policy::fp_hex(client);
    let holders = screen_holders();
    let rule =
        policy::first_session_rule(wanted, me.as_deref(), &holders, compositor.has_primary());
    let Some((kept, first)) = rule else {
        return (Some(wanted), false);
    };
    let first = first
        .as_deref()
        .map_or_else(|| "unknown".into(), device_label);
    tracing::info!(%first, ?wanted, topology = ?kept,
        "monitors stay as the first session left them");
    (Some(kept), true)
}

/// Whether a session on `compositor` plays on the desk, the desktop that holds the box's
/// monitors, where a device's own screen takes a role: not a contract `seat`, not gamescope,
/// and heads that read (`physical`) list a physical monitor. Heads that cannot be read keep it.
pub(crate) fn on_the_desk(
    seat: bool,
    compositor: Option<Compositor>,
    physical: Option<bool>,
) -> bool {
    !seat && compositor != Some(Compositor::Gamescope) && physical != Some(false)
}

/// [`on_the_desk`] for a session on `compositor` now. Never on a host with no mirror backend.
/// With `read_heads` it asks the compositor for its monitors: blocking, one read. Without, a
/// desktop that may have none still counts, as one whose heads cannot be read does.
pub(crate) fn desk(compositor: Option<Compositor>, read_heads: bool) -> bool {
    let physical = read_heads
        .then(|| heads(compositor).ok())
        .flatten()
        .map(|h| h.iter().any(|m| !m.managed));
    cfg!(any(target_os = "linux", target_os = "windows"))
        && on_the_desk(policy::prefs().follows_contract(), compositor, physical)
}

/// The role an own screen takes under `topology`. Without a main screen (`primary` false)
/// `primary` extends, so it is the second screen.
pub(crate) fn role_of(topology: policy::Topology, primary: bool) -> Role {
    match topology {
        policy::Topology::Exclusive => Role::Only,
        policy::Topology::Primary if primary => Role::Main,
        policy::Topology::Primary | policy::Topology::Extend | policy::Topology::Auto => {
            Role::Second
        }
    }
}

/// Whether `seat`'s gamescope shows HDR content now. One that is not up shows none.
/// Blocking: X round trips.
pub(crate) fn hdr_content_now(seat: Option<&str>) -> bool {
    #[cfg(target_os = "linux")]
    {
        GamescopeHdrContent::default().poll(seat).unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = seat;
        false
    }
}

/// The heads of `compositor`, else of the live session's. Blocking: one compositor IPC.
pub(crate) fn heads(compositor: Option<Compositor>) -> anyhow::Result<Vec<PhysicalMonitor>> {
    #[cfg(windows)]
    {
        let _ = compositor;
        monitors::list_windows()
    }
    #[cfg(not(windows))]
    {
        compositor.map_or_else(detect, Ok).and_then(monitors::list)
    }
}

/// The pairing label of the device `fp` (hex) as its live session shows it, else its prefix.
pub(crate) fn device_label(fp: &str) -> String {
    crate::session_status::snapshot()
        .into_iter()
        .find(|s| crate::session_status::is_client(&s.client, fp))
        .and_then(|s| s.client_name)
        .unwrap_or_else(|| fp.chars().take(12).collect())
}

/// [`resolve_screen`] for the device `fp`, against the heads of `compositor`.
fn pick(
    compositor: Option<Compositor>,
    fp: Option<&str>,
    wanted: Option<DisplayTarget>,
) -> ScreenPick {
    let (target, locked) = match wanted {
        Some(t) => (t, pf_host_config::config().capture_monitor.is_some()),
        None => screen_target(fp, false),
    };
    let DisplayTarget::Monitor(want) = &target else {
        return ScreenPick {
            target,
            locked,
            reason: None,
        };
    };
    let listed = if cfg!(any(target_os = "linux", target_os = "windows")) {
        heads(compositor).ok()
    } else {
        Some(Vec::new())
    };
    let reason = unavailable(want, listed.as_deref(), &streamed_monitors(), fp);
    let Some(reason) = reason else {
        return ScreenPick {
            target,
            locked,
            reason: None,
        };
    };
    let device = fp.map_or("anonymous".into(), |f| {
        f.chars().take(12).collect::<String>()
    });
    match &reason {
        ScreenReason::Taken(by) => tracing::info!(connector = %want, by = %by, device = %device,
            "screen is streamed by another device; this device gets its own screen"),
        _ => tracing::warn!(connector = %want, reason = ?reason, device = %device,
            "screen is off or missing; this device gets its own screen"),
    }
    ScreenPick {
        target: DisplayTarget::Own,
        locked,
        reason: Some(reason),
    }
}

/// Why the monitor `want` cannot stream to the device `me`: off or missing among `heads`, or
/// in `streamed` to another named device. Unreadable `heads` leave the verdict to the mirror.
/// A mirror that names no device holds nothing: it cannot be told from `me` itself.
fn unavailable(
    want: &str,
    heads: Option<&[PhysicalMonitor]>,
    streamed: &[(String, Option<String>)],
    me: Option<&str>,
) -> Option<ScreenReason> {
    if let Some(heads) = heads {
        // Only our own outputs, or none: a nested or headless session has no desk to mirror.
        if heads.iter().all(|h| h.managed) {
            return Some(ScreenReason::Missing);
        }
        match monitors::resolve(heads, want) {
            Err(_) => return Some(ScreenReason::Missing),
            Ok(m) if !m.enabled => return Some(ScreenReason::Off),
            Ok(_) => {}
        }
    }
    let by = streamed.iter().find_map(|(connector, owner)| {
        let owner = owner.as_deref()?;
        (connector.eq_ignore_ascii_case(want) && Some(owner) != me).then_some(owner)
    })?;
    Some(ScreenReason::Taken(device_label(by)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(connector: &str, enabled: bool) -> PhysicalMonitor {
        PhysicalMonitor {
            connector: connector.into(),
            description: "ACME 27".into(),
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled,
            managed: false,
        }
    }

    /// Off and missing heads fall back; a head another named device streams is taken, the
    /// device's own mirror and an unnamed one are not.
    #[test]
    fn a_monitor_falls_back_when_off_missing_or_taken() {
        let heads = [head("DP-1", true), head("HDMI-A-1", false)];
        let none: &[(String, Option<String>)] = &[];
        assert_eq!(unavailable("DP-1", Some(&heads), none, Some("aa")), None);
        assert_eq!(
            unavailable("HDMI-A-1", Some(&heads), none, Some("aa")),
            Some(ScreenReason::Off)
        );
        assert_eq!(
            unavailable("DP-9", Some(&heads), none, Some("aa")),
            Some(ScreenReason::Missing)
        );
        assert_eq!(unavailable("DP-9", None, none, Some("aa")), None, "unread");

        let tv = "bb22".repeat(16);
        let streamed = [("dp-1".to_string(), Some(tv.clone()))];
        assert_eq!(
            unavailable("DP-1", Some(&heads), &streamed, Some("aa")),
            Some(ScreenReason::Taken(tv[..12].to_string()))
        );
        assert_eq!(
            unavailable("DP-1", Some(&heads), &streamed, Some(&tv)),
            None
        );
        let unnamed = [("DP-1".to_string(), None)];
        assert_eq!(
            unavailable("DP-1", Some(&heads), &unnamed, Some("aa")),
            None
        );
    }
}
