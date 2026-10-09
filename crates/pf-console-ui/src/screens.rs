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
pub(crate) mod profiles;
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
use std::borrow::Cow;

/// Backdrop the shell crossfades on push/pop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bg {
    Aurora,
    /// Same mesh as [`Bg::Aurora`], calmed. The shell chases one `calm` uniform — not a second shader.
    Form,
}

/// This device, from [`crate::shell::ConsoleOptions`]. Fixed for the shell's life but for
/// `av1_ok`, which the overlay corrects before the first frame.
#[derive(Clone, Debug)]
pub struct Device {
    pub platform: crate::platform::Platform,
    /// This device's own screen ([`crate::shell::ConsoleOptions::screen`]).
    pub screen: Option<crate::shell::DeviceScreen>,
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
    /// The host answers profile fetches ([`crate::shell::ConsoleOptions::profiles`]).
    pub profiles: bool,
    /// Name the host stores this client under when pairing.
    pub name: String,
    /// The About row's version ([`crate::shell::ConsoleOptions::version`]).
    pub version: String,
}

/// Per-event screen context. `settings` is mut — the settings screen persists in place.
pub struct Ctx<'a> {
    pub hosts: &'a [HostRow],
    /// Live library slot; the top screen owns it.
    pub library: &'a LibraryShared,
    pub settings: &'a mut trust::Settings,
    /// Persistence for `settings` and the preset catalog. A settings change goes through
    /// [`Ctx::write`].
    pub store: &'a dyn crate::store::SettingsStore,
    pub pads: &'a [PadInfo],
    pub device: &'a Device,
    /// Shell clock in seconds (spinners, pulses).
    pub t: f64,
}

#[cfg(test)]
impl Device {
    /// A desktop that decodes everything, named `test`.
    pub(crate) fn test() -> Device {
        Device {
            platform: crate::platform::Platform::Desktop,
            screen: None,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            profiles: true,
            name: "test".into(),
            version: crate::VERSION.into(),
        }
    }
}

#[cfg(test)]
impl<'a> Ctx<'a> {
    /// [`Device::test`] over the file store, with no hosts or pads. Tests override the rest
    /// by struct update.
    pub(crate) fn test(settings: &'a mut trust::Settings, library: &'a LibraryShared) -> Ctx<'a> {
        static DESKTOP: std::sync::LazyLock<Device> = std::sync::LazyLock::new(Device::test);
        Ctx {
            hosts: &[],
            library,
            settings,
            store: crate::store::file_store(),
            pads: &[],
            device: &DESKTOP,
            t: 0.0,
        }
    }
}

impl Ctx<'_> {
    /// Rebases `settings` on the store, runs `f`, and saves when `f` reports a change.
    /// The store writes the whole file: a save without the rebase reverts another
    /// writer's. Returns what `f` returned.
    pub(crate) fn write(&mut self, f: impl FnOnce(&mut Self) -> bool) -> bool {
        *self.settings = self.store.load();
        let changed = f(self);
        if changed {
            self.store.save(self.settings);
        }
        changed
    }
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
#[derive(Clone, Debug)]
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
    /// The profile id to play as: the host card's pick. `None` names none.
    pub profile: Option<String>,
    /// Check the box's profiles before the dial (§10.1). `None` dials as it stands.
    pub ask: Option<ProfileAsk>,
    /// The picked profile's row, for the seat check before the dial (§9.2). `None` dials as it
    /// stands.
    pub seat: Option<Seated>,
}

/// A picked profile as the host last listed it, and where to reach the host's management lane.
#[derive(Clone, Debug)]
pub(crate) struct Seated {
    pub row: pf_client_core::profiles::ListedProfile,
    pub mgmt: u16,
}

/// What the profile check before a dial needs: the card's host and its saved pick.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProfileAsk {
    /// [`HostRow::host_key`]: where a pick is saved.
    pub key: String,
    pub name: String,
    pub mgmt: u16,
    pub saved: Option<pf_client_core::profiles::ProfilePick>,
}

