//! Context menu for whatever the console is looking at: a saved host, a pinned
//! preset card, or a library title. One screen; the subject names the object
//! and [`OptionsScreen::actions`] owns the verbs.
//!
//! Which face button raises it is per screen (carousel ▲, library X); both
//! legends say "Options". A pinned card offers Unpin only — it is a shortcut,
//! not a second host.
//!
//! Evidence: `design/host-actions.md`. Tests in this module pin the arm-then-fire
//! rule, the host-key NUL split, and the padless Library row.

use crate::glyphs::{Hint, HintKey};
use crate::library::LibraryGame;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, EDGE_INSET, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::start;
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Wake,
    /// Point `Settings::default_host` at this record, or clear it when it already
    /// does. With one paired host it changes nothing today and everything on the
    /// day a second one pairs: an explicit default survives, a derived one drops.
    MakeDefault,
    /// Indexed into [`HostRow::actions`]. Not a variant per id: this build must
    /// render a label the host sent that we have never heard of.
    Host(usize),
    SendLogs,
    /// Stream the host itself, launching nothing — "Resume <title>" when it has a
    /// game up. A shelf's A launches the focused TITLE, so this is the only way
    /// back into a game the host started on its own: an untracked launch has no
    /// catalog entry to press A on.
    Connect,
    /// Same gate as the carousel's Y (saved and paired). Here because a TV
    /// remote has no Y.
    Library,
    /// Measure this host's link. Same gate as [`Action::SendLogs`]: the probe is a
    /// second connect, so an unpaired or unreachable host has nothing to measure.
    SpeedTest,
    CopyLink,
    Edit,
    /// [`Screen::BindPreset`] for the primary tile, or for a library title. Not on
    /// a pin: the pin is the preset.
    BindPreset,
    /// Per-host [`KnownHost::clipboard_sync`]. Lives on the host, not Settings:
    /// the other end of the pipe is this machine.
    Clipboard,
    Forget,
    Unpin,
    Cancel,
}

/// What the menu was raised on. A third kind is a variant plus a row list, not
/// another screen.
pub(crate) enum Subject {
    /// Saved host tile, or a pinned preset card (the pin rides in the row).
    Host(HostRow),
    /// Title on a shelf, with the serving host (pin included) so a link off a
    /// pinned card still streams as that card does.
    Game {
        host: HostRow,
        id: String,
        title: String,
    },
}

pub(crate) struct OptionsScreen {
    /// Subject by value. Discovery rewrites the carousel and the shelf while
    /// this is up; an index or borrow would retarget Forget onto whatever slid
    /// into the slot.
    subject: Subject,
    pub(super) list: MenuList,
    /// Destructive row armed on first press, fires on second. `Option<Action>`
    /// not a bool: arming Forget must not fire Restart if the cursor moved.
    armed: Option<Action>,
}

impl OptionsScreen {
    pub(crate) fn for_host(host: &HostRow) -> OptionsScreen {
        OptionsScreen::on(Subject::Host(host.clone()))
    }

    fn on(subject: Subject) -> OptionsScreen {
        OptionsScreen {
            subject,
            list: MenuList::new(),
            armed: None,
        }
    }

    /// Saved hosts only. A discovered-but-unsaved row is not ours to edit.
    /// Titles skip this: a shelf only exists for a saved host.
    pub(crate) fn available(host: &HostRow) -> bool {
        host.saved
    }

    fn host(&self) -> &HostRow {
        match &self.subject {
            Subject::Host(h) => h,
            Subject::Game { host, .. } => host,
        }
    }

    pub(crate) fn title(&self) -> String {
        match &self.subject {
            Subject::Host(h) => match &h.pin {
                Some(p) => format!("{} \u{b7} {}", h.name, p.name),
                None => h.name.clone(),
            },
            Subject::Game { title, .. } => title.clone(),
        }
    }

    /// What the connecting takeover names for [`Action::Connect`]: the game being
    /// resumed if there is one, else the host — with a pinned card's preset, the
    /// same `host · preset` shape its tile wears.
    fn title_for_connect(&self) -> String {
        let host = self.host();
        let subject = if host.running.is_empty() {
            &host.name
        } else {
            &host.running
        };
        match &host.pin {
            Some(p) => format!("{subject} \u{b7} {}", p.name),
            None => subject.clone(),
        }
    }

