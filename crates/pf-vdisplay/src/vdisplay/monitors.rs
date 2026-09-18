//! Physical-monitor enumeration: heads the compositor already has.
//!
//! Read-only. Everything else in this crate creates and owns virtual outputs;
//! this module only reports existing heads so an operator can pin capture
//! (`PUNKTFUNK_CAPTURE_MONITOR`) and the console can render a picker.
//! See `design/per-monitor-portal-capture.md`.
//!
//! Lives here because enumeration is per-compositor and this crate already
//! speaks those dialects. Each backend implements listing beside its other
//! IPC; this module is the shared type and the dispatch.
//!
//! `x`/`y` identify a head: two monitors can share a size, never an origin
//! in the compositor's global space. Size-keyed matching is a trap — see
//! `pf-inject`'s absolute-coordinate region selection.

use crate::Compositor;
use anyhow::Result;
// Only the non-Linux fallback arm of `list` bails; on Linux every backend has its own arm, so an
// unconditional import is an unused one under `-D warnings`.
#[cfg(not(target_os = "linux"))]
use anyhow::bail;

/// One head as the compositor currently reports it.
///
/// `x`/`y` are logical — compositor global layout, the same space libei
/// regions use. `width`/`height` are the current mode in pixels, which is
/// what every backend reports and what a capturer opens against. `scale`
/// is the factor between them; [`Self::logical_size`] is the only correct
/// way to compare a size against `x`/`y`. Mixing the two spaces is a trap.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalMonitor {
    /// Connector name (`DP-1`, `HDMI-A-2`). The id `PUNKTFUNK_CAPTURE_MONITOR` names.
    pub connector: String,
    /// Human label for a picker (`make model`, else the connector). Never used for matching.
    pub description: String,
    /// Current mode in pixels, not logical size. See [`Self::logical_size`].
    pub width: u32,
    pub height: u32,
    /// Refresh in mHz (60000 = 60 Hz). 0 when the backend doesn't report it.
    pub refresh_mhz: u32,
    /// Top-left in the compositor's global logical space — the identity key (see the module doc).
    pub x: i32,
    pub y: i32,
    /// Logical scale factor (1.0 when unreported).
    pub scale: f64,
    /// The compositor's primary/focused head, when it says.
    pub primary: bool,
    /// Enabled (driven). A disabled head is still listed, so "why can't I pick it?" has an answer.
    pub enabled: bool,
    /// Best-effort: we created this output. Only KWin names them distinctly;
    /// other backends leave this `false` rather than guess. Grey out, never filter.
    pub managed: bool,
}

/// Picker label from make/model, else the connector.
///
/// Compositors write the literal `"Unknown"` rather than leaving fields empty;
/// treat that as absent so a row does not read "Unknown Unknown".
pub(crate) fn describe(make: &str, model: &str, connector: &str) -> String {
    let known = |s: &str| {
        let s = s.trim();
        !s.is_empty() && !s.eq_ignore_ascii_case("unknown")
    };
    let label = [make, model]
        .iter()
        .map(|s| s.trim())
        .filter(|s| known(s))
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        connector.to_string()
    } else {
        label
    }
}

impl PhysicalMonitor {
    /// Extent in the same space as `x`/`y`: mode pixels divided by `scale`.
    ///
    /// The only correct way to ask whether a layout coordinate is inside this
    /// head. Comparing `width`/`height` to `x`/`y` is right only at scale 1.0
    /// (a 3840-px panel at 150% occupies 2560 logical units). A non-positive
    /// scale is treated as 1.0 rather than dividing by zero.
    pub fn logical_size(&self) -> (f64, f64) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        (
            f64::from(self.width) / scale,
            f64::from(self.height) / scale,
        )
    }

    /// `1920x1080@60` — for logs and pickers.
    pub fn mode_label(&self) -> String {
        if self.refresh_mhz == 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            format!(
                "{}x{}@{}",
                self.width,
                self.height,
                (self.refresh_mhz as f64 / 1000.0).round() as u32
            )
        }
    }
}

