//! Who plays: one circle per profile on a host, its name under it (`profiles-and-seats.md`
//! §10.2). Three ways in, one screen:
//!
//! - A connect that needs a pick ([`pf_client_core::profiles::picker_decision`]): OK plays as
//!   the focused profile. Remember on this device, off until turned on, saves the pick.
//! - A host card's profile verb or its menu's Switch profile: OK remembers the focused
//!   profile; Ask every time forgets it, so every connect asks.
//! - The Games tab's host pill: the paired hosts over the circles, which follow the focused
//!   host. OK on a host shows its games; OK on a circle also remembers that profile.
//!
//! Back changes nothing. The focus plate is the shell's.

use crate::el::{Axis, El, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, ProfilesAnswer};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::library::art::ArtDecoder;
use crate::screens::{ConnectIntent, Ctx, Outbox, ProfileAsk, Screen, ScreenView, Seated};
use crate::theme::{fill, Fonts, Frame, PanelStroke, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::profiles::{initials, ListedProfile, ProfilePick};
use skia_safe::{Canvas, Color4f, Image, Matrix, RRect, Rect, TileMode};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

const GRID: &str = "profile-grid";
/// A circle's diameter and a cell's width, design units.
const FACE: f64 = 132.0;
const CELL_W: f64 = 196.0;
const CELL_GAP: f64 = 28.0;
/// The hosts line: a tab's height and label, and the air under it.
const HOST_H: f64 = 38.0;
const HOST_TEXT: f64 = 16.0;
const HOST_PAD: f64 = 10.0;
const HOST_AIR: f64 = 20.0;
/// The Remember switch's pill.
const REMEMBER_H: f64 = 44.0;
const REMEMBER_LABEL: &str = "Remember on this device";

fn card_id(i: usize) -> Id {
    Id::new("profile-card", i)
}

fn host_id(i: usize) -> Id {
    Id::new("profile-host", i)
}

fn hosts_id() -> Id {
    Id::new("profile-hosts", 0)
}

fn remember_id() -> Id {
    Id::new("profile-remember", 0)
}

/// The hosts a Games tab can show: each paired, saved host's own card.
pub(crate) fn paired_hosts(hosts: &[HostRow]) -> impl Iterator<Item = &HostRow> {
    hosts
        .iter()
        .filter(|h| h.paired && h.saved && h.pin.is_none())
}

/// What each paired host listed of its profiles this run, by fingerprint, and the profile
/// each one last played as. The shell fills it from every answer; screens read it.
#[derive(Default)]
pub(crate) struct ProfileBook {
    listed: BTreeMap<String, Vec<ListedProfile>>,
    /// Hosts asked and not yet answered.
    asked: Vec<String>,
    /// The id this run last played as on each host: it leads the next picker there.
    last: BTreeMap<String, String>,
    /// Picture URLs asked since a host last listed its profiles. That list is what retries.
    pictures: BTreeSet<String>,
    /// The shell never asks ([`crate::shell::ConsoleOptions::profiles`]): nothing is listed
    /// and who plays is never a choice.
    silent: bool,
}

/// The book of a test context that has learned nothing.
#[cfg(test)]
pub(crate) static NO_PROFILES: ProfileBook = ProfileBook {
    listed: BTreeMap::new(),
    asked: Vec::new(),
    last: BTreeMap::new(),
    pictures: BTreeSet::new(),
    silent: false,
};

/// Decoded at this scale whatever the window: up to 600 across, twice a face on a 4K screen.
const PICTURE_K: f64 = 2.0;

thread_local! {
    /// Pictures on the drawing thread, by URL. Every screen that draws a face reads this one.
    static PICTURES: RefCell<Pictures> = RefCell::new(Pictures::default());
}

#[derive(Default)]
struct Pictures {
    /// `None`: the bytes wouldn't decode, so the initials stay and nothing asks again.
    decoded: HashMap<String, Option<Image>>,
    decoder: ArtDecoder,
}

/// Decode a picture that arrived, off the thread that draws.
pub(crate) fn offer_picture(url: String, bytes: Arc<[u8]>) {
    PICTURES.with_borrow_mut(|p| p.decoder.want(url, bytes, PICTURE_K));
}

/// Pictures the decoder has finished since the last frame.
pub(crate) fn adopt_pictures() {
    PICTURES.with_borrow_mut(|p| p.decoded.extend(p.decoder.finished()));
}

fn picture_known(url: &str) -> bool {
    PICTURES.with_borrow(|p| p.decoded.contains_key(url))
}

impl ProfileBook {
    pub(crate) fn new(asks: bool) -> ProfileBook {
        ProfileBook {
            silent: !asks,
            ..ProfileBook::default()
        }
    }

    /// The shell asks hosts for their profiles.
    pub(crate) fn asks(&self) -> bool {
        !self.silent
    }

    pub(crate) fn listed(&self, fp: &str) -> Option<&[ListedProfile]> {
        self.listed.get(fp).map(Vec::as_slice)
    }

    /// Who plays on `h` is a choice: the shell asks, and `h` listed two profiles or more or
    /// remembers one.
    pub(crate) fn choice(&self, h: &HostRow) -> bool {
        !self.silent
            && h.paired
            && (h.remembered().is_some() || self.listed(&h.fp_hex).is_some_and(|l| l.len() > 1))
    }

    /// `fp` answered. A failed answer keeps what it listed before.
    pub(crate) fn learn(&mut self, fp: &str, answer: &ProfilesAnswer) {
        self.asked.retain(|f| f != fp);
        match answer {
            ProfilesAnswer::Listed(l) => {
                self.listed.insert(fp.into(), l.clone());
                self.pictures.clear();
            }
            ProfilesAnswer::NoProfiles => drop(self.listed.insert(fp.into(), Vec::new())),
            ProfilesAnswer::Failed(_) => {}
        }
    }

    /// Note an ask of `fp`. `false` when one is already out.
    pub(crate) fn ask(&mut self, fp: &str) -> bool {
        let fresh = !self.asked.iter().any(|f| f == fp);
        if fresh {
            self.asked.push(fp.into());
        }
        fresh
    }

    pub(crate) fn asked(&self) -> &[String] {
        &self.asked
    }

    /// The pictures `h` shows that nothing has asked for: its saved pick's and its rows'. Noted
    /// as asked, so the caller sends each once.
    pub(crate) fn pictures_to_ask(&mut self, h: &HostRow) -> Vec<String> {
        let saved = h.profile.iter().filter_map(|p| p.avatar.as_ref());
        let rows = self.listed.get(&h.fp_hex).into_iter().flatten();
        saved
            .chain(rows.filter_map(|p| p.avatar.as_ref()))
            .filter(|url| !picture_known(url) && self.pictures.insert((*url).clone()))
            .cloned()
            .collect()
    }

    pub(crate) fn note_pick(&mut self, fp: &str, id: &str) {
        self.last.insert(fp.into(), id.into());
    }

    pub(crate) fn last_pick(&self, fp: &str) -> Option<&str> {
        self.last.get(fp).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn with(fp: &str, listed: Vec<ListedProfile>) -> ProfileBook {
        let mut book = ProfileBook::default();
        book.learn(fp, &ProfilesAnswer::Listed(listed));
        book
    }
}

/// What the host has said so far.
enum State {
    Waiting,
    /// The shell never asks: no circles, no line.
    Unasked,
    Listed(Vec<ListedProfile>),
    NoProfiles,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Before a connect: OK plays.
    Connect,
    /// One host's pick: OK remembers.
    Switch,
    /// The Games tab's host and pick.
    Games,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    /// Host `i` of [`paired_hosts`].
    Host(usize),
    /// Circle `i`; the one past the last profile is Ask every time.
    Face(usize),
    Remember,
}

pub(crate) struct ProfilesScreen {
    mode: Mode,
    /// The host whose profiles show.
    ask: ProfileAsk,
    /// That host's pin: the answer the shell feeds is keyed on it.
    fp_hex: String,
    state: State,
    /// The saved pick the host no longer lists.
    gone: Option<String>,
    /// The connect waiting on this pick.
    then: Option<ConnectIntent>,
    /// The id that leads the circles.
    lead: Option<String>,
    /// Connect only: save the pick on this device.
    remember: bool,
    focus: Focus,
    /// Boxed: the screen is a variant of one enum.
    tree: Box<Tree>,
    geom: Vec<(Focus, Rect)>,
}

impl ProfilesScreen {
    /// Who plays on `host`, from its profile verb or menu. `None` for an unpaired card.
    pub(crate) fn switch(host: &HostRow, book: &ProfileBook) -> Option<ProfilesScreen> {
        let mut s = ProfilesScreen::with(Mode::Switch, ProfileAsk::of(host)?, &host.fp_hex);
        s.fill(book);
        Some(s)
    }

    /// The Games tab's host pill: the paired hosts, `host` shown and focused.
    pub(crate) fn games(host: &HostRow, hosts: &[HostRow], book: &ProfileBook) -> ProfilesScreen {
        let key = host.host_key();
        let row = paired_hosts(hosts).find(|h| h.key == key).unwrap_or(host);
        let ask = ProfileAsk::of(row).unwrap_or_else(|| ProfileAsk {
            key: key.into(),
            name: row.name.clone(),
            mgmt: row.mgmt_port,
            saved: None,
        });
        let mut s = ProfilesScreen::with(Mode::Games, ask, &row.fp_hex);
        s.focus = paired_hosts(hosts)
            .position(|h| h.key == key)
            .map_or(Focus::Face(0), Focus::Host);
        s.fill(book);
        s
    }

    /// The picker a connect waits on: `listed` is what the box answered, `gone` the saved
    /// pick it no longer lists, `lead` the id to put first.
    pub(crate) fn before(
        intent: ConnectIntent,
        ask: ProfileAsk,
        listed: Vec<ListedProfile>,
        gone: Option<String>,
        lead: Option<String>,
    ) -> ProfilesScreen {
        let fp = intent.fp_hex.clone();
        let mut s = ProfilesScreen::with(Mode::Connect, ask, &fp);
        s.lead = lead;
        s.set_listed(listed);
        s.gone = gone;
        s.then = Some(intent);
        s
    }

    fn with(mode: Mode, ask: ProfileAsk, fp_hex: &str) -> ProfilesScreen {
        let lead = (ask.saved.as_ref())
            .filter(|p| p.remember)
            .map(|p| p.id.clone());
        ProfilesScreen {
            mode,
            ask,
            fp_hex: fp_hex.into(),
            state: State::Waiting,
            gone: None,
            then: None,
            lead,
            remember: false,
            focus: Focus::Face(0),
            tree: Box::default(),
            geom: Vec::new(),
        }
    }

    /// The command that fills a waiting screen.
    pub(crate) fn fetch(host: &HostRow) -> ConsoleCmd {
        ConsoleCmd::FetchProfiles {
            addr: host.addr.clone(),
            mgmt: host.mgmt_port,
            fp_hex: host.fp_hex.clone(),
        }
    }

    /// This screen, asking `host` for its list when the book had none.
    pub(crate) fn opened(self, host: &HostRow, fx: &mut Outbox) -> Screen {
        if self.waiting() {
            fx.cmds.push(Self::fetch(host));
        }
        Screen::Profiles(self)
    }

    /// The book's list, else wait for one where the shell asks.
    fn fill(&mut self, book: &ProfileBook) {
        match book.listed(&self.fp_hex) {
            Some([]) => self.state = State::NoProfiles,
            Some(l) => self.set_listed(l.to_vec()),
            None if book.silent => self.state = State::Unasked,
            None => self.state = State::Waiting,
        }
    }

    pub(crate) fn fp_hex(&self) -> &str {
        &self.fp_hex
    }

    pub(crate) fn waiting(&self) -> bool {
        matches!(self.state, State::Waiting)
    }

    pub(crate) fn set_answer(&mut self, answer: ProfilesAnswer) {
        match answer {
            ProfilesAnswer::Listed(l) => self.set_listed(l),
            ProfilesAnswer::NoProfiles => self.state = State::NoProfiles,
            ProfilesAnswer::Failed(why) => self.state = State::Failed(why),
        }
    }

    /// The lead first (§10.2), the rest in the host's order.
    fn set_listed(&mut self, mut listed: Vec<ListedProfile>) {
        if listed.is_empty() {
            self.state = State::NoProfiles;
            return;
        }
        if let Some(i) = (self.lead.as_ref()).and_then(|id| listed.iter().position(|p| &p.id == id))
        {
            let p = listed.remove(i);
            listed.insert(0, p);
        }
        self.state = State::Listed(listed);
        if let Focus::Face(i) = self.focus {
            self.focus = Focus::Face(i.min(self.faces().len()));
        }
    }

    fn listed(&self) -> &[ListedProfile] {
        match &self.state {
            State::Listed(l) => l,
            _ => &[],
        }
    }

    /// The circles: every profile before a connect, else only where there is a choice.
    fn faces(&self) -> &[ListedProfile] {
        let l = self.listed();
        match self.mode {
            Mode::Connect => l,
            _ if l.len() > 1 => l,
            _ => &[],
        }
    }

    /// Ask every time follows the circles, outside a connect.
    fn ask_tile(&self) -> bool {
        self.mode != Mode::Connect && !self.faces().is_empty()
    }

    fn remembered(&self) -> Option<&str> {
        (self.ask.saved.as_ref())
            .filter(|p| p.remember)
            .map(|p| p.id.as_str())
    }

    /// Circle `i` is what this host does now: its remembered profile, or Ask every time.
    fn current(&self, i: usize) -> bool {
        if self.mode == Mode::Connect {
            return false;
        }
        let faces = self.faces();
        match faces.get(i) {
            Some(p) => self.remembered() == Some(p.id.as_str()),
            None => !faces
                .iter()
                .any(|p| self.remembered() == Some(p.id.as_str())),
        }
    }

    /// Show `h`'s profiles under the hosts line.
    fn show_host(&mut self, h: &HostRow, book: &ProfileBook, fx: &mut Outbox) {
        let Some(ask) = ProfileAsk::of(h) else {
            return;
        };
        if ask.key == self.ask.key {
            return;
        }
        *self = ProfilesScreen {
            focus: self.focus,
            tree: std::mem::take(&mut self.tree),
            ..ProfilesScreen::with(Mode::Games, ask, &h.fp_hex)
        };
        self.fill(book);
        if self.waiting() {
            fx.cmds.push(Self::fetch(h));
        }
    }

    fn act(&mut self, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let i = match self.focus {
            Focus::Remember => {
                self.remember = !self.remember;
                return Some(MenuPulse::Confirm);
            }
            Focus::Host(i) => {
                fx.shelf = Some(paired_hosts(ctx.hosts).nth(i)?.clone());
                fx.pop();
                return Some(MenuPulse::Confirm);
            }
            Focus::Face(i) => i,
        };
        let key = self.ask.key.clone();
        match (self.faces().get(i).cloned(), self.then.take()) {
            (Some(row), Some(intent)) => {
                let pick = row.pick();
                let keep = self.remember.then(|| ProfilePick {
                    remember: true,
                    ..pick.clone()
                });
                if keep.is_some() || self.ask.saved.is_some() {
                    fx.cmds.push(ConsoleCmd::SetProfile { key, profile: keep });
                }
                fx.connect = Some(ConnectIntent {
                    profile: Some(pick.id),
                    ask: None,
                    seat: Some(Seated {
                        row,
                        mgmt: self.ask.mgmt,
                    }),
                    ..intent
                });
            }
            (Some(row), None) => {
                let pick = ProfilePick {
                    remember: true,
                    ..row.pick()
                };
                fx.toast = Some(format!(
                    "Playing as {} on {}",
                    pick.display_name, self.ask.name
                ));
                fx.cmds.push(ConsoleCmd::SetProfile {
                    key,
                    profile: Some(pick),
                });
            }
            (None, None) if self.ask_tile() && i == self.faces().len() => {
                if self.ask.saved.is_some() {
                    fx.cmds.push(ConsoleCmd::SetProfile { key, profile: None });
                }
                fx.toast = Some(format!(
                    "{} asks who\u{2019}s playing each time",
                    self.ask.name
                ));
            }
            (_, then) => {
                self.then = then;
                return None;
            }
        }
        if self.mode == Mode::Games {
            fx.shelf = paired_hosts(ctx.hosts)
                .find(|h| h.key == self.ask.key)
                .cloned();
        }
        fx.pop();
        Some(MenuPulse::Confirm)
    }

    /// Focus `to`; on the hosts line the circles follow it.
    fn focus_on(&mut self, to: Focus, ctx: &Ctx, fx: &mut Outbox) {
        self.focus = to;
        if let Focus::Host(i) = to {
            if let Some(h) = paired_hosts(ctx.hosts).nth(i) {
                self.show_host(h, ctx.profiles, fx);
            }
        }
    }

    fn step(&mut self, dir: MenuDir, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let to = self.tree.move_focus(dir)?;
        let (to, _) = *self.geom.iter().find(|(f, _)| self.id_of(*f) == to)?;
        self.focus_on(to, ctx, fx);
        Some(MenuPulse::Move)
    }

    fn id_of(&self, f: Focus) -> Id {
        match f {
            Focus::Host(i) => host_id(i),
            Focus::Face(i) => card_id(i),
            Focus::Remember => remember_id(),
        }
    }

    /// The line over the circles: the gone pick, else what stands in for them.
    fn line(&self) -> Option<String> {
        let name = &self.ask.name;
        match &self.state {
            State::Waiting => Some("Loading profiles\u{2026}".into()),
            State::Unasked => None,
            State::NoProfiles => Some(format!("{name} has no profiles.")),
            State::Failed(why) => Some(format!("Couldn't load the profiles \u{2014} {why}")),
            State::Listed(l) if self.faces().is_empty() => l
                .first()
                .map(|p| format!("Everyone on {name} plays as {}.", p.display_name)),
            State::Listed(_) => {
                (self.gone.as_ref()).map(|n| format!("{n} is gone from this host."))
            }
        }
    }

    /// The hosts line, centred while it fits; it scrolls to keep the focused host in view.
    fn hosts_el<'a>(
        &self,
        ui: &'a Frame,
        hosts: &[&'a HostRow],
        fonts: &'a Fonts,
        width: f64,
        k: f64,
    ) -> El<'a> {
        let size = HOST_TEXT * k;
        let gap = 10.0 * k;
        let widths: Vec<f64> = (hosts.iter())
            .map(|h| f64::from(fonts.measure(&h.name, W::Bold, size)) + 2.0 * HOST_PAD * k)
            .collect();
        let run = widths.iter().sum::<f64>() + gap * widths.len().saturating_sub(1) as f64;
        let margin = ui.edge(k);
        let mut x = ((width - run) / 2.0).max(margin);
        let air = (32.0 * k) as f32;
        let h = HOST_H * k;
        let mut row = El::column();
        for (i, (&host, w)) in hosts.iter().zip(widths).enumerate() {
            let shown = host.key == self.ask.key;
            let ink = ui.fg(if shown { 1.0 } else { 0.6 });
            let name = host.name.as_str();
            row = row.child(
                El::paint(move |canvas, r| {
                    crate::widgets::text_tab(canvas, fonts, name, r, size, ink)
                })
                .id(host_id(i))
                .focusable((h / 2.0) as f32)
                .place(Rect::from_xywh(air + x as f32, air, w as f32, h as f32)),
            );
            x += w + gap;
        }
        let span = (x - gap + margin).max(width) as f32 + 2.0 * air;
        El::scroll(hosts_id(), Axis::Horizontal)
            .child(row.size(span, h as f32 + 2.0 * air))
            .place(Rect::from_xywh(
                -air,
                -air,
                width as f32 + 2.0 * air,
                h as f32 + 2.0 * air,
            ))
    }
}

impl ScreenView for ProfilesScreen {
    fn title(&self) -> String {
        match self.mode {
            Mode::Games => "Host and profile".into(),
            _ => format!("Who\u{2019}s playing on {}?", self.ask.name),
        }
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Move(dir) => self.step(dir, ctx, fx).or(Some(MenuPulse::Boundary)),
            MenuEvent::Confirm => self.act(ctx, fx).or(Some(MenuPulse::Boundary)),
            MenuEvent::Back => {
                fx.pop();
                None
            }
            _ => None,
        }
    }

    fn press(&mut self) {
        self.tree.press();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.tree.drag(hosts_id(), p) || self.tree.drag(Id::new(GRID, 0), p)
    }

    /// Hover focuses; a press on what has focus acts. The switch acts on the first press.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let hit = self.geom.iter().find(|(_, r)| p.hits(*r)).map(|(f, _)| *f);
        match p.kind {
            PointerKind::Scroll { up } => {
                self.tree
                    .pan(Id::new(GRID, 0), if up { -80.0 } else { 80.0 });
                self.tree.release(Id::new(GRID, 0), 0.0);
                true
            }
            PointerKind::Move => match hit {
                Some(f) if f != self.focus => {
                    self.focus_on(f, ctx, fx);
                    true
                }
                _ => false,
            },
            PointerKind::Press => match hit {
                Some(f) if f == self.focus || f == Focus::Remember => {
                    self.focus = f;
                    self.act(ctx, fx);
                    true
                }
                Some(f) => {
                    self.focus_on(f, ctx, fx);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        let selected = |say: String, on: bool| match on {
            true => format!("{say}, selected"),
            false => say,
        };
        match self.focus {
            Focus::Remember => Some(format!(
                "{REMEMBER_LABEL}, {}",
                if self.remember { "on" } else { "off" }
            )),
            Focus::Host(i) => paired_hosts(ctx.hosts).nth(i).map(|h| h.name.clone()),
            Focus::Face(i) => match self.faces().get(i) {
                Some(p) => {
                    let say = selected(p.display_name.clone(), self.current(i));
                    Some(match p.note() {
                        Some(note) => format!("{say}, {note}"),
                        None => say,
                    })
                }
                None if self.ask_tile() => Some(selected("Ask every time".into(), self.current(i))),
                None => None,
            },
        }
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let ok = match self.focus {
            Focus::Remember => Some("Change"),
            Focus::Host(_) => Some("Open"),
            Focus::Face(_) if self.then.is_some() && !self.faces().is_empty() => Some("Play"),
            Focus::Face(_) if !self.faces().is_empty() => Some("Select"),
            Focus::Face(_) => None,
        };
        let mut hints: Vec<Hint> = ok
            .map(|v| Hint::new(HintKey::Confirm, v))
            .into_iter()
            .collect();
        hints.push(Hint::new(HintKey::Back, "Back"));
        hints
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
        let ui = ctx.ui;
        let width = f64::from(rect.width());
        let hosts: Vec<&HostRow> = match self.mode {
            Mode::Games => paired_hosts(ctx.hosts).collect(),
            _ => Vec::new(),
        };
        let mut tree = std::mem::take(&mut self.tree);
        tree.tick(dt as f32);
        let mut root = El::column();
        let mut top = 0.0;
        if !hosts.is_empty() {
            root = root.child(self.hosts_el(ui, &hosts, fonts, width, k));
            top += (HOST_H + HOST_AIR) * k;
        }
        // The line sits centred over the circles it speaks for.
        if let Some(line) = self.line() {
            let (size, max_w) = (15.0 * k, width - 2.0 * ui.edge(k));
            let tw = f64::from(fonts.measure(&line, W::Regular, size)).min(max_w);
            let x = f64::from(rect.center_x()) - tw / 2.0;
            let baseline = f64::from(rect.top) + top + 20.0 * k;
            fonts.draw_clipped(
                canvas,
                &line,
                x,
                baseline,
                W::Regular,
                size,
                ui.fg(0.7),
                max_w,
            );
            top += 32.0 * k;
        }
        if self.waiting() {
            let cy = f64::from(rect.top) + top + 60.0 * k;
            crate::theme::spinner(canvas, ui, f64::from(rect.center_x()), cy, 14.0 * k, ctx.t);
        }
        let switch = self.mode == Mode::Connect && !self.faces().is_empty();
        let foot = if switch { (REMEMBER_H + 24.0) * k } else { 0.0 };
        let grid_h = (f64::from(rect.height()) - top - foot).max(0.0);
        let faces = self.faces();
        let cells = faces.len() + usize::from(self.ask_tile());
        let grid = Id::new(GRID, 0);
        // How much of the circles' room they fill: the switch sits right under them.
        let mut used = 0.0;
        if cells > 0 {
            let avail = width - 2.0 * ui.edge(k);
            let gap = CELL_GAP * k;
            let cols = (((avail + gap) / (CELL_W * k + gap)).floor() as usize).clamp(1, cells);
            let cw = ((avail - gap * (cols - 1) as f64) / cols as f64).min(CELL_W * k);
            let face = (FACE * k).min(cw * 0.8);
            let grid_w = cw * cols as f64 + gap * (cols - 1) as f64;
            let left = (width - grid_w) / 2.0;
            let air = (24.0 * k) as f32;
            let lines = cells.div_ceil(cols) as f64;
            let cell_h = face + (14.0 + 52.0) * k;
            used = (lines * cell_h + (lines - 1.0) * gap + 2.0 * f64::from(air)).min(grid_h);
            let current: Vec<bool> = (0..cells).map(|i| self.current(i)).collect();
            let current: &[bool] = &current;
            let cells_at: Vec<usize> = (0..cells).collect();
            let rows = cells_at.chunks(cols).map(move |chunk| {
                El::row()
                    .gap(gap as f32)
                    .children(chunk.iter().map(move |&i| {
                        let chosen = current[i];
                        let p = faces.get(i);
                        El::column()
                            .gap((14.0 * k) as f32)
                            .style(|s| s.align_items = Some(taffy::AlignItems::CENTER))
                            .child(
                                El::paint(move |canvas, cell| {
                                    let (cx, cy) = (cell.center_x(), cell.center_y());
                                    let r = f64::from(cell.width()) / 2.0;
                                    match p {
                                        Some(p) => draw_face(
                                            canvas,
                                            ui,
                                            fonts,
                                            &p.display_name,
                                            p.accent.as_deref(),
                                            p.avatar.as_deref(),
                                            cx,
                                            cy,
                                            r,
                                        ),
                                        None => draw_ask(canvas, ui, fonts, cx, cy, r),
                                    }
                                    if chosen {
                                        let ring = (3.0 * k) as f32;
                                        let mut paint = crate::theme::stroke(ui.fg(0.9), ring);
                                        paint.set_anti_alias(true);
                                        canvas.draw_circle((cx, cy), r as f32 + ring * 2.0, &paint);
                                    }
                                })
                                .id(card_id(i))
                                .focusable((face / 2.0) as f32)
                                .size(face as f32, face as f32),
                            )
                            .child(
                                El::paint(move |canvas, cell| match p {
                                    Some(p) => draw_caption(canvas, ui, fonts, p, cell, k),
                                    None => {
                                        draw_label(canvas, ui, fonts, "Ask every time", cell, k)
                                    }
                                })
                                .size(cw as f32, (52.0 * k) as f32),
                            )
                    }))
            });
            root = root.child(
                El::scroll(grid, Axis::Vertical)
                    .gap(gap as f32)
                    .style(|s| {
                        s.align_items = Some(taffy::AlignItems::START);
                        s.padding.left = taffy::LengthPercentage::length(left as f32);
                        s.padding.top = taffy::LengthPercentage::length(air);
                        s.padding.bottom = taffy::LengthPercentage::length(air);
                    })
                    .children(rows)
                    .place(Rect::from_xywh(
                        0.0,
                        top as f32,
                        width as f32,
                        grid_h as f32,
                    )),
            );
        }
        if switch {
            let on = self.remember;
            let w = f64::from(fonts.measure(REMEMBER_LABEL, W::SemiBold, 15.0 * k)) + 92.0 * k;
            let r = Rect::from_xywh(
                ((width - w) / 2.0) as f32,
                (top + used + 8.0 * k) as f32,
                w as f32,
                (REMEMBER_H * k) as f32,
            );
            root = root.child(
                El::paint(move |canvas, r| draw_remember(canvas, ui, fonts, on, r, k))
                    .id(remember_id())
                    .focusable((REMEMBER_H * k / 2.0) as f32)
                    .place(r),
            );
        }
        let frame = tree.layout(root, rect);
        // The tree glides the circles to centre the focused row; a finger parks them.
        if let (Focus::Face(i), Some((view, max))) = (self.focus, frame.scroll(grid)) {
            if let Some(r) = frame.rect(card_id(i)) {
                tree.follow(
                    grid,
                    (r.center_y() - view.center_y()).clamp(0.0, max),
                    ui.reduce_motion,
                );
            }
        }
        if let (Focus::Host(i), Some((view, max))) = (self.focus, frame.scroll(hosts_id())) {
            if let Some(r) = frame.rect(host_id(i)) {
                tree.follow(
                    hosts_id(),
                    (r.center_x() - view.center_x()).clamp(0.0, max),
                    ui.reduce_motion,
                );
            }
        }
        tree.set_focus(Some(self.id_of(self.focus)));
        let cheap = super::settings::rows::reduce_ui_res(
            ctx.settings,
            ctx.device.platform,
            ctx.device.fallback_ui,
        );
        tree.paint_focus(canvas, ui, frame, k as f32, dt, cheap);
        let targets = (0..hosts.len())
            .map(Focus::Host)
            .chain((0..cells).map(Focus::Face))
            .chain(switch.then_some(Focus::Remember));
        self.geom = targets
            .filter_map(|f| Some((f, tree.rect(self.id_of(f))?)))
            .collect();
        self.tree = tree;
    }
}

/// The name under a circle, and its one line where it has one (`ListedProfile::note`).
fn draw_caption(canvas: &Canvas, ui: &Frame, fonts: &Fonts, p: &ListedProfile, cell: Rect, k: f64) {
    draw_label(canvas, ui, fonts, &p.display_name, cell, k);
    if let Some(note) = p.note() {
        let w = f64::from(cell.width());
        let size = 14.0 * k;
        let tw = f64::from(fonts.measure(&note, W::Regular, size)).min(w);
        let x = f64::from(cell.center_x()) - tw / 2.0;
        let base = f64::from(cell.top) + 42.0 * k;
        fonts.draw_clipped(canvas, &note, x, base, W::Regular, size, ui.fg(0.6), w);
    }
}

/// A circle's name, bold and centred at the top of `cell`.
fn draw_label(canvas: &Canvas, ui: &Frame, fonts: &Fonts, text: &str, cell: Rect, k: f64) {
    let w = f64::from(cell.width());
    let size = 19.0 * k;
    let tw = f64::from(fonts.measure(text, W::Bold, size)).min(w);
    let x = f64::from(cell.center_x()) - tw / 2.0;
    let base = f64::from(cell.top) + 20.0 * k;
    fonts.draw_clipped(canvas, text, x, base, W::Bold, size, ui.fg(1.0), w);
}

/// Ask every time's circle: a question mark on the field.
fn draw_ask(canvas: &Canvas, ui: &Frame, fonts: &Fonts, cx: f32, cy: f32, r: f64) {
    canvas.draw_circle((cx, cy), r as f32, &fill(ui.fg(0.1)));
    let size = r * 0.78;
    let tw = f64::from(fonts.measure("?", W::Bold, size));
    let (x, y) = (f64::from(cx) - tw / 2.0, f64::from(cy) + size * 0.36);
    fonts.draw(canvas, "?", x, y, W::Bold, size, ui.fg(0.9));
}

/// The Remember pill: its label, then a switch.
fn draw_remember(canvas: &Canvas, ui: &Frame, fonts: &Fonts, on: bool, r: Rect, k: f64) {
    let corner = (f64::from(r.height()) / 2.0 / k) as f32;
    crate::theme::panel(
        canvas,
        ui,
        r,
        corner,
        None,
        PanelStroke::Plain(0.08),
        k as f32,
    );
    let size = 15.0 * k;
    let (x, cy) = (f64::from(r.left) + 20.0 * k, f64::from(r.center_y()));
    fonts.draw(
        canvas,
        REMEMBER_LABEL,
        x,
        cy + size * 0.36,
        W::SemiBold,
        size,
        ui.fg(0.95),
    );
    let (tw, th) = (36.0 * k, 20.0 * k);
    let track = Rect::from_xywh(
        (f64::from(r.right) - 18.0 * k - tw) as f32,
        (cy - th / 2.0) as f32,
        tw as f32,
        th as f32,
    );
    let ink = if on { ui.accent(1.0) } else { ui.fg(0.25) };
    let half = (th / 2.0) as f32;
    canvas.draw_rrect(RRect::new_rect_xy(track, half, half), &fill(ink));
    let kx = f64::from(track.left) + th / 2.0 + if on { tw - th } else { 0.0 };
    let knob = Color4f::new(1.0, 1.0, 1.0, 1.0);
    canvas.draw_circle(
        (kx as f32, cy as f32),
        (th / 2.0 - 3.0 * k) as f32,
        &fill(knob),
    );
}

/// `#RRGGBB`, else the palette's accent.
fn accent_of(ui: &Frame, hex: Option<&str>) -> Color4f {
    let v = hex
        .and_then(|a| a.strip_prefix('#'))
        .filter(|h| h.len() == 6)
        .and_then(|h| u32::from_str_radix(h, 16).ok());
    match v {
        Some(v) => Color4f::new(
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
            1.0,
        ),
        None => ui.accent(1.0),
    }
}

/// The picture at `url` filling a circle, cropped about its centre. `false` while there is none.
fn draw_picture(canvas: &Canvas, url: &str, cx: f32, cy: f32, r: f32) -> bool {
    PICTURES.with_borrow(|p| {
        let Some(Some(img)) = p.decoded.get(url) else {
            return false;
        };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let scale = 2.0 * r / iw.min(ih);
        let mut local = Matrix::scale((scale, scale));
        local.post_translate((cx - iw * scale / 2.0, cy - ih * scale / 2.0));
        let tile = (TileMode::Clamp, TileMode::Clamp);
        let Some(shader) = img.to_shader(tile, crate::theme::art_sampling(), Some(&local)) else {
            return false;
        };
        let mut paint = crate::theme::shaded();
        paint.set_anti_alias(true);
        paint.set_shader(shader);
        canvas.draw_circle((cx, cy), r, &paint);
        true
    })
}

/// A profile's face in a circle of radius `r` at `cx, cy`: its picture, else its initials on
/// its accent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_face(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    name: &str,
    accent: Option<&str>,
    picture: Option<&str>,
    cx: f32,
    cy: f32,
    r: f64,
) {
    if picture.is_some_and(|url| draw_picture(canvas, url, cx, cy, r as f32)) {
        return;
    }
    let bg = accent_of(ui, accent);
    canvas.draw_circle((cx, cy), r as f32, &fill(bg));
    let luma = crate::theme::luma((f64::from(bg.r), f64::from(bg.g), f64::from(bg.b)));
    let ink = if luma > 0.6 {
        Color4f::new(0.06, 0.06, 0.09, 1.0)
    } else {
        Color4f::new(1.0, 1.0, 1.0, 1.0)
    };
    let letters = initials(name);
    let letters = if letters.is_empty() {
        "?".into()
    } else {
        letters
    };
    let size = r * 0.78;
    let tw = f64::from(fonts.measure(&letters, W::Bold, size));
    fonts.draw(
        canvas,
        &letters,
        f64::from(cx) - tw / 2.0,
        f64::from(cy) + size * 0.36,
        W::Bold,
        size,
        ink,
    );
}

/// A remembered pick, small: its face on the palette's accent; `None` is Ask every time.
pub(crate) fn draw_pick(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    pick: Option<&ProfilePick>,
    cx: f32,
    cy: f32,
    r: f64,
) {
    match pick {
        Some(p) => draw_face(
            canvas,
            ui,
            fonts,
            &p.display_name,
            None,
            p.avatar.as_deref(),
            cx,
            cy,
            r,
        ),
        None => draw_ask(canvas, ui, fonts, cx, cy, r),
    }
}

#[cfg(test)]
mod tests;
