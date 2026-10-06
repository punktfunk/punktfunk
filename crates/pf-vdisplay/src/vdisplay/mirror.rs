//! Stream a physical head the compositor already has, not a virtual output.
//! See `design/per-monitor-portal-capture.md`; Windows: `design/windows-wgc-capture.md` §4.6.
//!
//! Implements [`VirtualDisplay`] so it drops into the session machinery, but
//! creates nothing. [`DisplayOwnership::External`] is the contract: no keep-alive,
//! no topology, no reuse — the registry must not disable, re-position, or restore
//! a monitor the user is sitting in front of.
//!
//! `create()` ignores the requested [`Mode`]. A panel runs at the mode the user
//! set; the client scales. Selection is a host-level pin
//! (`PUNKTFUNK_CAPTURE_MONITOR`), not a per-session choice: one pin for the whole
//! host, so the shared injector cannot be re-aimed per session.

use super::backend::{DisplayOwnership, VirtualDisplay, VirtualOutput};
use super::monitors;
use crate::{Compositor, Mode};
use anyhow::{bail, Context, Result};

/// An existing-head recording: PipeWire node plus optional remote fd.
///
/// `remote_fd` is `None` when KWin/Mutter publish on the user's daemon. Portal
/// backends (sway/xdpw, Hyprland/xdph) hand back a sandboxed fd the capturer
/// must connect through — the same split their virtual-output paths already make.
#[cfg(target_os = "linux")]
pub(crate) struct MirrorStream {
    pub node_id: u32,
    pub remote_fd: Option<std::os::fd::OwnedFd>,
    /// Portal-negotiated cursor mode. Compositor-protocol backends (KWin/Mutter/gamescope)
    /// get what they ask for, so they leave this `None`.
    pub cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Dropping this ends the recording. Does not own the monitor.
    pub keepalive: Box<dyn Send>,
}

#[cfg(target_os = "linux")]
impl MirrorStream {
    /// This stream as a session cast ([`VirtualDisplay::join_cast`]).
    pub(crate) fn into_cast(self) -> super::backend::SessionCastParts {
        (self.node_id, self.remote_fd, self.keepalive)
    }
}

/// Streams an existing monitor, named by connector.
pub struct MirrorDisplay {
    compositor: Compositor,
    connector: String,
    hw_cursor: bool,
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
}

impl MirrorDisplay {
    pub fn new(compositor: Compositor, connector: String) -> Result<Self> {
        Ok(MirrorDisplay {
            compositor,
            connector,
            hw_cursor: false,
            last_cursor_mode: None,
        })
    }
}

impl VirtualDisplay for MirrorDisplay {
    fn name(&self) -> &'static str {
        "mirror"
    }

    fn producer(&self) -> &'static str {
        self.compositor.id()
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn poolable_now(&self) -> bool {
        // Asked before `create` can declare `External`. Default `true` would claim
        // a head we did not make and must not pool.
        false
    }

    fn create(&mut self, _mode: Mode) -> Result<VirtualOutput> {
        #[cfg(target_os = "windows")]
        return self.create_windows();
        #[cfg(not(target_os = "windows"))]
        self.create_cast()
    }
}

impl MirrorDisplay {
    /// Windows: the monitor itself is the output. Nothing is created and nothing is held; the
    /// capture worker opens the monitor by the name resolved here, and the target carries no
    /// driver process because no driver is involved.
    #[cfg(target_os = "windows")]
    fn create_windows(&mut self) -> Result<VirtualOutput> {
        let inventory = pf_win_display::display_events::snapshot_or_query()
            .targets
            .to_vec();
        let monitors = monitors::from_inventory(inventory.clone());
        let target = monitors::resolve(&monitors, &self.connector)?;
        check_mirrorable(target, self.compositor)?;
        let head = inventory
            .iter()
            .find(|t| t.gdi_name == target.connector)
            .with_context(|| {
                format!("monitor {:?} left the display inventory", target.connector)
            })?;
        tracing::info!(
            connector = %target.connector,
            mode = %target.mode_label(),
            at = %format!("+{}+{}", target.x, target.y),
            target_id = head.target_id,
            "mirroring an existing monitor (no virtual display created)"
        );
        let dims = (target.width, target.height, refresh_hz(target.refresh_mhz));
        let mut out = VirtualOutput::owned(0, Some(dims), Box::new(()));
        out.win_capture = Some(pf_frame::dxgi::WinCaptureTarget {
            adapter_luid: head.key.adapter_luid,
            gdi_name: head.gdi_name.clone(),
            target_id: head.target_id,
            wudf_pid: 0,
            cursor_excluded: false,
        });
        out.ownership = DisplayOwnership::External;
        Ok(out)
    }