    /// Pinned-card keys append the preset id past a NUL (service row builder).
    /// Commands address the host half.
    fn host_key(&self) -> &str {
        let key = self.host().key.as_str();
        key.split('\0').next().unwrap_or(key)
    }

    // Unused: seam for platform-conditional rows. Call sites already pass it.
    fn actions(&self, _platform: crate::platform::Platform) -> Vec<Action> {
        let host = match &self.subject {
            Subject::Host(h) => h,
            // Not Play: the tile's A already launches THIS title. Connect is the other
            // press — it starts nothing — and leads because on a shelf with a game up it
            // is the row you came for. Settings preset is the one place a per-title
            // override can be set, so it ships even with an empty catalog.
            Subject::Game { .. } => {
                return vec![
                    Action::Connect,
                    Action::CopyLink,
                    Action::BindPreset,
                    Action::Cancel,
                ]
            }
        };
        if host.pin.is_some() {
            return vec![Action::Unpin, Action::CopyLink, Action::Cancel];
        }
        let mut a = Vec::new();
        // Online already: Wake would sit there counting seconds.
        if host.can_wake && !host.online {
            a.push(Action::Wake);
        }
        // Needs a store record to point at; a discovered row has no id.
        if host.paired && host.id.is_some() {
            a.push(Action::MakeDefault);
        }
        a.extend((0..host.actions.len()).map(Action::Host));
        // Upload authenticates with the streaming cert and needs a live host;
        // anything else would only toast an error.
        if host.paired && host.online {
            a.push(Action::SendLogs);
        }
        // Same gate as carousel Y. Ahead of Copy link: this row navigates, and
        // on a padless remote it is the only way to the shelf.
        if host.paired && host.saved {
            a.push(Action::Library);
        }
        // A TV box on a powerline adapter is exactly the machine whose link is worth
        // measuring, so the couch surface gets this row too — the touch home and both
        // desktop shells have carried it for far longer.
        if host.paired && host.online {
            a.push(Action::SpeedTest);
        }
        a.extend([
            Action::CopyLink,
            Action::Edit,
            Action::BindPreset,
            Action::Clipboard,
            Action::Forget,
            Action::Cancel,
        ]);
        a
    }

    /// `default` (`Settings::default_host`) names this row. A derived default reads as
    /// unset: pressing the row is what freezes it, and a frozen one has nothing to add.
    fn is_default(&self, default: Option<&str>) -> bool {
        let id = self.host().id.as_deref();
        id.is_some() && default == id
    }

    fn label(&self, a: Action, default: Option<&str>) -> String {
        match a {
            Action::Wake => "Wake host".into(),
            // Geist carries no U+2713, so a check mark here draws as a missing glyph.
            Action::MakeDefault if self.is_default(default) => "Default host (on)".into(),
            Action::MakeDefault => "Make default host".into(),
            Action::Host(i) => match self.host().actions.get(i) {
                Some(act) if self.armed == Some(a) => {
                    format!("{} \u{2014} press again", act.label)
                }
                Some(act) => act.label.clone(),
                None => String::new(),
            },
            Action::SendLogs => "Send logs to host".into(),
            // Names the title when there is one: "Resume" alone would leave the
            // player guessing which game the host means.
            Action::Connect => match self.host().running.as_str() {
                "" => format!("Connect to {}", self.host().name),
                title => format!("Resume {title}"),
            },
            Action::Library => "Library".into(),
            Action::SpeedTest => "Test network speed\u{2026}".into(),
            Action::CopyLink => "Copy link".into(),
            Action::Edit => "Edit\u{2026}".into(),
            Action::BindPreset => match self.subject {
                Subject::Game { .. } => "Settings preset\u{2026}".into(),
                Subject::Host(_) => "Default preset\u{2026}".into(),
            },
            Action::Clipboard => format!(
                "Shared clipboard: {}",
                if self.host().clipboard_sync {
                    "On"
                } else {
                    "Off"
                }
            ),
            Action::Forget if self.armed == Some(Action::Forget) => {
                "Forget \u{2014} press again".into()
            }
            Action::Forget => "Forget".into(),
            Action::Unpin => "Unpin card".into(),
            Action::Cancel => "Cancel".into(),
        }
    }