impl ProfileAsk {
    /// A paired, pinned card's check; the rest have no profiles to ask about.
    pub(crate) fn of(h: &HostRow) -> Option<ProfileAsk> {
        (h.paired && !h.fp_hex.is_empty()).then(|| ProfileAsk {
            key: h.host_key().to_string(),
            name: h.name.clone(),
            mgmt: h.mgmt_port,
            saved: h.profile.clone(),
        })
    }
}

impl ConnectIntent {
    /// Stream `h`: launch `game` (id, title), else its desk. The takeover names the game,
    /// else what the host has up, else the host. A pinned card's preset rides along and
    /// adds its name to the title.
    pub(crate) fn to_host(h: &HostRow, game: Option<(&str, &str)>) -> ConnectIntent {
        let subject = match game {
            Some((_, title)) => title,
            None if !h.running.is_empty() => &h.running,
            None => &h.name,
        };
        ConnectIntent {
            addr: h.addr.clone(),
            port: h.port,
            fp_hex: h.fp_hex.clone(),
            launch: game.map(|(id, _)| id.to_string()),
            title: match &h.pin {
                Some(p) => format!("{subject} \u{b7} {}", p.name),
                None => subject.to_string(),
            },
            request_access: false,
            preset: h.pin.as_ref().map(|p| p.id.clone()),
            profile: h.profile.as_ref().map(|p| p.id.clone()),
            ask: ProfileAsk::of(h),
            seat: None,
        }
    }

    /// The same connect with `preset` in place of the pin's: Connect with….
    pub(crate) fn with_preset(self, preset: Option<String>) -> ConnectIntent {
        ConnectIntent { preset, ..self }
    }
}

pub(crate) enum Nav {
    Push(Box<Screen>),
    /// Pop this screen; popping the root focuses its tab, and the tab's Back asks to exit.
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
    /// Leave the console: the exit question's yes.
    pub quit: bool,
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
    pf_client_core::deeplink::saved_host_link(
        &store.known_hosts(),
        Some(fp_hex),
        addr,
        port,
        preset,
        launch,
    )
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
    /// Who plays on a host. Raised by Switch profile, and by a connect that needs a pick.
    Profiles(profiles::ProfilesScreen),
}

/// What every screen answers. The shell reaches the top screen's through [`Screen::view`]
/// and [`Screen::view_mut`]; the optional ones default to "nothing here".
pub(crate) trait ScreenView {
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse>;

    /// Mouse/touch in device pixels. `true` if the point landed on this screen's
    /// furniture, even when the press is a no-op — a stray tap must not fall through.
    /// `false` only for the empty backdrop.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool;

    fn hints(&self, ctx: &Ctx) -> Vec<Hint>;

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    );

    fn title(&self) -> String;

    /// OK went down on a remote: what has focus dips now, before the release acts on it.
    fn press(&mut self) {}

    /// A finger drag, offered to what the screen scrolls: its menu list, or the library
    /// grid. `false` scrolls it by ticks, as on the carousels or over a keyboard tray.
    fn pan(&mut self, _p: Pointer) -> bool {
        false
    }

    /// SDL `TextInput` (hardware keyboards; Steam's keyboard under gamescope).
    fn text_input(&mut self, _text: &str) {}

    /// Raw key while a field is editing (Backspace repeats, Return = done).
    /// Takes `ctx` because the settings screen commits the typed bitrate on close.
    fn edit_key(&mut self, _key: crate::input::Key, _ctx: &mut Ctx) -> bool {
        false
    }

    /// A text field is open — the run loop keeps SDL text input started.
    fn editing(&self) -> bool {
        false
    }

    /// The field [`Self::editing`] has open.
    fn edit_field(&self) -> Option<EditField> {
        None
    }

    /// One explainer line under the list, painted on the shell's bottom tray.
    fn foot(&self, _ctx: &Ctx) -> Option<Cow<'static, str>> {
        None
    }

    /// What a screen reader should speak for whatever this screen has focused.
    /// `None` where a screen does not answer: silence beats naming the wrong row.
    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        None
    }
}

