//! Console screens and the shared contract they answer.
//!
//! Each screen owns its focus and content. [`crate::shell::Shell`] owns the stack,
//! transitions, chrome, and overlays. A screen never draws its own background or
//! hint bar, so every screen animates and reads the same way.

pub(crate) mod add_host;
pub(crate) mod bind_preset;
pub(crate) mod card_menu;
pub(crate) mod collections;
pub(crate) mod grants;
pub(crate) mod home;
pub(crate) mod input_test;
pub(crate) mod library;
pub(crate) mod licenses;

pub(crate) mod pair;
pub(crate) mod palette;
pub(crate) mod pin_hosts;
pub(crate) mod players;
pub(crate) mod preset;
pub(crate) mod prompt;
pub(crate) mod ring_editor;
pub(crate) mod search;
pub(crate) mod settings;
pub(crate) mod shortcut_editor;

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
    /// A TV: no clipboard to copy to, no phone sensors ([`crate::shell::ConsoleOptions::tv`]).
    pub tv: bool,
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

/// The text field a screen has open, for a host whose own keyboard types into it
/// (an Apple TV's, where iPhone typing and dictation live).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EditField {
    pub label: String,
    pub text: String,
    /// Digits only: a PIN, a port, a bitrate.
    pub digits: bool,
}