    /// Host-reported verbs can be unavailable. The row stays; activating it
    /// toasts why. Vanishing would look like the verb never existed.
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Host(i) => self.host().actions.get(i).is_none_or(|act| act.available),
            _ => true,
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let actions = self.actions(ctx.platform);
        let (msg, pulse) = self.list.menu(ev, actions.len());
        self.dispatch(msg, pulse, &actions, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let actions = self.actions(ctx.platform);
        let (msg, pulse) = self.list.pointer(p, actions.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.dispatch(msg, pulse, &actions, ctx, fx);
        true
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
        // Arming is per row. Leaving it must not leave a live trigger on the
        // next destructive row the cursor lands on.
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

    /// `punktfunk://` from the store at activation, never at open. The row may
    /// have left the store while the menu was up.
    fn link(&self, store: &dyn crate::store::SettingsStore) -> Option<String> {
        match &self.subject {
            Subject::Host(h) => crate::screens::host_link(store, h),
            Subject::Game { host, id, .. } => crate::screens::saved_host_link(
                store,
                &host.fp_hex,
                &host.addr,
                host.port,
                host.pin.as_ref().map(|p| p.id.as_str()),
                Some(id.as_str()),
            ),
        }
    }

    fn run(&mut self, action: Action, ctx: &mut Ctx, fx: &mut Outbox) {
        let store = ctx.store;
        let key = self.host_key().to_string();
        match action {
            Action::Wake => {
                fx.cmds.push(ConsoleCmd::Wake {
                    key,
                    then_connect: false,
                });
                fx.pop();
            }
            // Whole-file writer: rebase on the store before mutating, or a setting
            // another screen just wrote is reverted.
            Action::MakeDefault => {
                let on = !self.is_default(ctx.settings.default_host.as_deref());
                let name = self.host().name.clone();
                let id = self.host().id.clone();
                *ctx.settings = ctx.store.load();
                ctx.settings.default_host = on.then_some(id).flatten();
                ctx.store.save(ctx.settings);
                let opens = start::StartIn::parse(&ctx.settings.start_in) != start::StartIn::Hosts;
                fx.toast = Some(match (on, opens) {
                    (true, true) => format!("{name} opens on launch"),
                    (true, false) => format!("{name} is the default host"),
                    (false, _) => format!("{name} is no longer the default host"),
                });
                fx.pop();
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
                // No toast: the takeover the service thread raises IS the feedback, and a
                // notice under it would narrate the same thing twice.
                fx.pop();
            }
            Action::CopyLink => {
                match self.link(store) {
                    Some(url) => {
                        fx.copy = Some(url);
                        fx.toast = Some("Link copied".into());
                    }
                    // Host left the store between open and now.
                    None => fx.toast = Some("This host isn't saved any more".into()),
                }
                fx.pop();
            }
            // No launch id: the host is already showing whatever is up, and asking
            // it to launch the game it is running is how a second copy starts. Pop,
            // so ending the session lands back on the shelf this was raised from.
            Action::Connect => {
                let host = self.host();
                fx.connect = Some(super::ConnectIntent {
                    addr: host.addr.clone(),
                    port: host.port,
                    fp_hex: host.fp_hex.clone(),
                    launch: None,
                    title: self.title_for_connect(),
                    request_access: false,
                    preset: host.pin.as_ref().map(|p| p.id.clone()),
                });
                fx.pop();
            }
            // Fetch, then open on the epoch taken *before* the command drains
            // so this fetch's titles are distinct from ones already in the
            // model. Replace, not push: Back from the shelf is the carousel.
            Action::Library => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::FetchLibrary {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                });
                let epoch = ctx.library.fetch_epoch();
                fx.replace(Screen::Library(super::library::LibraryScreen::new(
                    self.host(),
                    epoch,
                )));
            }
            Action::Edit => fx.replace(Screen::AddHost(super::add_host::AddHostScreen::edit(
                self.host(),
            ))),
            // Same screen either way; the subject decides which binding it writes.
            Action::BindPreset => {
                let host_name = self.host().name.clone();
                let screen = match &self.subject {
                    Subject::Game { id, title, .. } => {
                        super::bind_preset::BindPresetScreen::for_game(
                            key,
                            host_name,
                            super::bind_preset::GameSubject {
                                id: id.clone(),
                                title: title.clone(),
                            },
                            store.presets(),
                        )
                    }
                    Subject::Host(_) => {
                        super::bind_preset::BindPresetScreen::new(key, host_name, store.presets())
                    }
                };
                fx.replace(Screen::BindPreset(screen));
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
            Action::Forget if self.armed != Some(Action::Forget) => {
                self.armed = Some(Action::Forget)
            }
            Action::Forget => {
                fx.cmds.push(ConsoleCmd::ForgetHost { key });
                fx.toast = Some(format!("Forgot {}", self.host().name));
                fx.pop();
            }
            Action::Host(i) => {
                let host = self.host();
                let Some(act) = host.actions.get(i) else {
                    return; // row list changed under the cursor
                };
                // Host says no: toast why rather than send a request it will refuse.
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
                // `danger` (restart, shut down) arms then fires. Sleep is
                // reversible via Wake, so one press.
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
            Action::Cancel => fx.pop(),
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Choose"),
            Hint::new(HintKey::Back, "Close"),
        ]
    }

    fn blurb(&self) -> String {
        match &self.subject {
            Subject::Host(h) if h.pin.is_some() => {
                "This card is a shortcut to one preset on this host. Unpinning it changes \
                 nothing about the host or the preset."
                    .into()
            }
            Subject::Host(_) => "Manage this saved host.".into(),
            Subject::Game { host, .. } => format!("Actions for this title on {}.", host.name),
        }
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        // Air under the title so the first row does not sit on it.
        fonts.leading(
            canvas,
            &self.blurb(),
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.72 * k,
        );
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
            rect.right,
            rect.bottom,
        );
        let rows: Vec<RowSpec> = self
            .actions(ctx.platform)
            .into_iter()
            .map(|a| {
                RowSpec::action(
                    self.label(a, ctx.settings.default_host.as_deref()),
                    self.enabled(a),
                )
            })
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
    }
}

impl OptionsScreen {
    pub(crate) fn for_game(host: &HostRow, game: &LibraryGame) -> OptionsScreen {
        OptionsScreen::on(Subject::Game {
            host: host.clone(),
            id: game.id.clone(),
            title: game.title.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PresetChip;
    use crate::screens::settings::tests::fake_home;
    use crate::screens::Nav;

    /// Drive `run` with a throwaway `Ctx` over a scratch config dir. Settings come
    /// from the store, not `Default`, so a row that writes one reads its own last write.
    fn run_action(s: &mut OptionsScreen, action: Action, fx: &mut Outbox) {
        fake_home();
        let mut settings = crate::store::file_store().load();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &[],
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        s.run(action, &mut ctx, fx);
    }

    fn host() -> HostRow {
        HostRow {
            key: "aa".into(),
            id: None,
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 9778,
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
        }
    }

    #[test]
    fn a_discovered_host_has_no_menu() {
        assert!(OptionsScreen::available(&host()));
        assert!(!OptionsScreen::available(&HostRow {
            saved: false,
            ..host()
        }));
    }

    #[test]
    fn wake_is_offered_only_when_it_would_do_something() {
        let awake = OptionsScreen::for_host(&HostRow {
            can_wake: true,
            online: true,
            ..host()
        });
        assert!(!awake
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::Wake));
        let asleep = OptionsScreen::for_host(&HostRow {
            can_wake: true,
            online: false,
            ..host()
        });
        assert!(asleep
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::Wake));
    }

    #[test]
    fn speed_test_needs_a_paired_host_that_answers() {
        let live = OptionsScreen::for_host(&HostRow {
            paired: true,
            online: true,
            ..host()
        });
        assert!(live
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::SpeedTest));
        // The probe is a second connect: an offline or unpaired host has nothing to measure.
        for h in [
            HostRow {
                paired: true,
                online: false,
                ..host()
            },
            HostRow {
                paired: false,
                online: true,
                ..host()
            },
        ] {
            assert!(!OptionsScreen::for_host(&h)
                .actions(crate::platform::Platform::Desktop)
                .contains(&Action::SpeedTest));
        }
    }