impl Screen {
    pub(crate) fn view(&self) -> &dyn ScreenView {
        match self {
            Screen::Home(s) => s,
            Screen::Library(s) => s,
            Screen::Settings(s) => s,
            Screen::AddHost(s) => s,
            Screen::Pair(s) => s,
            Screen::PinHosts(s) => s,
            Screen::BindPreset(s) => s,
            Screen::Players(s) => s,
            Screen::RingEditor(s) => &**s,
            Screen::ShortcutEditor(s) => s,
            Screen::CardMenu(s) => s,
            Screen::Customize(s) => s,
            Screen::Palette(s) => s,
            Screen::Grants(s) => s,
            Screen::Prompt(s) => s,
            Screen::Licenses(s) => s,
            Screen::Search(s) => s,
            Screen::PresetMenu(s) => s,
            Screen::PresetName(s) => s,
            Screen::PresetEdit(s) => s,
            Screen::InputTest(s) => s,
            Screen::Profiles(s) => s,
        }
    }

    pub(crate) fn view_mut(&mut self) -> &mut dyn ScreenView {
        match self {
            Screen::Home(s) => s,
            Screen::Library(s) => s,
            Screen::Settings(s) => s,
            Screen::AddHost(s) => s,
            Screen::Pair(s) => s,
            Screen::PinHosts(s) => s,
            Screen::BindPreset(s) => s,
            Screen::Players(s) => s,
            Screen::RingEditor(s) => &mut **s,
            Screen::ShortcutEditor(s) => s,
            Screen::CardMenu(s) => s,
            Screen::Customize(s) => s,
            Screen::Palette(s) => s,
            Screen::Grants(s) => s,
            Screen::Prompt(s) => s,
            Screen::Licenses(s) => s,
            Screen::Search(s) => s,
            Screen::PresetMenu(s) => s,
            Screen::PresetName(s) => s,
            Screen::PresetEdit(s) => s,
            Screen::InputTest(s) => s,
            Screen::Profiles(s) => s,
        }
    }

    /// The shelf a launch leaves from: the Games tab's, or the games under the Hosts row.
    pub(crate) fn shelf(&self) -> Option<&library::LibraryScreen> {
        match self {
            Screen::Library(l) => Some(l),
            Screen::Home(h) => h.shelf(),
            _ => None,
        }
    }

    /// A finger drag, offered to what the screen scrolls; never while a field is open, where
    /// it scrolls by ticks over the keyboard tray.
    pub(crate) fn pan(&mut self, p: Pointer) -> bool {
        !self.view().editing() && self.view_mut().pan(p)
    }

    /// Focus arrives from the shell's tabs above: a screen with its own strip lands there,
    /// the next thing down.
    pub(crate) fn enter_from_top(&mut self) {
        if let Screen::Settings(s) = self {
            s.enter_from_top();
        }
    }

    /// How far past the content's top and bottom edges the shell's trays reach in, px:
    /// the depth of a screen's own pinned chrome, so one ramp covers it with the band's.
    pub(crate) fn pinned(&self, k: f64, ctx: &Ctx) -> (f32, f32) {
        match self {
            Screen::Library(s) => s.pinned(k),
            Screen::Settings(s) => s.pinned(k),
            _ if self.view().foot(ctx).is_some() => {
                (0.0, (crate::widgets::FOOT_DETAIL_H * k) as f32)
            }
            _ => (0.0, 0.0),
        }
    }

    /// A screen's own pinned chrome, drawn by the shell over its trays after [`ScreenView::render`].
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
            Screen::Settings(s) => s.render_pinned(canvas, rect, k, dt, fonts, ctx),
            _ => {
                let Some(detail) = self.view().foot(ctx) else {
                    return;
                };
                let h = (crate::widgets::FOOT_DETAIL_H * k) as f32;
                let edge = crate::theme::edge(k);
                crate::widgets::Foot {
                    detail: Some(&detail),
                    ..Default::default()
                }
                .paint(
                    canvas,
                    fonts,
                    Rect::from_ltrb(rect.left, rect.bottom - h, rect.right, rect.bottom),
                    (f64::from(rect.left) + edge, f64::from(rect.right) - edge),
                    k,
                );
            }
        }
    }

    pub(crate) fn background(&self) -> Bg {
        match self {
            Screen::Home(_) | Screen::Library(_) | Screen::Players(_) => Bg::Aurora,
            _ => Bg::Form,
        }
    }
}