impl EditField {
    fn new(label: &str, text: &str, digits: bool) -> Option<EditField> {
        Some(EditField {
            label: label.into(),
            text: text.into(),
            digits,
        })
    }
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
    /// Switch to this tab (a pad shortcut).
    pub tab: Option<crate::shell::Tab>,
    /// Browse games: focus the games under the Hosts row.
    pub browse: bool,
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
    /// ([`card_menu::CardMenu::for_host`], [`card_menu::CardMenu::for_game`]);
    /// the menu owns the verbs.
    pub(crate) fn options(&mut self, menu: card_menu::CardMenu) {
        self.push(Screen::CardMenu(menu));
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
    Settings(settings::SettingsScreen),
    AddHost(add_host::AddHostScreen),
    Pair(pair::PairScreen),
    PinHosts(pin_hosts::PinHostsScreen),
    /// Which preset the host's primary tile connects with.
    BindPreset(bind_preset::BindPresetScreen),
    /// The Controllers tab: attached pads, and the platform's grants and tests.
    Players(players::PlayersScreen),
    /// In-stream ring, editing mode. Raised by the Quick actions settings row.
    RingEditor(Box<ring_editor::RingEditorScreen>),
    ShortcutEditor(shortcut_editor::ShortcutEditorScreen),
    CardMenu(card_menu::CardMenu),
    /// The Games tab's sections: order and switches.
    Customize(library::CustomizeScreen),
    /// The Background row's cards. Raised by the Interface section.
    Palette(palette::PaletteScreen),
    /// Controller grants and tests. Raised by the Controllers tab's last card.
    Grants(grants::GrantsScreen),
    /// A question the app asks through the console ([`prompt::Prompt`]).
    Prompt(prompt::PromptScreen),
    /// Open-source licences. Raised by the About section.
    Licenses(licenses::LicensesScreen),
    /// A title search over one host's shelf. Raised by the shelf's Search pill.
    Search(search::SearchScreen),
    /// A preset's menu: edit, rename, pin, delete. Raised by its Presets row.
    PresetMenu(preset::PresetMenu),
    /// A preset's name, new or renamed.
    PresetName(preset::PresetName),
    /// A preset's settings over the global ones.
    PresetEdit(preset::PresetEdit),
    /// The live controller test. Raised by the Controllers tab's Test card.
    InputTest(input_test::InputTestScreen),
}

impl Screen {
    /// The shelf a launch leaves from: the Games tab's, or the games under the Hosts row.
    pub(crate) fn shelf(&self) -> Option<&library::LibraryScreen> {
        match self {
            Screen::Library(l) => Some(l),
            Screen::Home(h) => h.shelf(),
            _ => None,
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match self {
            Screen::Home(s) => s.menu(ev, ctx, fx),
            Screen::Library(s) => s.menu(ev, ctx, fx),
            Screen::Settings(s) => s.menu(ev, ctx, fx),
            Screen::AddHost(s) => s.menu(ev, ctx, fx),
            Screen::RingEditor(s) => s.menu(ev, ctx, fx),
            Screen::ShortcutEditor(s) => s.menu(ev, ctx, fx),
            Screen::Pair(s) => s.menu(ev, ctx, fx),
            Screen::PinHosts(s) => s.menu(ev, ctx, fx),
            Screen::BindPreset(s) => s.menu(ev, ctx, fx),
            Screen::Players(s) => s.menu(ev, ctx, fx),
            Screen::CardMenu(s) => s.menu(ev, ctx, fx),
            Screen::Customize(s) => s.menu(ev, ctx, fx),
            Screen::Palette(s) => s.menu(ev, ctx, fx),
            Screen::Grants(s) => s.menu(ev, ctx, fx),
            Screen::Prompt(s) => s.menu(ev, ctx, fx),
            Screen::Licenses(s) => s.menu(ev, ctx, fx),
            Screen::Search(s) => s.menu(ev, ctx, fx),
            Screen::PresetMenu(s) => s.menu(ev, ctx, fx),
            Screen::PresetName(s) => s.menu(ev, ctx, fx),
            Screen::PresetEdit(s) => s.menu(ev, ctx, fx),
            Screen::InputTest(s) => s.menu(ev, ctx, fx),
        }
    }

    /// Focus arrives from the shell's tabs above: a screen with its own strip lands there,
    /// the next thing down.
    pub(crate) fn enter_from_top(&mut self) {
        if let Screen::Settings(s) = self {
            s.enter_from_top();
        }
    }

    /// OK went down on a remote: what has focus dips now, before the release acts on it.
    pub(crate) fn press(&mut self) {
        match self {
            Screen::Home(s) => s.press(),
            Screen::Library(s) => s.press(),
            Screen::Settings(s) => s.press(),
            Screen::AddHost(s) => s.list.dip(),
            Screen::Pair(s) => s.list.dip(),
            Screen::PinHosts(s) => s.list.dip(),
            Screen::BindPreset(s) => s.list.dip(),
            Screen::CardMenu(s) => s.press(),
            Screen::Customize(s) => s.list.dip(),
            Screen::Palette(s) => s.press(),
            Screen::Grants(s) => s.list.dip(),
            Screen::Prompt(s) => s.list.dip(),
            Screen::Search(s) => s.list.dip(),
            Screen::PresetMenu(s) => s.list.dip(),
            Screen::PresetName(s) => s.list.dip(),
            Screen::PresetEdit(s) => s.list.dip(),
            _ => {}
        }
    }

    /// A finger drag, offered to what the screen scrolls: its menu list, or the library
    /// grid. `false` scrolls it by ticks, as on the carousels or over a keyboard tray.
    pub(crate) fn pan(&mut self, p: Pointer) -> bool {
        if self.editing() {
            return false;
        }
        match self {
            Screen::Settings(s) => s.list.pan(p),
            Screen::AddHost(s) => s.list.pan(p),
            Screen::Pair(s) => s.list.pan(p),
            Screen::PinHosts(s) => s.list.pan(p),
            Screen::BindPreset(s) => s.list.pan(p),
            Screen::CardMenu(s) => s.list.pan(p),
            Screen::Customize(s) => s.list.pan(p),
            Screen::Palette(s) => s.pan(p),
            Screen::Grants(s) => s.list.pan(p),
            Screen::Prompt(s) => s.list.pan(p),
            Screen::Licenses(s) => s.pan(p),
            Screen::Search(s) => s.list.pan(p),
            Screen::PresetMenu(s) => s.list.pan(p),
            Screen::PresetName(s) => s.list.pan(p),
            Screen::PresetEdit(s) => s.list.pan(p),
            Screen::ShortcutEditor(s) => s.pan_list().is_some_and(|l| l.pan(p)),
            Screen::RingEditor(s) => s.pan_list().pan(p),
            Screen::Library(s) => s.pan(p),
            Screen::Home(s) => s.pan(p),
            Screen::Players(_) | Screen::InputTest(_) => false,
        }
    }

    /// Mouse/touch in device pixels. `true` if the point landed on this screen's
    /// furniture, even when the press is a no-op — a stray tap must not fall through.
    /// `false` only for the empty backdrop.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match self {
            Screen::Home(s) => s.pointer(p, ctx, fx),
            Screen::Library(s) => s.pointer(p, ctx, fx),
            Screen::Settings(s) => s.pointer(p, ctx, fx),
            Screen::AddHost(s) => s.pointer(p, ctx, fx),
            Screen::RingEditor(s) => s.pointer(p, ctx, fx),
            Screen::ShortcutEditor(s) => s.pointer(p, ctx, fx),
            Screen::Pair(s) => s.pointer(p, ctx, fx),
            Screen::PinHosts(s) => s.pointer(p, ctx, fx),
            Screen::BindPreset(s) => s.pointer(p, ctx, fx),
            Screen::Players(s) => s.pointer(p, ctx, fx),
            Screen::CardMenu(s) => s.pointer(p, ctx, fx),
            Screen::Customize(s) => s.pointer(p, ctx, fx),
            Screen::Palette(s) => s.pointer(p, ctx, fx),
            Screen::Grants(s) => s.pointer(p, ctx, fx),
            Screen::Prompt(s) => s.pointer(p, ctx, fx),
            Screen::Licenses(s) => s.pointer(p, ctx, fx),
            Screen::Search(s) => s.pointer(p, ctx, fx),
            Screen::PresetMenu(s) => s.pointer(p, ctx, fx),
            Screen::PresetName(s) => s.pointer(p, ctx, fx),
            Screen::PresetEdit(s) => s.pointer(p, ctx, fx),
            Screen::InputTest(_) => true,
        }
    }