    #[test]
    fn a_pinned_card_offers_no_speed_test() {
        // A pin is a shortcut to one preset, not a second host — same rule as Send logs.
        let pinned = OptionsScreen::for_host(&HostRow {
            paired: true,
            online: true,
            pin: Some(PresetChip {
                id: "prof-1".into(),
                name: "4K".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            ..host()
        });
        assert!(!pinned
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::SpeedTest));
    }

    #[test]
    fn send_logs_is_offered_on_every_platform_with_an_uploader() {
        let reachable = OptionsScreen::for_host(&HostRow {
            paired: true,
            online: true,
            ..host()
        });
        assert!(reachable
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::SendLogs));
        assert!(reachable
            .actions(crate::platform::Platform::Android)
            .contains(&Action::SendLogs));
    }

    #[test]
    fn host_actions_appear_only_when_the_host_offered_them() {
        let none = OptionsScreen::for_host(&host());
        assert!(!none
            .actions(crate::platform::Platform::Desktop)
            .iter()
            .any(|a| matches!(a, Action::Host(_))));

        let s = OptionsScreen::for_host(&powered());
        let rows = s.actions(crate::platform::Platform::Desktop);
        assert_eq!(
            rows.iter().filter(|a| matches!(a, Action::Host(_))).count(),
            3
        );
        assert_eq!(s.label(Action::Host(0), None), "Sleep host");
        // Unknown id still renders: the host sent the label.
        let future = OptionsScreen::for_host(&HostRow {
            actions: vec![crate::model::HostAction {
                id: "plugin:vpn:toggle".into(),
                label: "Toggle the VPN".into(),
                danger: false,
                available: true,
                unavailable_reason: String::new(),
            }],
            ..host()
        });
        assert_eq!(future.label(Action::Host(0), None), "Toggle the VPN");
    }

