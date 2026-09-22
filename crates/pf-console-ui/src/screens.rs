//! Console screens and the shared contract they answer.
//!
//! Each screen owns its focus and content. [`crate::shell::Shell`] owns the stack,
//! transitions, chrome, and overlays. A screen never draws its own background or
//! hint bar, so every screen animates and reads the same way.

pub(crate) mod add_host;
pub(crate) mod bind_preset;
pub(crate) mod collections;
pub(crate) mod controllers;
pub(crate) mod home;
pub(crate) mod library;

pub(crate) mod options;
pub(crate) mod pair;
pub(crate) mod pin_hosts;
pub(crate) mod ring_editor;
pub(crate) mod settings;
pub(crate) mod shortcut_editor;

/// Alias home still opens by. Same type as [`options::OptionsScreen`].
pub(crate) mod host_options {
    pub(crate) use super::options::OptionsScreen as HostOptionsScreen;

    impl HostOptionsScreen {
        pub(crate) fn new(host: &crate::model::HostRow) -> HostOptionsScreen {
            HostOptionsScreen::for_host(host)
        }
    }
}

use crate::glyphs::Hint;
use crate::library::LibraryShared;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::theme::Fonts;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::{menu_nav::PadInfo, trust};
use skia_safe::{Canvas, Rect};

/// Backdrop the shell crossfades on push/pop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bg {
    Aurora,
    /// Same mesh as [`Bg::Aurora`], calmed. The shell chases one `calm` uniform — not a second shader.
    Form,
}

/// Per-event screen context. `settings` is mut — the settings screen persists in place.
pub struct Ctx<'a> {
    pub hosts: &'a [HostRow],
    /// Live library slot; the top screen owns it.
    pub library: &'a LibraryShared,
    pub settings: &'a mut trust::Settings,
    /// Persistence for `settings` and the preset catalog. `load` immediately before a
    /// mutation (rebase), then `save`.
    pub store: &'a dyn crate::store::SettingsStore,
    pub platform: crate::platform::Platform,
    /// This device's own screen ([`crate::shell::ConsoleOptions::screen`]).
    pub screen: Option<crate::shell::DeviceScreen>,
    pub pads: &'a [PadInfo],
    /// Steam Deck: never draw our keyboard — Steam's types via SDL text input.
    pub deck: bool,
    /// Host has a fallback UI ([`crate::shell::ConsoleOptions::fallback_ui`]); gates the
    /// console-off row.
    pub fallback_ui: bool,
    /// This device decodes PyroWave ([`crate::shell::ConsoleOptions::pyrowave_ok`]).
    /// False marks the codec row's PyroWave value unsupported.
    pub pyrowave_ok: bool,
    /// This device decodes AV1 in hardware ([`crate::shell::ConsoleOptions::av1_ok`]).
    /// False marks the codec row's AV1 value unsupported: the Hello never asks for it.
    pub av1_ok: bool,
    /// Name the host stores this client under when pairing.
    pub device_name: &'a str,
    /// Shell clock in seconds (spinners, pulses).
    pub t: f64,
}

/// Session the shell turns into `OverlayAction::Launch` plus the connecting overlay.
pub(crate) struct ConnectIntent {
    pub addr: String,
    pub port: u16,
    pub fp_hex: String,
    /// Library title id; `None` streams the desktop.
    pub launch: Option<String>,
    pub title: String,
    /// No-PIN delegated approval. The shell shows "waiting for approval" instead of
    /// "connecting" and parks on a long budget until the host lets this client in.
    pub request_access: bool,
    /// One-off preset for this launch; `None` keeps the host's default binding.
    pub preset: Option<String>,
}

pub(crate) enum Nav {
    Push(Box<Screen>),
    /// Pop this screen; popping the root quits the console.
    Pop,
    /// Swap in place, animated as a push. Pop+push would leave the old menu on the
    /// stack, so Back from the editor would describe the host as it was before the edit.
    Replace(Box<Screen>),
}

/// Per-event asks of the shell, applied after dispatch — no re-entrant stack mutation.
#[derive(Default)]
pub(crate) struct Outbox {
    pub nav: Option<Nav>,
    pub connect: Option<ConnectIntent>,
    pub cmds: Vec<ConsoleCmd>,
    pub toast: Option<String>,
    /// Clipboard text. Rides the run loop, not the command bus: SDL owns the clipboard.
    pub copy: Option<String>,
}