/// Every head `compositor` reports, in compositor order.
///
/// Backend unreachable is an error; `Ok(vec![])` means reached it, no heads.
/// A picker can treat both as empty; a pin resolver must not (see [`resolve`]).
pub fn list(compositor: Compositor) -> Result<Vec<PhysicalMonitor>> {
    match compositor {
        // Goes through `kwin` so the in-process-then-`kscreen-doctor` ladder
        // degrades like every other KWin op, not a hard-fail on a wedged compositor.
        #[cfg(target_os = "linux")]
        Compositor::Kwin => crate::kwin::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Mutter => crate::mutter::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Wlroots => crate::wlroots::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Hyprland => crate::hyprland::list_monitors(),
        // Game Mode gamescope is DRM master and has a real connector; nested
        // ones this crate spawns are headless. Empty list, not an error, for those.
        #[cfg(target_os = "linux")]
        Compositor::Gamescope => crate::gamescope::list_monitors(),
        #[cfg(target_os = "linux")]
        Compositor::Windows => {
            anyhow::bail!("physical-monitor enumeration needs a Linux backend")
        }
        #[cfg(not(target_os = "linux"))]
        _ => bail!("physical-monitor enumeration is implemented for the Linux backends only"),
    }
}

/// Every head Windows reports — the non-compositor counterpart to [`list`].
///
/// Reads the CCD database the Windows backend already drives. Inactive heads
/// are listed with zeroed geometry and `enabled: false`, matching [`list`].
///
/// Two fields cannot mean here what they mean on Linux:
/// * `scale` is always `1.0`. Windows scaling is per-monitor DPI per application,
///   not a compositor-global logical scale, so the geometry is pixels.
/// * `refresh_mhz` comes from the path's own rational rate (59.94 stays distinct from 60).
#[cfg(windows)]
pub fn list_windows() -> Result<Vec<PhysicalMonitor>> {
    // `Ok` on an empty inventory, matching [`list`]: every panel off is a real state. The display
    // actor's cached snapshot (immunity plan WP9) — a direct read only before its first publish.
    Ok(from_inventory(
        pf_win_display::display_events::snapshot_or_query()
            .targets
            .to_vec(),
    ))
}

/// CCD inventory → [`PhysicalMonitor`]. Split from the OS call so tests can
/// cover the pin mapping without touching the display database.
#[cfg(windows)]
fn from_inventory(inv: Vec<pf_win_display::win_display::TargetInventory>) -> Vec<PhysicalMonitor> {
    inv.into_iter()
        .map(|t| {
            // GDI name is what capture pins on; inactive paths have none, so
            // fall back to `target-{id}` — `resolve` matches this, a blank cannot.
            let connector = if t.gdi_name.is_empty() {
                format!("target-{}", t.target_id)
            } else {
                t.gdi_name
            };
            PhysicalMonitor {
                description: describe("", &t.friendly, &connector),
                connector,
                width: t.width,
                height: t.height,
                refresh_mhz: t.refresh_mhz,
                x: t.x,
                y: t.y,
                scale: 1.0,
                primary: t.primary,
                enabled: t.active,
                // IddCx monitors carry our EDID manufacturer id in the device path.
                managed: t.ours,
            }
        })
        .collect()
}

/// Resolve a configured monitor name against `monitors`, exact then case-insensitive.
///
/// A miss is a hard error listing available names, never a silent fallback:
/// pinning `DP-2` and streaming `DP-1` is worse than refusing to start.
/// See `design/per-monitor-portal-capture.md`.
pub fn resolve<'a>(monitors: &'a [PhysicalMonitor], want: &str) -> Result<&'a PhysicalMonitor> {
    if let Some(m) = monitors.iter().find(|m| m.connector == want) {
        return Ok(m);
    }
    if let Some(m) = monitors
        .iter()
        .find(|m| m.connector.eq_ignore_ascii_case(want))
    {
        return Ok(m);
    }
    Err(MonitorNotFound {
        want: want.to_string(),
        available: monitors.iter().map(|m| m.connector.clone()).collect(),
    }
    .into())
}

/// A [`resolve`] miss, typed so the host can hand the client a sentence naming the
/// pin rather than the operator chain. `available` empty = the compositor reports
/// no monitors at all.
#[derive(Debug, Clone)]
pub struct MonitorNotFound {
    pub want: String,
    pub available: Vec<String>,
}

