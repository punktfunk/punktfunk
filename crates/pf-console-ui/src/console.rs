//! Portable console driver: the object a host holds (`android-skia-console-port.md`).
//! Owns the shell and fonts; host-facing vocabulary only: a canvas + [`Viewport`]
//! per frame, [`MenuEvent`]s, [`PointerInput`], [`Key`]s and text in;
//! [`OverlayAction`]s out; [`SessionPhase`] edges back. The Vulkan session's
//! [`crate::SkiaOverlay`] and the Android GL host both sit on this; nothing here
//! knows a `VkImage`, an SDL event, or a JNI env.

use crate::model::{ConsoleBus, ConsoleCmd, ConsoleShared, HostRow};
use crate::screens::Screen;
use crate::shell::{ConsoleOptions, Shell};
use crate::theme::Fonts;
use anyhow::Result;
use pf_client_core::console::{OverlayAction, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use punktfunk_core::config::GamepadPref;
use skia_safe::Canvas;
use std::time::{Duration, Instant};

pub use crate::input::Key;

/// Device family for the hint legend. Pointer has no source: a tap does not say
/// which buttons the other hand holds, so the legend stays as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// Glyphs follow the active pad's family.
    Pad,
    /// TV remote D-pad on Android; keyboard on desktop.
    Keys,
}

pub enum ConsoleEntry {
    /// Host list (`--browse`; Android Home).
    Home,
    /// Home with this host's library pushed (`--browse host`). B pops to Home.
    /// `Box` because `HostRow` is larger than the other variant.
    Library(Box<HostRow>),
    /// [`Self::Library`] plus one connect to the host's desktop, raised before the first
    /// frame (`start_in = stream`). Cancel or a refusal lands on the shelf underneath,
    /// and nothing retries.
    Stream(Box<HostRow>),
}

/// Host-side models and the command bus. Built before [`Console`]: handles are `Clone` +
/// thread-safe so the host can keep them on one thread and build the (not `Send`) console
/// on the draw thread.
#[derive(Clone, Default)]
pub struct ConsoleHandles {
    pub console: ConsoleShared,
    pub library: crate::library::LibraryShared,
    pub bus: ConsoleBus,
}

impl ConsoleHandles {
    pub fn new() -> ConsoleHandles {
        ConsoleHandles::default()
    }
}

/// Safe-area insets in device pixels. Chrome stays inside; the backdrop still paints
/// edge to edge. Zero on desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub insets: Insets,
    /// Device pixels per design unit. `None` uses `(height / 800).clamp(0.75, 3.0)`
    /// (Deck 1×, 4K TV 2.7×). A phone in hand should pass a density floor.
    pub scale: Option<f64>,
}

impl Viewport {
    pub fn plain(width: u32, height: u32) -> Viewport {
        Viewport {
            width,
            height,
            insets: Insets::default(),
            scale: None,
        }
    }
}

/// No input for this long: the console is being looked at, not used, and a host may
/// draw it at a reduced rate. Any input restores full rate on its own frame.
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(60);

/// What console frames cost, closed once a [`FrameCost::WINDOW`]. Time the draw and its
/// flush, never the swap: a swap blocks on vsync and always reads as one panel period.
#[derive(Default)]
pub struct FrameCost {
    frames: u32,
    sum: Duration,
    peak: Duration,
    since: Option<Instant>,
}

/// One closed [`FrameCost`] window.
#[derive(Debug, PartialEq)]
pub struct FrameReport {
    pub frames: u32,
    pub window: Duration,
    pub mean_ms: f64,
    pub peak_ms: f64,
}

impl FrameCost {
    /// One line a minute is cheap enough to leave on for everyone.
    pub const WINDOW: Duration = Duration::from_secs(60);

    /// Adds one frame's cost; returns the window once it has run [`Self::WINDOW`].
    pub fn add(&mut self, cost: Duration, now: Instant) -> Option<FrameReport> {
        let since = *self.since.get_or_insert(now);
        self.frames += 1;
        self.sum += cost;
        self.peak = self.peak.max(cost);
        let window = now.duration_since(since);
        if window < Self::WINDOW {
            return None;
        }
        let report = FrameReport {
            frames: self.frames,
            window,
            mean_ms: self.sum.as_secs_f64() * 1000.0 / f64::from(self.frames),
            peak_ms: self.peak.as_secs_f64() * 1000.0,
        };
        *self = FrameCost::default();
        Some(report)
    }
}

pub struct Console {
    shell: Shell,
    fonts: Fonts,
}

impl Console {
    /// Not `Send` (Skia). Build on the thread that will draw.
    pub fn new(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
        handles: &ConsoleHandles,
    ) -> Result<Console> {
        let stream = stream_intent(&entry);
        let fetch = entry_fetch(&entry);
        let stack = entry_stack(entry, &handles.library);
        if let Some(cmd) = fetch {
            handles.bus.send(cmd);
        }
        let mut shell = Shell::new(
            handles.console.clone(),
            handles.library.clone(),
            handles.bus.clone(),
            opts,
            stack,
        )?;
        if let Some(intent) = stream {
            shell.start_connect(intent);
        }
        let fonts = crate::theme::build_fonts()?;
        Ok(Console { shell, fonts })
    }