impl Outbox {
    pub(crate) fn push(&mut self, screen: Screen) {
        self.nav = Some(Nav::Push(Box::new(screen)));
    }

    pub(crate) fn pop(&mut self) {
        self.nav = Some(Nav::Pop);
    }

    pub(crate) fn replace(&mut self, screen: Screen) {
        self.nav = Some(Nav::Replace(Box::new(screen)));
    }

    /// Raise the context menu on the focused subject. The screen names the subject
    /// ([`options::OptionsScreen::for_host`], [`options::OptionsScreen::for_game`]);
    /// the menu owns the verbs.
    pub(crate) fn options(&mut self, menu: options::OptionsScreen) {
        self.push(Screen::HostOptions(menu));
    }
}

/// A saved host's `punktfunk://` link from the store (fingerprint + stable id a
/// screen does not hold). `launch` makes a game's link. `None` if the host has
/// left the store since the menu opened. The row's pin names the record; an unpinned
/// row is the placeholder at its address, never a host pinned there.
pub(crate) fn saved_host_link(
    store: &dyn crate::store::SettingsStore,
    fp_hex: &str,
    addr: &str,
    port: u16,
    preset: Option<&str>,
    launch: Option<&str>,
) -> Option<String> {
    let known = store.known_hosts();
    let host = known.resolve(Some(fp_hex), addr, port)?;
    Some(pf_client_core::deeplink::DeepLink::for_host(host, launch, preset).to_url())
}

pub(crate) fn host_link(store: &dyn crate::store::SettingsStore, row: &HostRow) -> Option<String> {
    saved_host_link(
        store,
        &row.fp_hex,
        &row.addr,
        row.port,
        row.pin.as_ref().map(|p| p.id.as_str()),
        None,
    )
}

pub(crate) enum Screen {
    Home(home::HomeScreen),
    Library(library::LibraryScreen),
    Collections(collections::CollectionsScreen),
    Settings(settings::SettingsScreen),
    AddHost(add_host::AddHostScreen),
    Pair(pair::PairScreen),
    PinHosts(pin_hosts::PinHostsScreen),
    /// Which preset the host's primary tile connects with.
    BindPreset(bind_preset::BindPresetScreen),
    /// Attached pads. Android-only — the settings row that opens it is not on desktop.
    Controllers(controllers::ControllersScreen),
    /// In-stream ring, editing mode. Raised by the Quick actions settings row.
    RingEditor(Box<ring_editor::RingEditorScreen>),
    ShortcutEditor(shortcut_editor::ShortcutEditorScreen),
    HostOptions(options::OptionsScreen),
}

