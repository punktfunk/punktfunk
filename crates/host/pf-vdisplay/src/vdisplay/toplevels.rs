//! The compositor's windows, and the verbs the launch path runs on the game's.
//!
//! [`list_all_toplevels`] is the host's own view of every head, read to spot a
//! launched game's window. [`list_toplevels`] is one streamed head only, and
//! [`window_action`] refuses any id that head does not hold.

use super::*;

/// One compositor toplevel, as every backend reports it.
///
/// `id` is the backend's own handle (a Hyprland address, a sway con id) and is
/// opaque above this module: it goes back to the backend that minted it and
/// nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toplevel {
    /// Backend handle. Never parsed above the backend, never reused across one.
    pub id: String,
    pub title: String,
    /// Wayland `app_id`, or the X11 class on an Xwayland window.
    pub app_id: String,
    /// `None` where the compositor does not report one (some Xwayland cons).
    pub pid: Option<u32>,
    pub focused: bool,
    pub fullscreen: bool,
    /// Workspace name as the compositor spells it, not its id.
    pub workspace: String,
    /// The head this window is on.
    pub output: String,
}

/// What the host does to a launched game's window, per its `on_window`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowVerb {
    /// Raise and give it focus.
    Focus,
    /// Make it full-screen on its head.
    Fullscreen,
}

impl WindowVerb {
    /// Operator-log name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Focus => "focus",
            Self::Fullscreen => "fullscreen",
        }
    }
}

/// Windows on streamed head `output`, newest compositor state each call.
///
/// Empty on any backend that cannot be asked, and on any read that fails.
/// `output` is checked against the backend's own mint, as in
/// [`focus_streamed_output`]: a physical connector's windows are the
/// operator's, not this session's.
pub fn list_toplevels(compositor: Compositor, output: &str) -> Vec<Toplevel> {
    match compositor {
        Compositor::Hyprland if hyprland::is_managed_output(output) => {
            hyprland::toplevels(Some(output))
        }
        Compositor::Wlroots if wlroots::is_managed_output(output) => {
            wlroots::toplevels(Some(output))
        }
        // No `_` arm: a new backend must decide here. KWin and GNOME list
        // windows but name no head; gamescope nests one app; Windows has no
        // compositor to ask.
        Compositor::Hyprland
        | Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope
        | Compositor::Windows => {
            tracing::info!(
                compositor = compositor.id(), output = %output,
                "no window list for this head"
            );
            Vec::new()
        }
    }
}

/// Every window on every head, for the host's own window stage. `None` when
/// this compositor cannot list its windows at all right now: KWin has not
/// granted the protocol, GNOME has not loaded punktfunk's extension, or it is
/// gamescope or Windows. An empty list means it can, and nothing is open.
///
/// Never a client's answer: it names the operator's other monitors, which is
/// what [`list_toplevels`] exists to keep out. KWin reads it over
/// `org_kde_plasma_window_management` and GNOME from punktfunk's shell
/// extension; neither names a window's head.
pub fn list_all_toplevels(compositor: Compositor) -> Option<Vec<Toplevel>> {
    match compositor {
        Compositor::Hyprland => Some(hyprland::toplevels(None)),
        Compositor::Wlroots => Some(wlroots::toplevels(None)),
        Compositor::Kwin => kwin_windows::toplevels(),
        Compositor::Mutter => gnome_windows::toplevels(),
        Compositor::Gamescope | Compositor::Windows => None,
    }
}

/// Can the host move, focus and fullscreen this compositor's windows
/// ([`move_toplevel_to_output`], [`window_action`])?
pub fn places_windows(compositor: Compositor) -> bool {
    match compositor {
        Compositor::Hyprland | Compositor::Wlroots => true,
        // No `_` arm; same backends as [`window_action`].
        Compositor::Kwin | Compositor::Mutter | Compositor::Gamescope | Compositor::Windows => {
            false
        }
    }
}

/// Carry window `id` onto head `output`.
///
/// The game opened where the compositor put it, and the player can only see
/// the streamed head.
pub fn move_toplevel_to_output(
    compositor: Compositor,
    id: &str,
    output: &str,
) -> anyhow::Result<()> {
    match compositor {
        Compositor::Hyprland => hyprland::move_to_output(id, output),
        Compositor::Wlroots => wlroots::move_to_output(id, output),
        // No `_` arm. KWin/Mutter/gamescope/Windows never list, so nothing
        // here has an id to move.
        Compositor::Kwin | Compositor::Mutter | Compositor::Gamescope | Compositor::Windows => {
            anyhow::bail!("{} cannot move a window", compositor.id())
        }
    }
}

/// Run `verb` on window `id` of streamed head `output`.
///
/// `id` must still be in [`list_toplevels`] for this head: an id the
/// compositor recycled, or one on the operator's own monitor, is refused here
/// rather than acted on.
pub fn window_action(
    compositor: Compositor,
    output: &str,
    verb: WindowVerb,
    id: &str,
) -> anyhow::Result<()> {
    // Re-read rather than trust the caller's copy: the id may have died since,
    // and a recycled one names some other window.
    if !list_toplevels(compositor, output)
        .iter()
        .any(|w| w.id == id)
    {
        anyhow::bail!("no window {id} on {output}");
    }
    match compositor {
        Compositor::Hyprland => hyprland::window_action(verb, id),
        Compositor::Wlroots => wlroots::window_action(verb, id),
        // No `_` arm. Unreachable while the backends above are the only ones
        // that list, and a compile error the day another one does.
        Compositor::Kwin | Compositor::Mutter | Compositor::Gamescope | Compositor::Windows => {
            anyhow::bail!("{} cannot act on a window", compositor.id())
        }
    }
}

/// Token that changes when this compositor's windows do, so an idle session
/// costs no `hyprctl`. `None` = this backend cannot say; re-read every time.
pub fn toplevels_token(compositor: Compositor) -> Option<u64> {
    match compositor {
        Compositor::Hyprland => hyprland::window_gen(),
        // No `_` arm. sway has an IPC subscribe, but the host runs no reader on
        // it; the rest have no list to watch.
        Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope
        | Compositor::Windows => None,
    }
}