    /// `pad` is the chip label; `None` means no controller.
    pub fn frame(
        &mut self,
        canvas: &Canvas,
        viewport: &Viewport,
        pad: Option<&str>,
        pad_pref: Option<GamepadPref>,
        pads: &[PadInfo],
    ) {
        self.shell
            .render_in(canvas, viewport, &self.fonts, pad, pad_pref, pads);
    }

    pub fn menu(&mut self, event: MenuEvent, source: InputSource) -> Option<MenuPulse> {
        self.shell.note_input_source(source);
        self.shell.handle_menu(event)
    }

    /// Pointer in surface pixels; the shell subtracts insets.
    pub fn pointer(&mut self, input: PointerInput) -> bool {
        self.shell.pointer_input(input)
    }

    pub fn key(&mut self, key: Key, shift: bool, repeat: bool) -> bool {
        self.shell.key(key, shift, repeat)
    }

    pub fn text(&mut self, text: &str) {
        self.shell.text_input(text);
    }

    /// True while a field is being edited: keep IME / SDL text-input started, and
    /// route printable keys as text, not [`Key`]s.
    pub fn editing(&self) -> bool {
        self.shell.editing()
    }

    pub fn session_phase(&mut self, phase: SessionPhase) {
        self.shell.session_phase(phase);
    }

    /// Drain after every input and every frame.
    pub fn take_action(&mut self) -> Option<OverlayAction> {
        self.shell.take_action()
    }

    /// What a screen reader should speak for the focused row: its label, then its value.
    ///
    /// Poll it and speak only when the string changes — a reader that repeats itself is
    /// worse than silence. `None` is a focus this driver does not describe (or a takeover
    /// holding the input), and the host then says nothing at all.
    pub fn focus_announcement(&mut self) -> Option<String> {
        self.shell.focus_announcement()
    }

    /// No menu, pointer, key or text input for `IDLE_AFTER` (a minute).
    pub fn idle(&self) -> bool {
        self.shell.idle()
    }

    /// Console is off screen; the shell keeps its stack for return.
    pub fn in_stream(&self) -> bool {
        self.shell.in_stream
    }

    /// Replace the stack with `entry` (deep link, or return to the shelf a game launched from).
    /// A return to the shelf already on top keeps it: the stack outlives the stream, and a
    /// rebuilt shelf starts empty and fetches again.
    pub fn navigate(&mut self, entry: ConsoleEntry) {
        if already_showing(self.shell.top(), &entry) {
            return;
        }
        let stream = stream_intent(&entry);
        let fetch = entry_fetch(&entry);
        let stack = entry_stack(entry, self.shell.library());
        if let Some(cmd) = fetch {
            self.shell.send_cmd(cmd);
        }
        self.shell.replace_stack(stack);
        if let Some(intent) = stream {
            self.shell.start_connect(intent);
        }
    }

    /// Skia resource-cache budget for the host `DirectContext`. The shell only carries it.
    pub fn gpu_cache_bytes(&self) -> usize {
        self.shell.gpu_cache_bytes
    }

    /// Shell and fonts for the Vulkan overlay: stream chrome uses the same fonts; the
    /// overlay holds the shell as `Option`.
    #[cfg(all(any(target_os = "linux", windows), feature = "vulkan-overlay"))]
    pub(crate) fn into_parts(self) -> (Shell, Fonts) {
        (self.shell, self.fonts)
    }
}

/// The desktop connect a [`ConsoleEntry::Stream`] carries. Built from the entry's own
/// row, never from `Shell::hosts` — that list is empty until the first `sync()`. Same
/// shape as the Options screen's "Connect to X": no launch, no preset override.
fn stream_intent(entry: &ConsoleEntry) -> Option<crate::screens::ConnectIntent> {
    let ConsoleEntry::Stream(host) = entry else {
        return None;
    };
    Some(crate::screens::ConnectIntent {
        addr: host.addr.clone(),
        port: host.port,
        fp_hex: host.fp_hex.clone(),
        launch: None,
        title: host.name.clone(),
        request_access: false,
        preset: None,
    })
}

/// The fetch a shelf entry needs. Nothing else loads a pushed shelf, so send it after
/// [`entry_stack`] snapshots the epoch.
fn entry_fetch(entry: &ConsoleEntry) -> Option<ConsoleCmd> {
    let (ConsoleEntry::Library(host) | ConsoleEntry::Stream(host)) = entry else {
        return None;
    };
    Some(ConsoleCmd::FetchLibrary {
        addr: host.addr.clone(),
        mgmt: host.mgmt_port,
        fp_hex: host.fp_hex.clone(),
    })
}