    /// Sleep fires on one press. Restart/shut down arm. Arming one must not
    /// leave the other live (`armed` is `Option<Action>`, not a bool).
    #[test]
    fn destructive_host_actions_arm_before_they_fire() {
        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.sleep"
        ));

        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(
            s.label(Action::Host(2), None),
            "Shut down host \u{2014} press again"
        );
        assert_eq!(s.label(Action::Host(1), None), "Restart host");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(1), &mut fx);
        assert!(
            fx.cmds.is_empty(),
            "arming shut down must not leave restart armed"
        );
        let mut s = OptionsScreen::for_host(&powered());
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
        let mut s = OptionsScreen::for_host(&HostRow {
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
    fn a_pinned_card_cannot_forget_or_edit_the_host() {
        let s = OptionsScreen::for_host(&pinned());
        assert_eq!(
            s.actions(crate::platform::Platform::Desktop),
            vec![Action::Unpin, Action::CopyLink, Action::Cancel]
        );
        // Commands address the host, not the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    /// Padless path to the shelf (no Y on a TV remote). Saved and paired;
    /// Replace so Back lands on the carousel.
    #[test]
    fn the_library_hangs_off_the_menu_for_a_padless_device() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s
            .actions(crate::platform::Platform::Android)
            .contains(&Action::Library));

        let mut fx = Outbox::default();
        run_action(&mut s, Action::Library, &mut fx);
        assert!(
            matches!(fx.cmds.first(), Some(ConsoleCmd::FetchLibrary { .. })),
            "opening the shelf asks for it first"
        );
        match fx.nav {
            Some(Nav::Replace(screen)) => assert!(matches!(*screen, Screen::Library(_))),
            _ => panic!("expected the shelf to replace the menu"),
        }

        // Unpaired: row absent, not inert.
        let unpaired = OptionsScreen::for_host(&HostRow {
            paired: false,
            ..host()
        });
        assert!(!unpaired
            .actions(crate::platform::Platform::Android)
            .contains(&Action::Library));
    }

    #[test]
    fn default_preset_opens_the_chooser_on_the_hosts_plain_key() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::BindPreset));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::BindPreset, &mut fx);
        match fx.nav {
            Some(crate::screens::Nav::Replace(screen)) => match *screen {
                Screen::BindPreset(b) => assert_eq!(b.host_name(), "Desk"),
                _ => panic!("expected the bind-preset chooser"),
            },
            _ => panic!("expected a replace"),
        }
    }

    #[test]
    fn the_clipboard_toggle_flips_the_stored_state() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s.label(Action::Clipboard, None).ends_with("Off"));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Clipboard, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetClipboard {
                key: "aa".into(),
                on: true,
            }]
        );
        let mut s = OptionsScreen::for_host(&HostRow {
            clipboard_sync: true,
            ..host()
        });
        assert!(s.label(Action::Clipboard, None).ends_with("On"));
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
    fn forget_needs_two_presses() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions(crate::platform::Platform::Desktop);
        let i = actions.iter().position(|a| *a == Action::Forget).unwrap();
        s.list.cursor = i;
        let mut fx = Outbox::default();

        run_action(&mut s, Action::Forget, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(s.armed, Some(Action::Forget));
        assert!(s.label(Action::Forget, None).contains("press again"));

        run_action(&mut s, Action::Forget, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::ForgetHost { key: "aa".into() }],
            "the second press forgets"
        );
    }

    #[test]
    fn leaving_the_forget_row_disarms_it() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions(crate::platform::Platform::Desktop);
        s.armed = Some(Action::Forget);
        s.list.cursor = actions.iter().position(|a| *a == Action::Cancel).unwrap();
        let mut ctx_settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &crate::library::LibraryShared::default(),
            settings: &mut ctx_settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &[],
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let mut fx = Outbox::default();
        s.dispatch(ListMsg::None, None, &actions, &mut ctx, &mut fx);
        assert_eq!(
            s.armed, None,
            "a cursor move off the row cancels the arming"
        );
    }

    /// Still nothing the cover already does — the shelf's A launches this title, and no row
    /// here repeats it. Connect is the other press: it starts nothing.
    #[test]
    fn a_title_offers_the_link_its_preset_and_nothing_its_cover_already_does() {
        let s = OptionsScreen::for_game(&host(), &game());
        // No Play row: the cover's own A launches. Connect and the preset row are the
        // verbs a title owns that nothing else on the shelf offers.
        assert_eq!(
            s.actions(crate::platform::Platform::Desktop),
            vec![
                Action::Connect,
                Action::CopyLink,
                Action::BindPreset,
                Action::Cancel
            ]
        );
        assert_eq!(s.label(Action::BindPreset, None), "Settings preset\u{2026}");
        // Cursor starts at 0: the row that gets you onto the host is under confirm.
        assert_eq!(s.list.cursor, 0);
        assert_eq!(s.title(), "Hollow Knight");
    }

    #[test]
    fn every_menu_can_be_left_without_doing_anything() {
        for s in [
            OptionsScreen::for_host(&host()),
            OptionsScreen::for_host(&pinned()),
            OptionsScreen::for_game(&host(), &game()),
            OptionsScreen::for_game(&pinned(), &game()),
        ] {
            assert_eq!(
                s.actions(crate::platform::Platform::Desktop).last(),
                Some(&Action::Cancel)
            );
        }
    }

    #[test]
    fn a_titles_menu_keeps_the_shelfs_whole_host_so_a_pinned_cards_preset_survives() {
        let s = OptionsScreen::for_game(&pinned(), &game());
        let Subject::Game { host, id, .. } = &s.subject else {
            panic!("built as a title menu");
        };
        assert_eq!(id, "steam:367520", "the link's launch id");
        assert_eq!(
            host.pin.as_ref().map(|p| p.id.as_str()),
            Some("prof-1"),
            "a link taken off a pinned card's shelf carries that card's preset"
        );
        // Host-addressed commands still use the host half of a pin key.
        assert_eq!(s.host_key(), "aa");
    }

    #[test]
    fn copy_link_always_closes_the_menu_and_says_what_happened() {
        // Always Pop. Staying open on failure would leave a row that can only fail again.
        for mut s in [
            OptionsScreen::for_host(&host()),
            OptionsScreen::for_game(&host(), &game()),
        ] {
            let mut fx = Outbox::default();
            run_action(&mut s, Action::CopyLink, &mut fx);
            assert!(matches!(fx.nav, Some(Nav::Pop)));
            assert!(fx.toast.is_some());
        }
    }

    /// The shelf's A launches the focused TITLE; this row is the other press — get me
    /// into whatever the host already has up, or onto the desktop when it has nothing.
    #[test]
    fn a_titles_menu_leads_with_resume_when_the_host_has_a_game_up() {
        let idle = OptionsScreen::for_game(&host(), &game());
        assert_eq!(
            idle.actions(crate::platform::Platform::Desktop).first(),
            Some(&Action::Connect)
        );
        assert_eq!(idle.label(Action::Connect, None), "Connect to Desk");

        let up = OptionsScreen::for_game(
            &HostRow {
                running: "Elden Ring".into(),
                ..host()
            },
            &game(),
        );
        assert_eq!(
            up.label(Action::Connect, None),
            "Resume Elden Ring",
            "naming the title is the whole point of the row"
        );
    }

    /// No launch id: the host is already showing it, and asking it to launch the game it
    /// is running starts a second copy. Pop, so the session ends back on this shelf.
    #[test]
    fn resume_streams_the_host_without_launching_anything() {
        let mut s = OptionsScreen::for_game(
            &HostRow {
                running: "Elden Ring".into(),
                ..pinned()
            },
            &game(),
        );
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Connect, &mut fx);
        let intent = fx.connect.expect("a connect intent");
        assert_eq!(intent.launch, None, "resume must not re-launch the title");
        assert_eq!(intent.addr, "10.0.0.5");
        assert_eq!(
            intent.preset.as_deref(),
            Some("prof-1"),
            "a pinned card's shelf resumes with that card's preset"
        );
        assert_eq!(
            intent.title, "Elden Ring \u{b7} 4K",
            "the takeover names the game, not the host"
        );
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    /// The row points `default_host` at a store record, so it needs one: an unpaired
    /// or merely discovered tile has no id to write, and a pinned card is a shortcut.
    #[test]
    fn make_default_needs_a_paired_record_with_an_id() {
        let desktop = crate::platform::Platform::Desktop;
        let saved = HostRow {
            id: Some("rec-1".into()),
            ..host()
        };
        assert!(OptionsScreen::for_host(&saved)
            .actions(desktop)
            .contains(&Action::MakeDefault));

        for row in [
            HostRow {
                id: None,
                ..saved.clone()
            },
            HostRow {
                paired: false,
                ..saved.clone()
            },
            HostRow {
                pin: Some(PresetChip {
                    id: "prof-1".into(),
                    name: "4K".into(),
                    accent: None,
                    bitrate_kbps: None,
                }),
                ..saved.clone()
            },
        ] {
            assert!(
                !OptionsScreen::for_host(&row)
                    .actions(desktop)
                    .contains(&Action::MakeDefault),
                "offered to a row that cannot be pointed at"
            );
        }
    }

    /// The label reads back the explicit pointer, and pressing the row a second time
    /// clears it — the same press both ways.
    #[test]
    fn make_default_sets_then_clears_the_pointer() {
        let saved = HostRow {
            id: Some("rec-1".into()),
            ..host()
        };
        let s = OptionsScreen::for_host(&saved);
        assert_eq!(s.label(Action::MakeDefault, None), "Make default host");
        assert_eq!(
            s.label(Action::MakeDefault, Some("rec-1")),
            "Default host (on)"
        );
        assert_eq!(
            s.label(Action::MakeDefault, Some("rec-2")),
            "Make default host",
            "another host's pointer is not this row's checkmark"
        );

        let mut s = OptionsScreen::for_host(&saved);
        let mut fx = Outbox::default();
        run_action(&mut s, Action::MakeDefault, &mut fx);
        assert_eq!(
            crate::store::file_store().load().default_host.as_deref(),
            Some("rec-1")
        );
        assert!(matches!(fx.nav, Some(Nav::Pop)));

        let mut fx = Outbox::default();
        run_action(&mut s, Action::MakeDefault, &mut fx);
        assert_eq!(
            crate::store::file_store().load().default_host,
            None,
            "the second press clears it"
        );
    }
}