/// The operator line. The session pipeline matches its `no monitor named` prefix to
/// refuse a retry, so the opening is a contract, not a wording choice.
impl std::fmt::Display for MonitorNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let want = &self.want;
        if self.available.is_empty() {
            return write!(
                f,
                "no monitor named {want:?} — this compositor reports no monitors at all"
            );
        }
        write!(
            f,
            "no monitor named {want:?} — this host has: {}",
            self.available.join(", ")
        )
    }
}

impl std::error::Error for MonitorNotFound {}

/// Names beyond this go unlisted: the sentence rides a 256-byte close frame, and
/// losing the next move to a truncation costs more than losing the fifth name.
const NAMED_IN_SENTENCE: usize = 4;

impl MonitorNotFound {
    /// The user's sentence — what did not happen, then the move that fixes it.
    /// Naming the heads the host does have is that move: the pin is a setting.
    pub fn user_message(&self) -> String {
        let want = &self.want;
        if self.available.is_empty() {
            return format!(
                "The host is set to capture a monitor called \"{want}\", and reports no \
                 monitors at all. Clear that in the host's display settings to stream a \
                 virtual display instead."
            );
        }
        let mut names = self
            .available
            .iter()
            .take(NAMED_IN_SENTENCE)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if self.available.len() > NAMED_IN_SENTENCE {
            names.push_str(", …");
        }
        format!(
            "The host is set to capture a monitor called \"{want}\", which isn't connected to \
             it. Change it to one the host has ({names}) in the host's display settings, or \
             clear it to stream a virtual display instead."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(connector: &str) -> PhysicalMonitor {
        PhysicalMonitor {
            connector: connector.into(),
            description: String::new(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled: true,
            managed: false,
        }
    }

    #[test]
    fn resolve_matches_exactly_then_case_insensitively() {
        let ms = [mon("DP-1"), mon("HDMI-A-2")];
        assert_eq!(resolve(&ms, "DP-1").unwrap().connector, "DP-1");
        assert_eq!(resolve(&ms, "hdmi-a-2").unwrap().connector, "HDMI-A-2");
    }

    #[test]
    fn resolve_prefers_the_exact_match_over_a_case_fold() {
        // Connector names are case-sensitive to the compositor; exact wins over a fold.
        let ms = [mon("dp-1"), mon("DP-1")];
        assert_eq!(resolve(&ms, "DP-1").unwrap().connector, "DP-1");
    }

    #[test]
    fn a_miss_lists_what_is_available() {
        let ms = [mon("DP-1"), mon("HDMI-A-2")];
        let err = resolve(&ms, "DP-9").unwrap_err().to_string();
        assert!(err.contains("DP-1") && err.contains("HDMI-A-2"), "{err}");
    }

    #[test]
    fn a_miss_with_no_monitors_says_so() {
        let err = resolve(&[], "DP-1").unwrap_err().to_string();
        assert!(err.contains("no monitors at all"), "{err}");
    }

    /// Reads the sentence off the error `resolve` really produces, so a reworded
    /// miss cannot leave the client's half behind.
    #[test]
    fn a_miss_words_the_pin_and_the_heads_for_the_user() {
        let ms = [mon("HDMI-A-1"), mon("DP-1")];
        let err = resolve(&ms, "HDMI-A-3").unwrap_err();
        let said = err
            .downcast_ref::<MonitorNotFound>()
            .expect("a miss stays typed through anyhow")
            .user_message();
        assert!(said.contains("HDMI-A-3"), "names the pin: {said}");
        assert!(said.contains("HDMI-A-1"), "names a head it has: {said}");
        assert!(
            !said.contains("no monitor named"),
            "not the operator line: {said}"
        );
        assert!(
            said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX,
            "fits one close frame uncut: {} bytes",
            said.len()
        );
    }

    /// Sixteen connectors is the policy ceiling; the sentence still has to arrive
    /// whole, so the list is what gets cut, never the next move.
    #[test]
    fn a_miss_on_a_crowded_host_still_fits_the_close_frame() {
        let ms = (0..16).map(|i| mon(&format!("DP-{i}"))).collect::<Vec<_>>();
        let said = resolve(&ms, "HDMI-A-3")
            .unwrap_err()
            .downcast_ref::<MonitorNotFound>()
            .unwrap()
            .user_message();
        assert!(
            said.ends_with("virtual display instead."),
            "keeps the move: {said}"
        );
        assert!(
            said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX,
            "fits one close frame uncut: {} bytes",
            said.len()
        );
    }

    #[test]
    fn describe_falls_back_to_the_connector_for_empty_or_unknown_fields() {
        assert_eq!(describe("ACME", "U2720Q", "DP-1"), "ACME U2720Q");
        assert_eq!(describe("Unknown", "Unknown", "HEADLESS-1"), "HEADLESS-1");
        assert_eq!(describe("", "", "DP-1"), "DP-1");
        assert_eq!(describe("Unknown", "U2720Q", "DP-1"), "U2720Q");
        assert_eq!(describe("  ", "unknown", "DP-2"), "DP-2");
    }

    #[test]
    fn logical_size_divides_the_mode_by_the_scale() {
        let mut m = mon("DP-1");
        m.width = 3840;
        m.height = 2160;
        m.scale = 1.5;
        assert_eq!(m.logical_size(), (2560.0, 1440.0));
        m.scale = 1.0;
        assert_eq!(m.logical_size(), (3840.0, 2160.0));
        // Non-positive scale must not produce infinity or NaN.
        m.scale = 0.0;
        assert_eq!(m.logical_size(), (3840.0, 2160.0));
    }

    #[test]
    fn mode_label_drops_an_unknown_refresh() {
        let mut m = mon("DP-1");
        assert_eq!(m.mode_label(), "1920x1080@60");
        m.refresh_mhz = 0;
        assert_eq!(m.mode_label(), "1920x1080");
    }
}

/// Windows inventory mapping — exercises [`from_inventory`] without an OS call.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use pf_win_display::win_display::TargetInventory;

    /// One inventory row. The foreign struct has no `Default`.
    fn target(target_id: u32, gdi_name: &str, active: bool) -> TargetInventory {
        TargetInventory {
            key: pf_win_display::win_display::CcdTargetKey::new(0, target_id),
            target_id,
            active,
            external_physical: true,
            internal_panel: false,
            tech: "HDMI",
            friendly: "ACME TV".into(),
            monitor_device_path: r"\\?\DISPLAY#ACM1234#".into(),
            ours: false,
            gdi_name: gdi_name.into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            refresh_mhz: 59940,
            primary: active,
            hdr: active.then_some(false),
            sdr_white_level: None,
            source_id: target_id,
            source_adapter_luid: 0,
        }
    }

    /// Inactive paths have no GDI name; list them under a pinnable `target-{id}`.
    #[test]
    fn an_inactive_path_gets_a_target_id_connector_and_enabled_false() {
        let mons = from_inventory(vec![target(4352, "", false)]);
        assert_eq!(mons.len(), 1);
        assert_eq!(mons[0].connector, "target-4352");
        assert!(!mons[0].enabled);
        assert_eq!(mons[0].scale, 1.0);
    }

    /// Whatever connector this mapping synthesizes, [`resolve`] must find it.
    #[test]
    fn resolve_can_find_a_synthesized_target_name() {
        let mons = from_inventory(vec![
            target(4352, "", false),
            target(1, r"\\.\DISPLAY1", true),
        ]);
        assert_eq!(
            resolve(&mons, "target-4352")
                .expect("synthesized name")
                .width,
            1920
        );
        assert_eq!(
            resolve(&mons, r"\\.\DISPLAY1").expect("gdi name").connector,
            r"\\.\DISPLAY1"
        );
        assert!(
            resolve(&mons, r"\\.\display1").is_ok(),
            "and case-insensitively, as `resolve` promises"
        );
    }

    #[test]
    fn our_own_idd_is_marked_managed() {
        let mut ours = target(257, r"\\.\DISPLAY2", true);
        ours.ours = true;
        let mons = from_inventory(vec![ours]);
        assert!(mons[0].managed);
        assert!(!from_inventory(vec![target(1, r"\\.\DISPLAY1", true)])[0].managed);
    }
}
