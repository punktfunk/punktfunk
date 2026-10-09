//! The card menu: what a hold (OK held on a remote, Y, a long press, a right click) opens
//! on a host card or a poster, and Host details, where everything else a saved host
//! offers lives in section tabs. One screen in three modes; the subject names the object and
//! [`CardMenu::actions`] owns the verbs.
//!
//! A host card's menu is six rows at most, a pinned card's three, a discovered one's two,
//! a poster's four, plus its files and End game; a poster's Details is its card, cover and
//! facts beside its verbs. Every
//! row carries an icon; Back leaves any of them. Tests in this module pin each menu's rows,
//! the arm-then-fire rule and the host-key NUL split (`console-ui-redesign.md` §2).

use crate::glyphs::{Hint, HintKey};
use crate::library::LibraryGame;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::store::SettingsStore;
use crate::theme::{edge, fg, Fonts, W};
use crate::widgets::{blurb, ListMsg, MenuList, RowSpec, TabStrip, TAB_STRIP_H};
use pf_client_core::library::InstallAction;
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::start;
use skia_safe::{Canvas, Image, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    /// The presets, for one connect.
    ConnectWith,
    /// The Games tab on this card's shelf.
    Browse,
    /// Who plays on this host: offered once the card carries a saved pick (§10.1).
    SwitchProfile,
    Wake,
    CopyLink,
    Details,
    Unpin,
    Pair,
    /// Forget the host's identity and keep the host: the next connect asks for a PIN.
    Unpair,
    /// Save a discovered host.
    AddHost,
    /// The presets, for one launch of this title.
    PlayWith,
    /// Launch the title, or bring back the one the host has up.
    Play,
    /// End the title on the host: only one this device launched and the host still runs.
    EndGame,
    /// Start, resume, pause or remove the title's download: the one its files allow.
    Files(InstallAction),
    /// Mark or unmark the title on this device.
    Favorite,
    /// The poster's card: cover, facts, and its verbs.
    TitleDetails,
    /// [`Screen::BindPreset`] for the host, or for a library title.
    BindPreset,
    /// Pin or unpin the catalog's preset `i` as a card of its own.
    Pin(usize),
    /// Measure this host's link: a second connect, so it needs a paired host that answers.
    SpeedTest,
    /// Per-host clipboard share. On the host, not in Settings: the other end is this machine.
    Clipboard,
    Edit,
    /// Point `Settings::default_host` at this record, or clear it when it already does.
    MakeDefault,
    /// Indexed into [`HostRow::actions`]: this build renders labels it has never heard of.
    Host(usize),
    SendLogs,
    Forget,
    /// Connect once with preset `i`; `None` is the host's own binding.
    Preset(Option<usize>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Menu,
    Details,
    ConnectWith,
}

/// What the menu was raised on.
#[derive(Clone)]
pub(crate) enum Subject {
    /// A host card: saved, pinned (the pin rides in the row) or discovered.
    Host(HostRow),
    /// A title on a shelf, with the serving host (pin included) so a link off a pinned
    /// card still streams as that card does.
    Game {
        host: HostRow,
        game: Box<LibraryGame>,
        cover: Option<Image>,
    },
}

pub(crate) struct CardMenu {
    /// By value: discovery rewrites the carousel while this is up, and an index would
    /// retarget Forget onto whatever slid into the slot.
    subject: Subject,
    mode: Mode,
    list: MenuList,
    /// A destructive row armed on the first press fires on the second. `Option<Action>`:
    /// arming Forget must not fire Restart if the cursor moved.
    armed: Option<Action>,
    /// A host's details, one section per tab: the tab shown, its strip, and whether the
    /// D-pad stands on the strip rather than the rows. Boxed: the screen is a variant of
    /// one enum.
    tab: usize,
    strip: Box<TabStrip>,
    strip_focus: bool,
}

impl CardMenu {
    pub(crate) fn for_host(host: &HostRow) -> CardMenu {
        CardMenu::on(Subject::Host(host.clone()), Mode::Menu)
    }

    /// The presets, for one connect to `host`.
    pub(crate) fn connect_with(host: &HostRow) -> CardMenu {
        CardMenu::on(Subject::Host(host.clone()), Mode::ConnectWith)
    }

    /// `host`'s details, in sections.
    pub(crate) fn host_details(host: &HostRow) -> CardMenu {
        CardMenu::on(Subject::Host(host.clone()), Mode::Details)
    }

    pub(crate) fn for_game(host: &HostRow, game: &LibraryGame, cover: Option<Image>) -> CardMenu {
        CardMenu::on(
            Subject::Game {
                host: host.clone(),
                game: Box::new(game.clone()),
                cover,
            },
            Mode::Menu,
        )
    }

    /// This subject in another mode, replacing this screen.
    fn to(&self, mode: Mode) -> Screen {
        Screen::CardMenu(CardMenu::on(self.subject.clone(), mode))
    }

    fn on(subject: Subject, mode: Mode) -> CardMenu {
        CardMenu {
            subject,
            mode,
            list: MenuList::new(),
            armed: None,
            tab: 0,
            strip: Box::new(TabStrip::new()),
            strip_focus: false,
        }
    }

    /// A host's details, which sit in section tabs; a title's stay one card.
    fn tabbed(&self) -> bool {
        self.mode == Mode::Details && matches!(self.subject, Subject::Host(_))
    }

    /// The details' sections in order, the empty ones dropped.
    fn sections(&self, store: &dyn SettingsStore) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for a in self.details(self.host(), store) {
            if !out.contains(&Self::section(a)) {
                out.push(Self::section(a));
            }
        }
        out
    }

    /// Show section `tab` of `n`, wrapping, from its first row.
    fn show_tab(&mut self, tab: i32, n: usize) -> Option<MenuPulse> {
        self.tab = tab.rem_euclid(n.max(1) as i32) as usize;
        self.list.jump_to(0);
        self.armed = None;
        Some(MenuPulse::Move)
    }

    fn host(&self) -> &HostRow {
        match &self.subject {
            Subject::Host(h) => h,
            Subject::Game { host, .. } => host,
        }
    }

    /// Commands address the host, not a pinned card's composite key.
    fn host_key(&self) -> &str {
        self.host().host_key()
    }

    /// The presets this host pins as cards, by id.
    fn pinned(&self, store: &dyn SettingsStore) -> Vec<String> {
        let h = self.host();
        store
            .known_hosts()
            .resolve(Some(&h.fp_hex), &h.addr, h.port)
            .map(|k| k.pinned_presets.clone())
            .unwrap_or_default()
    }

    /// The rows this menu offers. A TV has no clipboard, so no Copy link.
    fn actions(&self, store: &dyn SettingsStore, tv: bool) -> Vec<Action> {
        let mut rows = self.all_actions(store);
        if tv {
            rows.retain(|a| *a != Action::CopyLink);
        }
        rows
    }

    fn all_actions(&self, store: &dyn SettingsStore) -> Vec<Action> {
        let host = match (&self.subject, self.mode) {
            (_, Mode::ConnectWith) => {
                return std::iter::once(Action::Preset(None))
                    .chain((0..store.presets().len()).map(|i| Action::Preset(Some(i))))
                    .collect();
            }
            // No Play row: the poster's OK launches it. The Mac's four rows, the title's files,
            // and End game while the host runs a launch of this device's.
            (Subject::Game { game, .. }, Mode::Menu) => {
                let mut rows = vec![
                    Action::PlayWith,
                    Action::Favorite,
                    Action::TitleDetails,
                    Action::CopyLink,
                ];
                rows.extend(
                    game.install
                        .as_ref()
                        .and_then(|f| f.action)
                        .map(Action::Files),
                );
                rows.extend(game.endable.then_some(Action::EndGame));
                return rows;
            }
            (Subject::Game { game, .. }, _) => {
                let mut rows = vec![
                    Action::Play,
                    Action::Favorite,
                    Action::BindPreset,
                    Action::CopyLink,
                ];
                rows.extend(
                    game.install
                        .as_ref()
                        .and_then(|f| f.action)
                        .map(Action::Files),
                );
                rows.extend(game.endable.then_some(Action::EndGame));
                return rows;
            }
            (Subject::Host(h), _) => h,
        };
        let wake = host.can_wake && !host.online;
        if self.mode == Mode::Details {
            let sections = self.sections(store);
            let shown = sections[self.tab.min(sections.len() - 1)];
            let mut rows = self.details(host, store);
            rows.retain(|a| Self::section(*a) == shown);
            return rows;
        }
        if host.pin.is_some() {
            return vec![Action::Browse, Action::CopyLink, Action::Unpin];
        }
        if !host.saved {
            return vec![Action::Pair, Action::AddHost];
        }
        let mut a = Vec::new();
        if host.paired {
            a.extend([Action::ConnectWith, Action::Browse]);
            if host.profile.is_some() {
                a.push(Action::SwitchProfile);
            }
        } else {
            a.push(Action::Pair);
        }
        if wake {
            a.push(Action::Wake);
        }
        if host.paired {
            a.push(Action::CopyLink);
        }
        a.push(Action::Details);
        a
    }

    /// Host details, section by section; the tab strip takes the sections in this order.
    /// Power leads: sleep and shut down are the rows a player reaches for nightly, the
    /// rest is set once. An empty section drops out.
    fn details(&self, host: &HostRow, store: &dyn SettingsStore) -> Vec<Action> {
        let mut a = Vec::new();
        if host.can_wake && !host.online {
            a.push(Action::Wake);
        }
        a.extend((0..host.actions.len()).map(Action::Host));
        a.push(Action::BindPreset);
        a.extend((0..store.presets().len()).map(Action::Pin));
        if host.paired && host.online {
            a.push(Action::SpeedTest);
        }
        a.extend([Action::Clipboard, Action::Edit]);
        // A record to point at: a discovered row has no id.
        if host.paired && host.id.is_some() {
            a.push(Action::MakeDefault);
        }
        a.push(Action::Pair);
        if host.paired {
            a.push(Action::Unpair);
        }
        // Upload authenticates with the streaming cert and needs a live host.
        if host.paired && host.online {
            a.push(Action::SendLogs);
        }
        a.push(Action::Forget);
        a
    }

    /// The section, and so the tab, a details row sits in.
    fn section(a: Action) -> &'static str {
        match a {
            Action::BindPreset | Action::Pin(_) => "Presets",
            Action::SpeedTest | Action::Clipboard | Action::Edit | Action::MakeDefault => {
                "Connection"
            }
            Action::Pair | Action::Unpair => "Pairing",
            Action::Wake | Action::Host(_) => "Power",
            Action::SendLogs => "Logs",
            _ => "Remove",
        }
    }

    fn icon(&self, a: Action) -> &'static str {
        match a {
            Action::ConnectWith | Action::PlayWith | Action::Play | Action::Preset(_) => "play",
            Action::Favorite => "check",
            Action::TitleDetails => "info",
            Action::Browse => "gamepad-2",
            Action::SwitchProfile => "refresh-cw",
            Action::Wake => "power",
            Action::CopyLink => "link",
            Action::Clipboard => "copy",
            Action::Details => "info",
            Action::Unpin | Action::Pin(_) => "pin",
            Action::Pair => "lock",
            Action::Unpair => "log-out",
            Action::AddHost => "plus",
            Action::BindPreset => "settings",
            Action::SpeedTest => "gauge",
            Action::Edit => "pencil",
            Action::MakeDefault => "house",
            Action::Host(i) => match self.host().actions.get(i).map(|x| x.id.as_str()) {
                Some("power.sleep") => "moon",
                Some("power.reboot") => "rotate-cw",
                _ => "power",
            },
            Action::SendLogs => "scroll-text",
            Action::Forget => "trash-2",
            Action::EndGame => "x",
            Action::Files(InstallAction::Install | InstallAction::Resume) => "download",
            Action::Files(InstallAction::Pause) => "pause",
            Action::Files(InstallAction::Remove) => "trash-2",
        }
    }

    /// The title is marked on this device.
    fn is_favorite(&self, settings: &pf_client_core::trust::Settings) -> bool {
        match &self.subject {
            Subject::Game { host, game, .. } => {
                crate::library::favorites(settings, &host.fp_hex).contains(&game.id)
            }
            Subject::Host(_) => false,
        }
    }

    /// `default` (`Settings::default_host`) names this row. A derived default reads as unset.
    fn is_default(&self, default: Option<&str>) -> bool {
        let id = self.host().id.as_deref();
        id.is_some() && default == id
    }

    fn label(&self, a: Action, ctx: &Ctx) -> String {
        let presets = || ctx.store.presets();
        match a {
            Action::ConnectWith => "Connect with\u{2026}".into(),
            Action::Browse => "Browse games".into(),
            Action::SwitchProfile => "Switch profile\u{2026}".into(),
            Action::Wake => "Wake host".into(),
            Action::CopyLink => "Copy link".into(),
            Action::Details => "Host details\u{2026}".into(),
            Action::Unpin => "Unpin card".into(),
            Action::Pair if self.host().paired => "Pair again\u{2026}".into(),
            Action::Pair => "Pair\u{2026}".into(),
            Action::Unpair if self.armed == Some(Action::Unpair) => {
                "Unpair \u{2014} press again".into()
            }
            Action::Unpair => "Unpair".into(),
            Action::AddHost => "Add host".into(),
            Action::PlayWith => "Play with preset\u{2026}".into(),
            Action::Play => match &self.subject {
                Subject::Game { game, .. } if game.running => "Resume".into(),
                _ => "Play".into(),
            },
            Action::EndGame if self.armed == Some(Action::EndGame) => {
                "End game \u{2014} press again".into()
            }
            Action::EndGame => "End game".into(),
            Action::Files(InstallAction::Remove) if self.armed == Some(a) => {
                "Remove download \u{2014} press again".into()
            }
            Action::Files(f) => match &self.subject {
                Subject::Game { game, .. } => f.label(game.install.as_ref().map(|i| &i.install)),
                Subject::Host(_) => String::new(),
            },
            Action::Favorite if self.is_favorite(ctx.settings) => "Remove from Favorites".into(),
            Action::Favorite => "Add to Favorites".into(),
            Action::TitleDetails => "Details\u{2026}".into(),
            Action::BindPreset => match self.subject {
                Subject::Game { .. } => "Settings preset\u{2026}".into(),
                Subject::Host(_) => "Default preset\u{2026}".into(),
            },
            Action::Pin(i) => {
                let Some((id, name)) = presets().get(i).cloned() else {
                    return String::new();
                };
                let on = self.pinned(ctx.store).contains(&id);
                format!(
                    "\u{201c}{name}\u{201d} card: {}",
                    if on { "On" } else { "Off" }
                )
            }
            // The session binary runs the whole check; the other shells measure speed.
            Action::SpeedTest => match ctx.device.platform {
                crate::platform::Platform::Desktop => "Check network\u{2026}".into(),
                _ => "Test network speed\u{2026}".into(),
            },
            Action::Clipboard => format!(
                "Shared clipboard: {}",
                if self.host().clipboard_sync {
                    "On"
                } else {
                    "Off"
                }
            ),
            Action::Edit => "Edit name and address\u{2026}".into(),
            // Geist carries no U+2713, so a check mark here draws as a missing glyph.
            Action::MakeDefault if self.is_default(ctx.settings.default_host.as_deref()) => {
                "Default host: On".into()
            }
            Action::MakeDefault => "Default host: Off".into(),
            Action::Host(i) => match self.host().actions.get(i) {
                Some(act) if self.armed == Some(a) => {
                    format!("{} \u{2014} press again", act.label)
                }
                Some(act) => act.label.clone(),
                None => String::new(),
            },
            Action::SendLogs => "Send logs to host".into(),
            Action::Forget if self.armed == Some(Action::Forget) => {
                "Remove host \u{2014} press again".into()
            }
            Action::Forget => "Remove host".into(),
            Action::Preset(None) => "Default settings".into(),
            Action::Preset(Some(i)) => presets().get(i).map(|p| p.1.clone()).unwrap_or_default(),
        }
    }

    /// Host-reported verbs can be unavailable. The row stays; activating it says why.
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Host(i) => self.host().actions.get(i).is_none_or(|act| act.available),
            _ => true,
        }
    }

    fn dispatch(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        actions: &[Action],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(action) = actions.get(self.list.cursor).copied() else {
            return pulse;
        };
        // Arming is per row: leaving it must not leave a live trigger on the next one.
        if !matches!(msg, ListMsg::Activate) && self.armed != Some(action) {
            self.armed = None;
        }
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => {
                self.run(action, ctx, fx);
                pulse
            }
        }
    }

    /// `punktfunk://` from the store at activation, never at open: the row may have left
    /// the store while the menu was up.
    fn link(&self, store: &dyn SettingsStore) -> Option<String> {
        match &self.subject {
            Subject::Host(h) => crate::screens::host_link(store, h),
            Subject::Game { host, game, .. } => crate::screens::saved_host_link(
                store,
                &host.fp_hex,
                &host.addr,
                host.port,
                host.pin.as_ref().map(|p| p.id.as_str()),
                Some(game.id.as_str()),
            ),
        }
    }

    /// A connect with `preset`: a poster's launches its title, a card's the host alone.
    fn connect(&self, preset: Option<String>) -> super::ConnectIntent {
        let game = match &self.subject {
            Subject::Game { game, .. } => Some((game.id.as_str(), game.title.as_str())),
            Subject::Host(_) => None,
        };
        super::ConnectIntent::to_host(self.host(), game).with_preset(preset)
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx, fx: &mut Outbox) {
        let store = ctx.store;
        let key = self.host_key().to_string();
        match action {
            Action::ConnectWith | Action::PlayWith => fx.replace(self.to(Mode::ConnectWith)),
            Action::Details | Action::TitleDetails => fx.replace(self.to(Mode::Details)),
            Action::Play => {
                fx.connect = Some(self.connect(self.host().pin.as_ref().map(|p| p.id.clone())));
                fx.pop();
            }
            Action::Favorite => {
                let Subject::Game { host, game, .. } = &self.subject else {
                    return;
                };
                let mut on = false;
                ctx.write(|c| {
                    on = crate::library::toggle_favorite(c.settings, &host.fp_hex, &game.id);
                    true
                });
                fx.toast = Some(if on {
                    format!("{} is a favorite", game.title)
                } else {
                    format!("{} is no longer a favorite", game.title)
                });
                if self.mode == Mode::Menu {
                    fx.pop();
                }
            }
            Action::SwitchProfile => {
                let host = self.host();
                if let Some(screen) = super::profiles::ProfilesScreen::switch(host) {
                    fx.cmds.push(super::profiles::ProfilesScreen::fetch(host));
                    fx.replace(Screen::Profiles(screen));
                }
            }
            // The games under the row; the shell falls back to the Games tab.
            Action::Browse => {
                fx.browse = true;
                fx.pop();
            }
            Action::Wake => {
                fx.cmds.push(ConsoleCmd::Wake {
                    key,
                    then_connect: false,
                });
                fx.pop();
            }
            Action::Pair => fx.replace(Screen::Pair(super::pair::PairScreen::new(
                self.host(),
                &ctx.device.name,
            ))),
            Action::AddHost => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::SaveHost {
                    name: host.name.clone(),
                    addr: host.addr.clone(),
                    port: host.port,
                });
                fx.toast = Some(format!("Added {}", host.name));
                fx.pop();
            }
            // Toggles what the row showed, not what the rebase reads.
            Action::MakeDefault => {
                let on = !self.is_default(ctx.settings.default_host.as_deref());
                let name = self.host().name.clone();
                let id = self.host().id.clone();
                ctx.write(|c| {
                    c.settings.default_host = on.then_some(id).flatten();
                    true
                });
                let opens = start::StartIn::parse(&ctx.settings.start_in) != start::StartIn::Hosts;
                fx.toast = Some(match (on, opens) {
                    (true, true) => format!("{name} opens on launch"),
                    (true, false) => format!("{name} is the default host"),
                    (false, _) => format!("{name} is no longer the default host"),
                });
            }
            Action::SendLogs => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::SendLogs {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    host_name: host.name.clone(),
                });
                fx.toast = Some(format!("Sending logs to {}\u{2026}", host.name));
                fx.pop();
            }
            Action::SpeedTest => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::SpeedTest {
                    key,
                    addr: host.addr.clone(),
                    port: host.port,
                    fp_hex: host.fp_hex.clone(),
                    host_name: host.name.clone(),
                });
                // No toast: the takeover the service raises is the feedback.
                fx.pop();
            }
            Action::CopyLink => {
                match self.link(store) {
                    Some(url) => {
                        fx.copy = Some(url);
                        fx.toast = Some("Link copied".into());
                    }
                    None => fx.toast = Some("This host isn't saved any more".into()),
                }
                fx.pop();
            }
            Action::Preset(i) => {
                let preset = i.and_then(|i| store.presets().get(i).map(|p| p.0.clone()));
                fx.connect = Some(self.connect(preset));
                fx.pop();
            }
            Action::Edit => fx.replace(Screen::AddHost(super::add_host::AddHostScreen::edit(
                self.host(),
            ))),
            // Same screen either way; the subject decides which binding it writes.
            Action::BindPreset => {
                let host_name = self.host().name.clone();
                let screen = match &self.subject {
                    Subject::Game { game, .. } => super::bind_preset::BindPresetScreen::for_game(
                        key,
                        host_name,
                        super::bind_preset::GameSubject {
                            id: game.id.clone(),
                            title: game.title.clone(),
                        },
                        store.presets(),
                    ),
                    Subject::Host(_) => {
                        super::bind_preset::BindPresetScreen::new(key, host_name, store.presets())
                    }
                };
                fx.replace(Screen::BindPreset(screen));
            }
            Action::Pin(i) => {
                if let Some((id, name)) = store.presets().get(i).cloned() {
                    let pin = !self.pinned(store).contains(&id);
                    fx.toast = Some(if pin {
                        format!("Pinned \u{201c}{name}\u{201d} as a card")
                    } else {
                        format!("Unpinned \u{201c}{name}\u{201d}")
                    });
                    fx.cmds.push(ConsoleCmd::SetPin {
                        key,
                        preset_id: id,
                        pin,
                    });
                }
            }
            Action::Clipboard => {
                let host = self.host();
                let on = !host.clipboard_sync;
                fx.toast = Some(if on {
                    format!("Clipboard shared with {}", host.name)
                } else {
                    format!("Clipboard no longer shared with {}", host.name)
                });
                fx.cmds.push(ConsoleCmd::SetClipboard { key, on });
                fx.pop();
            }
            Action::Unpair if self.armed != Some(Action::Unpair) => {
                self.armed = Some(Action::Unpair)
            }
            Action::Unpair => {
                fx.cmds.push(ConsoleCmd::UnpairHost { key });
                fx.toast = Some(format!(
                    "Unpaired {}. The next connect asks for a PIN.",
                    self.host().name
                ));
                fx.pop();
            }
            // Ending a game can lose unsaved progress: arm, then fire.
            Action::EndGame if self.armed != Some(Action::EndGame) => {
                self.armed = Some(Action::EndGame)
            }
            Action::EndGame => {
                let Subject::Game { host, game, .. } = &self.subject else {
                    return;
                };
                fx.cmds.push(ConsoleCmd::EndGame {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    app_id: game.id.clone(),
                    title: game.title.clone(),
                });
                fx.toast = Some(format!("Ending {}\u{2026}", game.title));
                fx.pop();
            }
            // Removing loses the download: arm, then fire.
            Action::Files(InstallAction::Remove) if self.armed != Some(action) => {
                self.armed = Some(action)
            }
            Action::Files(f) => {
                let Subject::Game { host, game, .. } = &self.subject else {
                    return;
                };
                fx.cmds.push(ConsoleCmd::Install {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    app_id: game.id.clone(),
                    title: game.title.clone(),
                    action: f,
                });
                fx.toast = Some(match f {
                    InstallAction::Install | InstallAction::Resume => {
                        format!("Starting {}'s download\u{2026}", game.title)
                    }
                    InstallAction::Pause => format!("Pausing {}'s download\u{2026}", game.title),
                    InstallAction::Remove => format!("Removing {}\u{2026}", game.title),
                });
                fx.pop();
            }
            Action::Forget if self.armed != Some(Action::Forget) => {
                self.armed = Some(Action::Forget)
            }
            Action::Forget => {
                fx.cmds.push(ConsoleCmd::ForgetHost { key });
                fx.toast = Some(format!("Removed {}", self.host().name));
                fx.pop();
            }
            Action::Host(i) => {
                let host = self.host();
                let Some(act) = host.actions.get(i) else {
                    return; // row list changed under the cursor
                };
                // The host says no: say why rather than send a request it will refuse.
                if !act.available {
                    let why = act.unavailable_reason.clone();
                    fx.toast = Some(if why.is_empty() {
                        format!("{} isn't available right now", act.label)
                    } else {
                        why
                    });
                    fx.pop();
                    return;
                }
                // `danger` (restart, shut down) arms then fires. Sleep undoes with Wake.
                if act.danger && self.armed != Some(action) {
                    self.armed = Some(action);
                    return;
                }
                fx.cmds.push(ConsoleCmd::HostAction {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    host_name: host.name.clone(),
                    action_id: act.id.clone(),
                    label: act.label.clone(),
                });
                fx.toast = Some(format!(
                    "{} \u{2014} asking {}\u{2026}",
                    act.label, host.name
                ));
                fx.pop();
            }
            Action::Unpin => {
                if let Some(p) = &self.host().pin {
                    fx.cmds.push(ConsoleCmd::SetPin {
                        key,
                        preset_id: p.id.clone(),
                        pin: false,
                    });
                    fx.toast = Some(format!("Unpinned {}", p.name));
                }
                fx.pop();
            }
        }
    }

    fn blurb(&self) -> String {
        match (&self.subject, self.mode) {
            (_, Mode::ConnectWith) => "This connect only; the card keeps its own preset.".into(),
            (Subject::Host(h), _) if h.pin.is_some() => {
                "A shortcut to one preset on this host. Unpinning it changes nothing about the \
                 host or the preset."
                    .into()
            }
            (Subject::Host(h), _) if !h.saved => "Found on this network.".into(),
            (Subject::Host(_), _) => String::new(),
            (Subject::Game { .. }, Mode::Details) => String::new(),
            (Subject::Game { host, .. }, _) => format!("On {}.", host.name),
        }
    }
}