    /// SDL `TextInput` (hardware keyboards; Steam's keyboard under gamescope).
    pub(crate) fn text_input(&mut self, text: &str) {
        match self {
            Screen::AddHost(s) => s.text_input(text),
            Screen::ShortcutEditor(s) => s.text_input(text),
            Screen::Pair(s) => s.text_input(text),
            Screen::Search(s) => s.text_input(text),
            Screen::PresetName(s) => s.text_input(text),
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
            Screen::Search(s) => s.edit_key(key),
            Screen::PresetName(s) => s.edit_key(key),
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
            Screen::Search(s) => s.editing(),
            Screen::PresetName(s) => s.editing(),
            Screen::Settings(s) => s.editing(),
            _ => false,
        }
    }

    /// The field [`Self::editing`] has open.
    pub(crate) fn edit_field(&self) -> Option<EditField> {
        match self {
            Screen::AddHost(s) => s.edit_field(),
            Screen::ShortcutEditor(s) => s.edit_field(),
            Screen::Pair(s) => s.edit_field(),
            Screen::Settings(s) => s.edit_field(),
            Screen::Search(s) => s.edit_field(),
            Screen::PresetName(s) => s.edit_field(),
            _ => None,
        }
    }

    /// How far past the content's top and bottom edges the shell's trays reach in, px:
    /// the depth of a screen's own pinned chrome, so one ramp covers it with the band's.
    pub(crate) fn pinned(&self, k: f64) -> (f32, f32) {
        match self {
            Screen::Library(s) => s.pinned(k),
            Screen::Players(s) => s.pinned(k),
            Screen::Settings(s) => s.pinned(k),
            Screen::Grants(s) => s.pinned(k),
            _ => (0.0, 0.0),
        }
    }