fn entry_stack(entry: ConsoleEntry, library: &crate::library::LibraryShared) -> Vec<Screen> {
    match entry {
        ConsoleEntry::Home => vec![Screen::Home(crate::screens::home::HomeScreen::new())],
        ConsoleEntry::Library(host) | ConsoleEntry::Stream(host) => vec![
            Screen::Home(crate::screens::home::HomeScreen::new()),
            // Snapshot the model's fetch epoch so the host's following `FetchLibrary`
            // is the first raise; that is how the shelf knows the result is its own.
            Screen::Library(crate::screens::library::LibraryScreen::new(
                &host,
                library.fetch_epoch(),
            )),
        ],
    }
}

/// A [`ConsoleEntry::Library`] whose shelf is `top`. A `Stream` entry always re-roots:
/// it carries a connect the caller expects to start.
fn already_showing(top: Option<&Screen>, entry: &ConsoleEntry) -> bool {
    match (top, entry) {
        (Some(Screen::Library(lib)), ConsoleEntry::Library(host)) => lib.shelf_of(host),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_cost_closes_once_a_window() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = FrameCost::default();
        assert_eq!(c.add(ms(2), t0), None);
        assert_eq!(c.add(ms(6), t0 + Duration::from_secs(30)), None);
        let r = c.add(ms(4), t0 + FrameCost::WINDOW).expect("window closed");
        assert_eq!((r.frames, r.window), (3, FrameCost::WINDOW));
        assert!((r.mean_ms - 4.0).abs() < 1e-9 && (r.peak_ms - 6.0).abs() < 1e-9);
        // The next frame opens a fresh window.
        assert_eq!(c.add(ms(1), t0 + FrameCost::WINDOW * 2), None);
    }

    fn row() -> HostRow {
        HostRow {
            key: "aa".into(),
            id: Some("rec-1".into()),
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_preset: None,
            running: String::new(),
            game_presets: Default::default(),
        }
    }

    /// Stream is the Library stack plus a desktop connect. Nothing else raises one,
    /// and the connect launches no title and overrides no preset.
    #[test]
    fn only_a_stream_entry_carries_a_connect() {
        assert!(stream_intent(&ConsoleEntry::Home).is_none());
        assert!(stream_intent(&ConsoleEntry::Library(Box::new(row()))).is_none());

        let intent =
            stream_intent(&ConsoleEntry::Stream(Box::new(row()))).expect("a desktop connect");
        assert_eq!(intent.addr, "10.0.0.5");
        assert_eq!(intent.launch, None);
        assert_eq!(intent.preset, None);
        assert!(!intent.request_access);
        assert_eq!(intent.title, "Desk");
    }

    /// A shelf entry loads its own library; the host list fetches nothing.
    #[test]
    fn a_shelf_entry_carries_its_fetch() {
        assert!(entry_fetch(&ConsoleEntry::Home).is_none());
        for entry in [
            ConsoleEntry::Library(Box::new(row())),
            ConsoleEntry::Stream(Box::new(row())),
        ] {
            let Some(ConsoleCmd::FetchLibrary { addr, mgmt, fp_hex }) = entry_fetch(&entry) else {
                panic!("a shelf entry fetches its library");
            };
            assert_eq!(
                (addr.as_str(), mgmt, fp_hex.as_str()),
                ("10.0.0.5", 47990, "aa")
            );
        }
    }

    /// Both host entries land on the same two screens, so B leaves a cancelled stream
    /// on the shelf rather than on the host list.
    #[test]
    fn a_stream_entry_opens_the_same_stack_as_library() {
        let library = crate::library::LibraryShared::default();
        for entry in [
            ConsoleEntry::Library(Box::new(row())),
            ConsoleEntry::Stream(Box::new(row())),
        ] {
            let stack = entry_stack(entry, &library);
            assert!(matches!(
                stack.as_slice(),
                [Screen::Home(_), Screen::Library(_)]
            ));
        }
    }

    /// Returning to the shelf on top keeps it (its posters survive the stream); another
    /// host's shelf, a Stream entry, or a Home top re-roots as before.
    #[test]
    fn a_library_entry_for_the_shelf_on_top_is_a_no_op() {
        let library = crate::library::LibraryShared::default();
        let stack = entry_stack(ConsoleEntry::Library(Box::new(row())), &library);
        let top = stack.last();
        assert!(already_showing(
            top,
            &ConsoleEntry::Library(Box::new(row()))
        ));
        assert!(!already_showing(
            top,
            &ConsoleEntry::Stream(Box::new(row()))
        ));
        let other = HostRow {
            key: "bb".into(),
            ..row()
        };
        assert!(!already_showing(
            top,
            &ConsoleEntry::Library(Box::new(other))
        ));
        assert!(!already_showing(
            stack.first(),
            &ConsoleEntry::Library(Box::new(row()))
        ));
    }
}