impl ScreenView for CardMenu {
    /// OK went down: the plate under the focused tab or row dips.
    fn press(&mut self) {
        if self.strip_focus {
            self.strip.press();
        } else {
            self.list.dip();
        }
    }

    fn title(&self) -> String {
        let name = match &self.subject {
            Subject::Host(h) => match &h.pin {
                Some(p) => format!("{} \u{b7} {}", h.name, p.name),
                None => h.name.clone(),
            },
            Subject::Game { game, .. } => game.title.clone(),
        };
        match (self.mode, &self.subject) {
            (Mode::Details, Subject::Game { .. }) | (Mode::Menu, _) => name,
            (Mode::Details, _) => format!("{name} \u{b7} Details"),
            (Mode::ConnectWith, Subject::Game { .. }) => format!("Play {name} with"),
            (Mode::ConnectWith, _) => format!("Connect to {name} with"),
        }
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        if self.tabbed() {
            // Up from the first row stands on the section tabs; there Left and Right walk
            // them and Down or OK returns. L1/R1 walk them from anywhere.
            let n = self.sections(ctx.store).len();
            let tab = self.tab as i32;
            match (ev, self.strip_focus) {
                (MenuEvent::JumpBack, _) => return self.show_tab(tab - 1, n),
                (MenuEvent::JumpForward, _) => return self.show_tab(tab + 1, n),
                (MenuEvent::Move(MenuDir::Up), false) if self.list.cursor == 0 => {
                    self.strip_focus = true;
                    return Some(MenuPulse::Move);
                }
                (MenuEvent::Move(MenuDir::Left), true) => return self.show_tab(tab - 1, n),
                (MenuEvent::Move(MenuDir::Right), true) => return self.show_tab(tab + 1, n),
                (MenuEvent::Move(MenuDir::Down) | MenuEvent::Confirm, true) => {
                    self.strip_focus = false;
                    return Some(MenuPulse::Move);
                }
                (MenuEvent::Move(_), true) => return Some(MenuPulse::Boundary),
                (_, true) => return None,
                _ => {}
            }
        }
        let actions = self.actions(ctx.store, ctx.device.tv);
        let (msg, pulse) = self.list.menu(ev, actions.len());
        self.dispatch(msg, pulse, &actions, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.tabbed() {
            if let Some(tab) = self.strip.pointer(p) {
                if p.press() {
                    self.show_tab(tab as i32, self.sections(ctx.store).len());
                }
                return true;
            }
            if p.press() {
                self.strip_focus = false;
            }
        }
        let actions = self.actions(ctx.store, ctx.device.tv);
        let (msg, pulse) = self.list.pointer(p, actions.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.dispatch(msg, pulse, &actions, ctx, fx);
        true
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Choose"),
            Hint::new(HintKey::Back, "Close"),
        ]
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let mut list_rect = blurb(canvas, fonts, &self.blurb(), rect, k);
        if let (Subject::Game { game, cover, .. }, Mode::Details) = (&self.subject, self.mode) {
            list_rect = title_card(canvas, fonts, game, cover.as_ref(), rect, k);
        }
        let sections = if self.tabbed() {
            self.sections(ctx.store)
        } else {
            Vec::new()
        };
        self.tab = self.tab.min(sections.len().saturating_sub(1));
        let strip_h = if sections.is_empty() {
            0.0
        } else {
            TAB_STRIP_H * k
        };
        let strip_top = list_rect.top;
        list_rect.top += strip_h as f32;
        let actions = self.actions(ctx.store, ctx.device.tv);
        let rows: Vec<RowSpec> = actions
            .iter()
            .map(|&a| {
                let row =
                    RowSpec::action(self.label(a, ctx), self.enabled(a)).with_icon(self.icon(a));
                // The address sits on the row that edits it; the pick on the row that changes it.
                match (a, &self.subject) {
                    (Action::Edit, Subject::Host(h)) => RowSpec {
                        value: Some(format!("{}:{}", h.addr, h.port)),
                        ..row
                    },
                    (Action::SwitchProfile, Subject::Host(h)) => RowSpec {
                        value: h.profile.as_ref().map(|p| p.display_name.clone()),
                        ..row
                    },
                    _ => row,
                }
            })
            .collect();
        let active = !self.strip_focus;
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, active);
        if !sections.is_empty() {
            // The tabs on the screen's margin, under the title, as Settings' sections sit.
            let bottom = strip_top + strip_h as f32;
            let r = Rect::from_ltrb(rect.left, strip_top, rect.right, bottom);
            let (tab, focused) = (self.tab, self.strip_focus);
            self.strip
                .render(canvas, r, &sections, tab, focused, fonts, k, dt);
        }
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

/// A poster's card: the cover at the left, the facts beside it. Returns where its verbs go.
fn title_card(
    canvas: &Canvas,
    fonts: &Fonts,
    game: &LibraryGame,
    cover: Option<&Image>,
    rect: Rect,
    k: f64,
) -> Rect {
    let pad = edge(k);
    let ch = (f64::from(rect.height()) - 24.0 * k).min(420.0 * k);
    let cw = ch * 2.0 / 3.0;
    let art = Rect::from_xywh(
        (f64::from(rect.left) + pad) as f32,
        (f64::from(rect.top) + 8.0 * k) as f32,
        cw as f32,
        ch as f32,
    );
    let rr = skia_safe::RRect::new_rect_xy(art, (14.0 * k) as f32, (14.0 * k) as f32);
    canvas.save();
    canvas.clip_rrect(rr, None, true);
    match cover {
        Some(img) => {
            let src = Rect::from_wh(img.width() as f32, img.height() as f32);
            canvas.draw_image_rect_with_sampling_options(
                img,
                Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                art,
                crate::theme::art_sampling(),
                &crate::theme::fill(fg(1.0)),
            );
        }
        None => super::library::draw_poster_placeholder(canvas, fonts, Some(game), art, k, 1.0),
    }
    canvas.restore();
    let x = f64::from(art.right) + 28.0 * k;
    let w = f64::from(rect.right) - x - pad;
    let mut y = f64::from(art.top) + 26.0 * k;
    fonts.draw_clipped(canvas, &game.title, x, y, W::Bold, 26.0 * k, fg(1.0), w);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let store = [Some(game.store.clone()), game.platform.clone()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
    let facts = [
        Some(store),
        game.developer.clone(),
        game.year.map(|y| y.to_string()),
        (!game.genres.is_empty()).then(|| game.genres.join(", ")),
        game.stats
            .as_ref()
            .and_then(|s| crate::library::stats_line(s, now)),
    ];
    for line in facts.into_iter().flatten().filter(|l| !l.is_empty()) {
        y += 24.0 * k;
        fonts.draw_clipped(canvas, &line, x, y, W::Regular, 15.0 * k, fg(0.7), w);
    }
    Rect::from_ltrb(
        (x - 24.0 * k) as f32,
        (y + 24.0 * k) as f32,
        rect.right,
        rect.bottom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PresetChip;
    use crate::screens::settings::tests::fake_home;
    use crate::screens::Nav;

    fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
        fake_home();
        let mut settings = crate::store::file_store().load();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        f(&mut ctx)
    }

    /// Drive `run` over a scratch config dir. Settings come from the store, so a row that
    /// writes one reads its own last write.
    fn run_action(s: &mut CardMenu, action: Action, fx: &mut Outbox) {
        with_ctx(|ctx| s.run(action, ctx, fx));
    }

    fn rows(s: &CardMenu) -> Vec<Action> {
        s.actions(crate::store::file_store(), false)
    }

    fn label(s: &CardMenu, a: Action) -> String {
        with_ctx(|ctx| s.label(a, ctx))
    }

    fn details(h: &HostRow) -> CardMenu {
        CardMenu::on(Subject::Host(h.clone()), Mode::Details)
    }

    /// `h`'s details tab by tab, as the D-pad meets them.
    fn tabs(h: &HostRow) -> Vec<Vec<Action>> {
        let n = details(h).sections(crate::store::file_store()).len();
        (0..n)
            .map(|tab| rows(&CardMenu { tab, ..details(h) }))
            .collect()
    }

    fn all_rows(h: &HostRow) -> Vec<Action> {
        tabs(h).concat()
    }

    fn host() -> HostRow {
        HostRow {
            addr: "10.0.0.5".into(),
            mgmt_port: 9778,
            ..HostRow::fixture("aa", "Desk")
        }
    }

    fn powered() -> HostRow {
        let act = |id: &str, label: &str, danger: bool, available: bool| crate::model::HostAction {
            id: id.into(),
            label: label.into(),
            danger,
            available,
            unavailable_reason: if available {
                String::new()
            } else {
                "this machine does not support sleep".into()
            },
        };
        HostRow {
            actions: vec![
                act("power.sleep", "Sleep host", false, true),
                act("power.reboot", "Restart host", true, true),
                act("power.shutdown", "Shut down host", true, true),
            ],
            ..host()
        }
    }

    fn pinned() -> HostRow {
        HostRow {
            key: "aa\u{0}prof-1".into(),
            pin: Some(PresetChip {
                id: "prof-1".into(),
                name: "4K".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            ..host()
        }
    }

    fn game() -> LibraryGame {
        LibraryGame {
            id: "steam:367520".into(),
            title: "Hollow Knight".into(),
            store: "steam".into(),
            launcher: false,
            icon: "steam".into(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
            endable: false,
            install: None,
        }
    }

    /// The design's three card menus: six rows at most, three on a pin, two on a find.
    #[test]
    fn each_card_gets_its_own_short_menu() {
        use Action::*;
        let asleep = HostRow {
            can_wake: true,
            online: false,
            ..host()
        };
        assert_eq!(
            rows(&CardMenu::for_host(&asleep)),
            vec![ConnectWith, Browse, Wake, CopyLink, Details]
        );
        let picked = HostRow {
            profile: Some(pf_client_core::profiles::ProfilePick {
                id: "kid".into(),
                display_name: "Kid".into(),
            }),
            ..asleep.clone()
        };
        assert_eq!(
            rows(&CardMenu::for_host(&picked)),
            vec![ConnectWith, Browse, SwitchProfile, Wake, CopyLink, Details],
            "a saved pick is one press away from changing"
        );
        assert_eq!(
            rows(&CardMenu::for_host(&host())),
            vec![ConnectWith, Browse, CopyLink, Details],
            "an awake host is not offered a wake"
        );
        assert_eq!(
            rows(&CardMenu::for_host(&pinned())),
            vec![Browse, CopyLink, Unpin]
        );
        let found = HostRow {
            saved: false,
            paired: false,
            ..host()
        };
        assert_eq!(rows(&CardMenu::for_host(&found)), vec![Pair, AddHost]);
        let unpaired = HostRow {
            paired: false,
            ..host()
        };
        assert_eq!(rows(&CardMenu::for_host(&unpaired)), vec![Pair, Details]);
        // Commands address the host, not the pin's composite key.
        assert_eq!(CardMenu::for_host(&pinned()).host_key(), "aa");
    }

    /// Switch profile opens the picker over the menu's place and asks the host for its list.
    #[test]
    fn switch_profile_asks_for_the_list() {
        let h = HostRow {
            profile: Some(pf_client_core::profiles::ProfilePick {
                id: "kid".into(),
                display_name: "Kid".into(),
            }),
            ..host()
        };
        let mut s = CardMenu::for_host(&h);
        let mut fx = Outbox::default();
        run_action(&mut s, Action::SwitchProfile, &mut fx);
        assert!(matches!(
            fx.nav,
            Some(Nav::Replace(ref sc)) if matches!(**sc, Screen::Profiles(_))
        ));
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::FetchProfiles {
                addr: "10.0.0.5".into(),
                mgmt: 9778,
                fp_hex: "aa".into(),
            }]
        );
        assert_eq!(label(&s, Action::SwitchProfile), "Switch profile\u{2026}");
    }

    #[test]
    fn browse_closes_the_menu_onto_the_games_below() {
        let mut s = CardMenu::for_host(&host());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Browse, &mut fx);
        assert!(fx.browse && fx.tab.is_none());
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    #[test]
    fn details_and_connect_with_replace_the_menu() {
        for action in [Action::Details, Action::ConnectWith] {
            let mut s = CardMenu::for_host(&host());
            let mut fx = Outbox::default();
            run_action(&mut s, action, &mut fx);
            assert!(
                matches!(fx.nav, Some(Nav::Replace(ref sc)) if matches!(**sc, Screen::CardMenu(_)))
            );
        }
    }

    /// Connect with… connects once; the default row leaves the preset to the binding.
    #[test]
    fn connect_with_default_settings_names_no_preset() {
        let mut s = CardMenu::on(Subject::Host(host()), Mode::ConnectWith);
        assert_eq!(rows(&s).first(), Some(&Action::Preset(None)));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Preset(None), &mut fx);
        let intent = fx.connect.expect("a connect");
        assert_eq!(intent.preset, None);
        assert_eq!(intent.launch, None);
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    #[test]
    fn a_discovered_card_adds_the_host() {
        let found = HostRow {
            saved: false,
            paired: false,
            ..host()
        };
        let mut s = CardMenu::for_host(&found);
        let mut fx = Outbox::default();
        run_action(&mut s, Action::AddHost, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SaveHost {
                name: "Desk".into(),
                addr: "10.0.0.5".into(),
                port: 9777,
            }]
        );
    }

    /// Details holds the rest, one section per tab: the speed test, logs and wake need what
    /// they always needed, and removal is the last tab.
    #[test]
    fn details_holds_everything_else_in_section_tabs() {
        let h = HostRow {
            id: Some("rec-1".into()),
            ..host()
        };
        for tab in tabs(&h) {
            let sections: Vec<&str> = tab.iter().map(|a| CardMenu::section(*a)).collect();
            assert!(
                sections.windows(2).all(|w| w[0] == w[1]),
                "one per tab: {sections:?}"
            );
        }
        let r = all_rows(&h);
        for a in [
            Action::BindPreset,
            Action::SpeedTest,
            Action::Clipboard,
            Action::Edit,
            Action::MakeDefault,
            Action::Pair,
            Action::SendLogs,
        ] {
            assert!(r.contains(&a), "{a:?} missing");
        }
        assert_eq!(r.last(), Some(&Action::Forget));

        let offline = all_rows(&HostRow {
            online: false,
            can_wake: true,
            ..host()
        });
        assert!(offline.contains(&Action::Wake));
        assert!(!offline.contains(&Action::SpeedTest) && !offline.contains(&Action::SendLogs));
        let unpaired = all_rows(&HostRow {
            paired: false,
            id: Some("rec-1".into()),
            ..host()
        });
        assert!(!unpaired.contains(&Action::MakeDefault));
    }

    /// Up from a tab's first row stands on the tabs, Right shows the next section from its
    /// top, and Down goes back to the rows.
    #[test]
    fn the_details_tabs_walk_by_dpad() {
        let mut s = details(&host());
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            let mut go = |s: &mut CardMenu, ev| s.menu(ev, ctx, &mut fx);
            let up = go(&mut s, MenuEvent::Move(MenuDir::Up));
            assert!(matches!(up, Some(MenuPulse::Move)));
            assert!(s.strip_focus);
            go(&mut s, MenuEvent::Move(MenuDir::Right));
            assert_eq!((s.tab, s.list.cursor), (1, 0));
            go(&mut s, MenuEvent::Move(MenuDir::Down));
            assert!(!s.strip_focus);
            go(&mut s, MenuEvent::JumpBack);
            assert_eq!(s.tab, 0, "L1 walks back from the rows");
        });
    }

    /// Power is the first tab when the host offers it: sleep is a nightly row, presets are
    /// set once. A host with nothing to offer opens on Presets.
    #[test]
    fn power_leads_the_details_when_the_host_offers_it() {
        let first = |h: &HostRow| details(h).sections(crate::store::file_store())[0];
        assert_eq!(first(&powered()), "Power");
        assert_eq!(first(&host()), "Presets");
        let asleep = HostRow {
            online: false,
            can_wake: true,
            ..host()
        };
        assert_eq!(first(&asleep), "Power");
    }

    #[test]
    fn host_actions_appear_only_when_the_host_offered_them() {
        assert!(!all_rows(&host())
            .iter()
            .any(|a| matches!(a, Action::Host(_))));
        let s = details(&powered());
        assert_eq!(
            all_rows(&powered())
                .iter()
                .filter(|a| matches!(a, Action::Host(_)))
                .count(),
            3
        );
        assert_eq!(label(&s, Action::Host(0)), "Sleep host");
        assert_eq!(s.icon(Action::Host(0)), "moon");
    }

    /// Sleep fires on one press. Restart and shut down arm; arming one must not leave the
    /// other live.
    #[test]
    fn destructive_host_actions_arm_before_they_fire() {
        let mut s = details(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.sleep"
        ));

        let mut s = details(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(
            label(&s, Action::Host(2)),
            "Shut down host \u{2014} press again"
        );
        assert_eq!(label(&s, Action::Host(1)), "Restart host");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(1), &mut fx);
        assert!(
            fx.cmds.is_empty(),
            "arming shut down must not leave restart armed"
        );
        let mut s = details(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.shutdown"
        ));
    }

    #[test]
    fn an_unavailable_action_explains_itself_instead_of_firing() {
        let mut s = details(&HostRow {
            actions: vec![crate::model::HostAction {
                id: "power.sleep".into(),
                label: "Sleep host".into(),
                danger: false,
                available: false,
                unavailable_reason: "this machine does not support sleep".into(),
            }],
            ..host()
        });
        assert!(!s.enabled(Action::Host(0)));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx);
        assert!(fx.cmds.is_empty(), "no request the host would refuse");
        assert_eq!(
            fx.toast.as_deref(),
            Some("this machine does not support sleep")
        );
    }

    #[test]
    fn default_preset_opens_the_chooser_on_the_hosts_plain_key() {
        let mut s = details(&host());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::BindPreset, &mut fx);
        match fx.nav {
            Some(Nav::Replace(screen)) => match *screen {
                Screen::BindPreset(b) => assert_eq!(b.host_name(), "Desk"),
                _ => panic!("expected the bind-preset chooser"),
            },
            _ => panic!("expected a replace"),
        }
    }

    #[test]
    fn the_clipboard_toggle_flips_the_stored_state() {
        let mut s = details(&host());
        assert!(label(&s, Action::Clipboard).ends_with("Off"));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Clipboard, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetClipboard {
                key: "aa".into(),
                on: true,
            }]
        );
        let mut s = details(&HostRow {
            clipboard_sync: true,
            ..host()
        });
        assert!(label(&s, Action::Clipboard).ends_with("On"));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Clipboard, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetClipboard {
                key: "aa".into(),
                on: false,
            }]
        );
    }

    #[test]
    fn remove_needs_two_presses() {
        let mut s = details(&host());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Forget, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert!(label(&s, Action::Forget).contains("press again"));
        run_action(&mut s, Action::Forget, &mut fx);
        assert_eq!(fx.cmds, vec![ConsoleCmd::ForgetHost { key: "aa".into() }]);
    }

    #[test]
    fn leaving_the_remove_row_disarms_it() {
        let mut s = details(&host());
        let actions = rows(&s);
        s.armed = Some(Action::Forget);
        s.list.cursor = 0;
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.dispatch(ListMsg::None, None, &actions, ctx, &mut fx));
        assert_eq!(
            s.armed, None,
            "a cursor move off the row cancels the arming"
        );
    }

    /// The Mac's four rows: nothing the poster's own OK already does.
    #[test]
    fn a_poster_menu_is_the_macs_four_rows() {
        let s = CardMenu::for_game(&host(), &game(), None);
        assert_eq!(
            rows(&s),
            vec![
                Action::PlayWith,
                Action::Favorite,
                Action::TitleDetails,
                Action::CopyLink
            ]
        );
        assert_eq!(s.title(), "Hollow Knight");
        let details = CardMenu::on(s.subject.clone(), Mode::Details);
        assert_eq!(
            rows(&details),
            vec![
                Action::Play,
                Action::Favorite,
                Action::BindPreset,
                Action::CopyLink
            ]
        );
        assert_eq!(
            label(&details, Action::BindPreset),
            "Settings preset\u{2026}"
        );
    }

    /// End game shows only on a title this device launched and the host still runs, and
    /// it arms before it asks the host.
    #[test]
    fn end_game_is_offered_on_this_devices_running_launch_and_arms_first() {
        let mut running = game();
        running.running = true;
        assert!(!rows(&CardMenu::for_game(&host(), &running, None)).contains(&Action::EndGame));
        running.endable = true;
        let mut s = CardMenu::for_game(&host(), &running, None);
        assert_eq!(rows(&s).last(), Some(&Action::EndGame));
        let details = CardMenu::on(s.subject.clone(), Mode::Details);
        assert_eq!(rows(&details).last(), Some(&Action::EndGame));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::EndGame, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert!(label(&s, Action::EndGame).contains("press again"));
        run_action(&mut s, Action::EndGame, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::EndGame {
                addr: "10.0.0.5".into(),
                mgmt: 9778,
                fp_hex: "aa".into(),
                app_id: "steam:367520".into(),
                title: "Hollow Knight".into(),
            }]
        );
    }

    /// A title's files row is the one its store state allows, and Remove arms first.
    #[test]
    fn a_titles_files_row_follows_its_install_and_remove_arms_first() {
        let installed = |action| {
            let mut g = game();
            g.install = Some(crate::library::TitleFiles {
                install: pf_client_core::library::TitleInstall {
                    state: "installed".into(),
                    size_bytes: Some(26_000_000_000),
                    free_bytes: None,
                },
                download: None,
                action,
            });
            g
        };
        assert!(!rows(&CardMenu::for_game(&host(), &installed(None), None))
            .iter()
            .any(|a| matches!(a, Action::Files(_))));
        let remove = Action::Files(InstallAction::Remove);
        let mut s = CardMenu::for_game(&host(), &installed(Some(InstallAction::Remove)), None);
        assert!(rows(&s).contains(&remove));
        assert_eq!(label(&s, remove), "Remove download \u{b7} 26 GB");
        let mut fx = Outbox::default();
        run_action(&mut s, remove, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        run_action(&mut s, remove, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::Install {
                addr: "10.0.0.5".into(),
                mgmt: 9778,
                fp_hex: "aa".into(),
                app_id: "steam:367520".into(),
                title: "Hollow Knight".into(),
                action: InstallAction::Remove,
            }]
        );
    }

    #[test]
    fn a_posters_menu_keeps_the_shelfs_whole_host_so_a_pinned_cards_preset_survives() {
        let mut s = CardMenu::for_game(&pinned(), &game(), None);
        assert_eq!(s.host_key(), "aa");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Preset(None), &mut fx);
        let intent = fx.connect.expect("a launch");
        assert_eq!(intent.launch.as_deref(), Some("steam:367520"));
        assert_eq!(intent.preset, None, "Default settings names no preset");
        let mut s = CardMenu::on(s.subject.clone(), Mode::Details);
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Play, &mut fx);
        let intent = fx.connect.expect("a launch");
        assert_eq!(
            intent.preset.as_deref(),
            Some("prof-1"),
            "the card's preset"
        );
        assert_eq!(intent.title, "Hollow Knight \u{b7} 4K");
    }

    #[test]
    fn copy_link_always_closes_the_menu_and_says_what_happened() {
        for mut s in [
            CardMenu::for_host(&host()),
            CardMenu::for_game(&host(), &game(), None),
        ] {
            let mut fx = Outbox::default();
            run_action(&mut s, Action::CopyLink, &mut fx);
            assert!(matches!(fx.nav, Some(Nav::Pop)));
            assert!(fx.toast.is_some());
        }
    }

    /// Favorites are this device's, per host, in the settings document; the label follows.
    #[test]
    fn favorite_marks_the_title_on_this_host() {
        let mut s = CardMenu::on(
            CardMenu::for_game(&host(), &game(), None).subject,
            Mode::Details,
        );
        let before = label(&s, Action::Favorite);
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Favorite, &mut fx);
        let marked = crate::library::favorites(&crate::store::file_store().load(), "aa")
            .contains(&"steam:367520".to_string());
        assert_ne!(label(&s, Action::Favorite), before, "the label flips");
        assert!(fx.nav.is_none(), "the card stays up to show it");
        run_action(&mut s, Action::Favorite, &mut fx);
        let again = crate::library::favorites(&crate::store::file_store().load(), "aa")
            .contains(&"steam:367520".to_string());
        assert_ne!(marked, again, "a second press undoes the first");
    }

    /// The label reads back the explicit pointer; pressing the row again clears it.
    #[test]
    fn make_default_sets_then_clears_the_pointer() {
        let saved = HostRow {
            id: Some("rec-1".into()),
            ..host()
        };
        let mut s = details(&saved);
        assert_eq!(label(&s, Action::MakeDefault), "Default host: Off");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::MakeDefault, &mut fx);
        assert_eq!(
            crate::store::file_store().load().default_host.as_deref(),
            Some("rec-1")
        );
        assert_eq!(label(&s, Action::MakeDefault), "Default host: On");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::MakeDefault, &mut fx);
        assert_eq!(crate::store::file_store().load().default_host, None);
    }

    /// A TV has no clipboard: Copy link leaves every menu, and nothing else does.
    #[test]
    fn a_tv_offers_no_copy_link() {
        let menu = CardMenu::for_host(&host());
        let desk = menu.actions(crate::store::file_store(), false);
        let tv = menu.actions(crate::store::file_store(), true);
        assert!(desk.contains(&Action::CopyLink));
        assert!(!tv.contains(&Action::CopyLink));
        assert_eq!(desk.len(), tv.len() + 1);
    }

    /// A paired host's Pairing tab offers Unpair beside Pair again; it arms, then sends one
    /// UnpairHost. An unpaired host has nothing to forget.
    #[test]
    fn unpair_arms_then_forgets_the_identity() {
        let pairing = |h: &HostRow| -> Vec<Action> {
            let d = details(h);
            let sections = d.sections(crate::store::file_store());
            let tab = sections.iter().position(|s| *s == "Pairing");
            rows(&CardMenu {
                tab: tab.expect("a Pairing tab"),
                ..d
            })
        };
        assert_eq!(pairing(&host()), vec![Action::Pair, Action::Unpair]);
        let unpaired = HostRow {
            paired: false,
            ..host()
        };
        assert_eq!(pairing(&unpaired), vec![Action::Pair]);

        let mut s = details(&host());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Unpair, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(label(&s, Action::Unpair), "Unpair \u{2014} press again");
        run_action(&mut s, Action::Unpair, &mut fx);
        assert_eq!(fx.cmds, vec![ConsoleCmd::UnpairHost { key: "aa".into() }]);
    }
}