    /// A compositor cast of the head: every backend but Windows.
    #[cfg(not(target_os = "windows"))]
    fn create_cast(&mut self) -> Result<VirtualOutput> {
        // Resolve first: geometry for the input anchor, and "that monitor is gone"
        // before any compositor call. `resolve` never substitutes another head.
        let monitors = monitors::list(self.compositor)
            .with_context(|| format!("enumerate monitors to mirror {:?}", self.connector))?;
        let target = monitors::resolve(&monitors, &self.connector)?;
        check_mirrorable(target, self.compositor)?;
        let origin = (target.x, target.y);
        let dims = (target.width, target.height, refresh_hz(target.refresh_mhz));

        let stream = match self.compositor {
            #[cfg(target_os = "linux")]
            Compositor::Kwin => {
                crate::kwin::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Mutter => {
                crate::mutter::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Wlroots => crate::portal_cast::stream_existing_output(
                &crate::wlroots::SELECTOR,
                &target.connector,
                self.hw_cursor,
            )?,
            #[cfg(target_os = "linux")]
            Compositor::Hyprland => crate::portal_cast::stream_existing_output(
                &crate::hyprland::SELECTOR,
                &target.connector,
                self.hw_cursor,
            )?,
            // DRM gamescope drives a real head; nested/headless reports none, so
            // `resolve` fails above and this arm is not reached for those.
            #[cfg(target_os = "linux")]
            Compositor::Gamescope => {
                crate::gamescope::stream_existing_output(&target.connector, self.hw_cursor)?
            }
            #[cfg(target_os = "linux")]
            Compositor::Windows => bail!("mirroring needs a Linux backend"),
            // Linux match is exhaustive: a new `Compositor` is a compile error here.
            // This arm exists because every arm above is `cfg(target_os = "linux")`.
            #[cfg(not(target_os = "linux"))]
            other => bail!(
                "mirroring an existing monitor is not supported on the {} backend",
                other.id()
            ),
        };

        self.last_cursor_mode = stream.cursor_mode;

        tracing::info!(
            connector = %target.connector,
            mode = %target.mode_label(),
            at = %format!("+{}+{}", origin.0, origin.1),
            node_id = stream.node_id,
            "mirroring an existing monitor (no virtual display created)"
        );

        // Dropping the keepalive stops the cast; we created nothing to restore.
        let mut out = VirtualOutput::owned(stream.node_id, Some(dims), stream.keepalive);
        out.remote_fd = stream.remote_fd;
        out.ownership = DisplayOwnership::External;
        // Host aims input; this crate must not depend on pf-inject. Connector equals
        // `wl_output.name` on wlroots/Hyprland; libei selects by region and cannot use it.
        out.output_name = Some(target.connector.clone());
        Ok(out)
    }
}

/// Refuse heads a compositor would otherwise answer with silence or a black stream.
fn check_mirrorable(target: &monitors::PhysicalMonitor, compositor: Compositor) -> Result<()> {
    if !target.enabled {
        bail!(
            "monitor {:?} is disabled — enable it before streaming it",
            target.connector
        );
    }
    if target.managed {
        // Only KWin `Virtual-punktfunk` and Hyprland `PF-N` names are ours by construction.
        // Sway names every headless output `HEADLESS-N`, so a refuse would block a real pin.
        if names_ours_conclusively(compositor) {
            bail!(
                "monitor {:?} is one of punktfunk's own virtual displays, not a physical head — \
                 clear the streamed-screen setting to use the normal virtual-display path",
                target.connector
            );
        }
        tracing::warn!(
            connector = %target.connector,
            "the pinned monitor looks like a headless output — on this compositor that name is \
             ambiguous (it may be one of punktfunk's own virtual displays), mirroring it anyway"
        );
    }
    if target.width == 0 || target.height == 0 {
        bail!(
            "monitor {:?} reports no current mode ({}x{}) — it is not driving a signal",
            target.connector,
            target.width,
            target.height
        );
    }
    Ok(())
}

/// Whether `managed` means "ours, for certain".
///
/// Exhaustive: warn-and-proceed is the wrong default — on KWin/Hyprland it
/// streams our own virtual display. A new `Compositor` must fail to compile here.
fn names_ours_conclusively(compositor: Compositor) -> bool {
    match compositor {
        // `Virtual-punktfunk-<id>` and `PF-N` are minted only by us.
        Compositor::Kwin | Compositor::Hyprland => true,
        // Sway's `HEADLESS-N` includes its own; Mutter has no distinguishing name;
        // gamescope only reports the real DRM head. A hint at most.
        Compositor::Wlroots | Compositor::Mutter | Compositor::Gamescope => false,
        // The inventory reads our EDID manufacturer id, which nothing else carries.
        Compositor::Windows => true,
    }
}

/// Whether a capture of the desktop can start now. Windows Graphics Capture shows neither the
/// lock screen nor the sign-in screen, and nobody signed in means no user to capture as, so a
/// pinned session that starts on either opens the ordinary virtual display instead.
#[cfg(target_os = "windows")]
pub(crate) fn desktop_capturable() -> bool {
    pf_win_display::refresh_secure_desktop();
    !pf_win_display::secure_desktop()
}

/// mHz → whole Hz for [`VirtualOutput::preferred_mode`]. Negotiation treats 0 as
/// "unset", so an unreported refresh becomes 60 — the same default KWin virtual
/// outputs are born with.
fn refresh_hz(mhz: u32) -> u32 {
    let hz = (mhz as f64 / 1000.0).round() as u32;
    if hz == 0 {
        60
    } else {
        hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mirror_names_the_compositor_that_produces_its_stream() {
        let kwin = MirrorDisplay::new(Compositor::Kwin, "DP-1".into()).unwrap();
        assert_eq!((kwin.name(), kwin.producer()), ("mirror", "kwin"));
    }

    fn head(connector: &str) -> monitors::PhysicalMonitor {
        monitors::PhysicalMonitor {
            connector: connector.into(),
            description: "ACME 27".into(),
            width: 2560,
            height: 1440,
            refresh_mhz: 144_000,
            x: 1920,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled: true,
            managed: false,
        }
    }

    #[test]
    fn a_real_enabled_head_is_mirrorable() {
        assert!(check_mirrorable(&head("DP-2"), Compositor::Kwin).is_ok());
    }

    #[test]
    fn a_disabled_head_is_refused_with_the_reason() {
        let mut m = head("DP-2");
        m.enabled = false;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn one_of_our_own_virtual_displays_is_refused() {
        let mut m = head("Virtual-punktfunk-1");
        m.managed = true;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("virtual displays"), "{err}");
    }

    /// Sway's `HEADLESS-N` is not proof of ours; refusing would block a headless sway pin.
    #[test]
    fn a_headless_sway_output_is_mirrored_despite_the_ambiguous_name() {
        let mut m = head("HEADLESS-2");
        m.managed = true;
        assert!(check_mirrorable(&m, Compositor::Wlroots).is_ok());
        // Hyprland treats `managed` as conclusive, so the same row is refused there.
        assert!(check_mirrorable(&m, Compositor::Hyprland).is_err());
    }

    /// Missing a naming backend from the `true` arm streams our own virtual display.
    #[test]
    fn the_conclusive_naming_table_is_pinned_per_backend() {
        assert!(names_ours_conclusively(Compositor::Kwin));
        assert!(names_ours_conclusively(Compositor::Hyprland));
        assert!(!names_ours_conclusively(Compositor::Wlroots));
        assert!(!names_ours_conclusively(Compositor::Mutter));
        assert!(!names_ours_conclusively(Compositor::Gamescope));
        assert!(names_ours_conclusively(Compositor::Windows));
    }

    /// `poolable_now` is asked before `create` reports ownership; both must say External.
    #[test]
    fn a_mirrored_head_is_never_registry_poolable() {
        let vd = MirrorDisplay::new(Compositor::Kwin, "DP-2".into()).unwrap();
        assert!(!vd.poolable_now());
    }

    /// Enabled but modeless would negotiate a 0x0 stream.
    #[test]
    fn a_head_with_no_current_mode_is_refused() {
        let mut m = head("DP-2");
        m.width = 0;
        m.height = 0;
        let err = check_mirrorable(&m, Compositor::Kwin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no current mode"), "{err}");
    }

    #[test]
    fn refresh_rounds_and_never_reports_zero() {
        assert_eq!(refresh_hz(60000), 60);
        assert_eq!(refresh_hz(59940), 60);
        assert_eq!(refresh_hz(119_920), 120);
        assert_eq!(refresh_hz(0), 60);
    }
}