    /// A screen's own pinned chrome, drawn by the shell over its trays after [`Self::render`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_pinned(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &Ctx,
    ) {
        match self {
            Screen::Library(s) => s.render_pinned(canvas, rect, k, fonts, ctx),
            Screen::Players(s) => s.render_pinned(canvas, rect, k, fonts, ctx),
            Screen::Settings(s) => s.render_pinned(canvas, rect, k, dt, fonts, ctx),
            Screen::Grants(s) => s.render_pinned(canvas, rect, k, fonts),
            _ => {}
        }
    }

    pub(crate) fn background(&self) -> Bg {
        match self {
            Screen::Home(_) | Screen::Library(_) | Screen::Players(_) => Bg::Aurora,
            _ => Bg::Form,
        }
    }

    pub(crate) fn title(&self, _ctx: &Ctx) -> String {
        match self {
            Screen::Home(_) => "Select a Host".into(),
            Screen::Library(s) => s.title(),
            Screen::Settings(_) => "Settings".into(),
            Screen::AddHost(s) => s.title(),
            Screen::RingEditor(s) => s.title(),
            Screen::ShortcutEditor(s) => s.title(),
            Screen::Pair(s) => format!("Pair with {}", s.host_name()),
            Screen::PinHosts(s) => format!("Pin \u{201c}{}\u{201d}", s.preset_name()),
            Screen::BindPreset(s) => s.heading(),
            Screen::Players(_) => "Controllers".into(),
            Screen::CardMenu(s) => s.title(),
            Screen::Customize(_) => "Customize".into(),
            Screen::Palette(_) => "Background".into(),
            Screen::Grants(_) => "Controller access".into(),
            Screen::Prompt(s) => s.title(),
            Screen::Licenses(_) => "Open-source licences".into(),
            Screen::Search(s) => s.title(),
            Screen::PresetMenu(s) => s.title(),
            Screen::PresetName(s) => s.title(),
            Screen::PresetEdit(s) => s.title(),
            Screen::InputTest(_) => "Controller test".into(),
        }
    }

    /// What a screen reader should speak for whatever this screen has focused.
    /// `None` where a screen does not answer: silence beats naming the wrong row.
    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        match self {
            Screen::Home(s) => s.announcement(ctx),
            Screen::Library(s) => s.announcement(ctx),
            Screen::Customize(s) => s.announcement(ctx),
            Screen::Palette(s) => s.announcement(ctx),
            Screen::Grants(s) => s.announcement(),
            Screen::Prompt(s) => s.announcement(),
            Screen::Settings(s) => s.announcement(ctx),
            Screen::Players(s) => s.announcement(ctx),
            _ => None,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        match self {
            Screen::Home(s) => s.hints(ctx),
            Screen::Library(s) => s.hints(ctx),
            Screen::Settings(s) => s.hints(ctx),
            Screen::AddHost(s) => s.hints(ctx),
            Screen::RingEditor(s) => s.hints(ctx),
            Screen::ShortcutEditor(s) => s.hints(ctx),
            Screen::Pair(s) => s.hints(ctx),
            Screen::PinHosts(s) => s.hints(ctx),
            Screen::BindPreset(s) => s.hints(ctx),
            Screen::Players(s) => s.hints(ctx),
            Screen::CardMenu(s) => s.hints(ctx),
            Screen::Customize(s) => s.hints(ctx),
            Screen::Palette(s) => s.hints(ctx),
            Screen::Grants(s) => s.hints(ctx),
            Screen::Prompt(s) => s.hints(ctx),
            Screen::Licenses(s) => s.hints(ctx),
            Screen::Search(s) => s.hints(ctx),
            Screen::PresetMenu(s) => s.hints(ctx),
            Screen::PresetName(s) => s.hints(ctx),
            Screen::PresetEdit(s) => s.hints(ctx),
            Screen::InputTest(s) => s.hints(ctx),
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
            // The shelf view draws its focus outside el: titles to walk are its targets.
            Screen::Library(s) => {
                s.render(canvas, rect, k, dt, fonts, ctx);
                crate::el::claim(usize::from(s.has_titles()));
            }
            Screen::Settings(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::AddHost(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::RingEditor(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::ShortcutEditor(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Pair(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PinHosts(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::BindPreset(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Players(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::CardMenu(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Customize(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Palette(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Grants(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Prompt(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Licenses(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Search(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PresetMenu(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PresetName(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PresetEdit(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::InputTest(s) => s.render(canvas, rect, k, dt, fonts, ctx),
        }
    }
}