impl Screen {
    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match self {
            Screen::Home(s) => s.menu(ev, ctx, fx),
            Screen::Library(s) => s.menu(ev, ctx, fx),
            Screen::Collections(s) => s.menu(ev, ctx, fx),
            Screen::Settings(s) => s.menu(ev, ctx, fx),
            Screen::AddHost(s) => s.menu(ev, ctx, fx),
            Screen::RingEditor(s) => s.menu(ev, ctx, fx),
            Screen::ShortcutEditor(s) => s.menu(ev, ctx, fx),
            Screen::Pair(s) => s.menu(ev, ctx, fx),
            Screen::PinHosts(s) => s.menu(ev, ctx, fx),
            Screen::BindPreset(s) => s.menu(ev, ctx, fx),
            Screen::Controllers(s) => s.menu(ev, ctx, fx),
            Screen::HostOptions(s) => s.menu(ev, ctx, fx),
        }
    }

    /// Takes finger pans: its list is on the element tree. Any other screen scrolls a
    /// drag by ticks.
    pub(crate) fn pans(&self) -> bool {
        matches!(self, Screen::Settings(_))
    }

    /// Mouse/touch in device pixels. `true` if the point landed on this screen's
    /// furniture, even when the press is a no-op — a stray tap must not fall through.
    /// `false` only for the empty backdrop.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match self {
            Screen::Home(s) => s.pointer(p, ctx, fx),
            Screen::Library(s) => s.pointer(p, ctx, fx),
            Screen::Collections(s) => s.pointer(p, ctx, fx),
            Screen::Settings(s) => s.pointer(p, ctx, fx),
            Screen::AddHost(s) => s.pointer(p, ctx, fx),
            Screen::RingEditor(s) => s.pointer(p, ctx, fx),
            Screen::ShortcutEditor(s) => s.pointer(p, ctx, fx),
            Screen::Pair(s) => s.pointer(p, ctx, fx),
            Screen::PinHosts(s) => s.pointer(p, ctx, fx),
            Screen::BindPreset(s) => s.pointer(p, ctx, fx),
            Screen::Controllers(s) => s.pointer(p, ctx, fx),
            Screen::HostOptions(s) => s.pointer(p, ctx, fx),
        }
    }

    /// SDL `TextInput` (hardware keyboards; Steam's keyboard under gamescope).
    pub(crate) fn text_input(&mut self, text: &str) {
        match self {
            Screen::AddHost(s) => s.text_input(text),
            Screen::ShortcutEditor(s) => s.text_input(text),
            Screen::Pair(s) => s.text_input(text),
            Screen::Settings(s) => s.text_input(text),
            _ => {}
        }
    }

    /// Raw key while a field is editing (Backspace repeats, Return = done).
    /// Takes `ctx` because the settings screen commits the typed bitrate on close.
    pub(crate) fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        match self {
            Screen::AddHost(s) => s.edit_key(key),
            Screen::ShortcutEditor(s) => s.edit_key(key),
            Screen::Pair(s) => s.edit_key(key),
            Screen::Settings(s) => s.edit_key(key, ctx),
            _ => false,
        }
    }

    /// A text field is open — the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        match self {
            Screen::AddHost(s) => s.editing(),
            Screen::ShortcutEditor(s) => s.editing(),
            Screen::Pair(s) => s.editing(),
            Screen::Settings(s) => s.editing(),
            _ => false,
        }
    }

    pub(crate) fn background(&self) -> Bg {
        match self {
            Screen::Home(_) | Screen::Library(_) | Screen::Collections(_) => Bg::Aurora,
            _ => Bg::Form,
        }
    }

    pub(crate) fn title(&self, _ctx: &Ctx) -> String {
        match self {
            Screen::Home(_) => "Select a Host".into(),
            Screen::Library(s) => s.title(),
            Screen::Collections(s) => s.title(),
            Screen::Settings(_) => "Settings".into(),
            Screen::AddHost(s) => s.title(),
            Screen::RingEditor(s) => s.title(),
            Screen::ShortcutEditor(s) => s.title(),
            Screen::Pair(s) => format!("Pair with {}", s.host_name()),
            Screen::PinHosts(s) => format!("Pin \u{201c}{}\u{201d}", s.preset_name()),
            Screen::BindPreset(s) => s.heading(),
            Screen::Controllers(_) => "Connected controllers".into(),
            Screen::HostOptions(s) => s.title(),
        }
    }

    /// What a screen reader should speak for whatever this screen has focused.
    /// `None` where a screen does not answer: silence beats naming the wrong row.
    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        match self {
            Screen::Home(s) => s.announcement(ctx.hosts),
            Screen::Library(s) => s.announcement(),
            Screen::Settings(s) => s.announcement(ctx),
            _ => None,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        match self {
            Screen::Home(s) => s.hints(ctx),
            Screen::Library(s) => s.hints(ctx),
            Screen::Collections(s) => s.hints(ctx),
            Screen::Settings(s) => s.hints(ctx),
            Screen::AddHost(s) => s.hints(ctx),
            Screen::RingEditor(s) => s.hints(ctx),
            Screen::ShortcutEditor(s) => s.hints(ctx),
            Screen::Pair(s) => s.hints(ctx),
            Screen::PinHosts(s) => s.hints(ctx),
            Screen::BindPreset(s) => s.hints(ctx),
            Screen::Controllers(s) => s.hints(ctx),
            Screen::HostOptions(s) => s.hints(ctx),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        match self {
            Screen::Home(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Library(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Collections(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Settings(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::AddHost(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::RingEditor(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::ShortcutEditor(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Pair(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PinHosts(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::BindPreset(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Controllers(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::HostOptions(s) => s.render(canvas, rect, k, dt, fonts, ctx),
        }
    }
}
