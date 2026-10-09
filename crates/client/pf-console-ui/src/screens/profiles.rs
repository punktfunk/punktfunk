//! Who plays: one circle per profile on a host, its name under it (`profiles-and-seats.md`
//! §10.2). Reached from the host menu's Switch profile, and raised by the shell when a
//! connect needs a pick first ([`pf_client_core::profiles::picker_decision`]).
//!
//! OK saves the pick on the host's record and, when a connect is waiting, continues it as
//! that profile. Back leaves the saved pick as it was. The focus plate is the shell's.

use crate::anim::approach;
use crate::el::{Axis, El, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, ProfilesAnswer};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, ProfileAsk, ScreenView, Seated};
use crate::theme::{edge, fg, fill, Fonts, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::profiles::{initials, ListedProfile, ProfilePick};
use skia_safe::{Canvas, Color4f, Rect};

const GRID: &str = "profile-grid";
/// A circle's diameter and a cell's width, design units.
const FACE: f64 = 132.0;
const CELL_W: f64 = 196.0;
const CELL_GAP: f64 = 28.0;

fn card_id(i: usize) -> Id {
    Id::new("profile-card", i)
}

/// What the host has said so far.
enum State {
    Waiting,
    Listed(Vec<ListedProfile>),
    NoProfiles,
    Failed(String),
}

pub(crate) struct ProfilesScreen {
    ask: ProfileAsk,
    /// The host's pin: the answer the shell feeds is keyed on it.
    fp_hex: String,
    state: State,
    /// The saved pick the host no longer lists.
    gone: Option<String>,
    /// The connect waiting on this pick.
    then: Option<ConnectIntent>,
    cursor: usize,
    /// Boxed: the screen is a variant of one enum.
    tree: Box<Tree>,
    geom: Vec<Rect>,
}

impl ProfilesScreen {
    /// Switch profile on `host`: waits for the list [`Self::fetch`] asked for.
    pub(crate) fn switch(host: &HostRow) -> Option<ProfilesScreen> {
        let ask = ProfileAsk::of(host)?;
        Some(ProfilesScreen::with(
            ask,
            host.fp_hex.clone(),
            State::Waiting,
        ))
    }

    /// The command that fills [`Self::switch`].
    pub(crate) fn fetch(host: &HostRow) -> ConsoleCmd {
        ConsoleCmd::FetchProfiles {
            addr: host.addr.clone(),
            mgmt: host.mgmt_port,
            fp_hex: host.fp_hex.clone(),
        }
    }

    /// The picker a connect waits on: `listed` is what the box answered, `gone` the saved
    /// pick it no longer lists.
    pub(crate) fn before(
        intent: ConnectIntent,
        ask: ProfileAsk,
        listed: Vec<ListedProfile>,
        gone: Option<String>,
    ) -> ProfilesScreen {
        let mut s = ProfilesScreen::with(ask, intent.fp_hex.clone(), State::Waiting);
        s.set_listed(listed);
        s.gone = gone;
        s.then = Some(intent);
        s
    }

    fn with(ask: ProfileAsk, fp_hex: String, state: State) -> ProfilesScreen {
        ProfilesScreen {
            ask,
            fp_hex,
            state,
            gone: None,
            then: None,
            cursor: 0,
            tree: Box::default(),
            geom: Vec::new(),
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

    /// The saved pick first (§10.2), the rest in the host's order; focus on the first.
    fn set_listed(&mut self, mut listed: Vec<ListedProfile>) {
        if listed.is_empty() {
            self.state = State::NoProfiles;
            return;
        }
        if let Some(saved) = &self.ask.saved {
            if let Some(i) = listed.iter().position(|p| p.id == saved.id) {
                let p = listed.remove(i);
                listed.insert(0, p);
            }
        }
        self.cursor = 0;
        self.state = State::Listed(listed);
    }

    fn listed(&self) -> &[ListedProfile] {
        match &self.state {
            State::Listed(l) => l,
            _ => &[],
        }
    }

    /// Save the focused profile; a waiting connect goes on as it.
    fn choose(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        let row = self.listed().get(self.cursor)?.clone();
        let pick = row.pick();
        fx.cmds.push(ConsoleCmd::SetProfile {
            key: self.ask.key.clone(),
            profile: Some(pick.clone()),
        });
        match self.then.take() {
            Some(intent) => {
                fx.connect = Some(ConnectIntent {
                    profile: Some(pick.id),
                    ask: None,
                    seat: Some(Seated {
                        row,
                        mgmt: self.ask.mgmt,
                    }),
                    ..intent
                })
            }
            None => {
                fx.toast = Some(format!(
                    "Playing as {} on {}",
                    pick.display_name, self.ask.name
                ))
            }
        }
        fx.pop();
        Some(MenuPulse::Confirm)
    }

    fn step(&mut self, dir: MenuDir) -> Option<MenuPulse> {
        let to = self.tree.move_focus(dir)?;
        let i = (0..self.listed().len()).find(|&i| card_id(i) == to)?;
        self.cursor = i;
        Some(MenuPulse::Move)
    }

    /// The line over the grid: the gone pick, else what stands in for a list.
    fn line(&self) -> Option<String> {
        match &self.state {
            State::Waiting => Some("Loading profiles\u{2026}".into()),
            State::NoProfiles => Some(format!("{} has no profiles.", self.ask.name)),
            State::Failed(why) => Some(format!("Couldn't load the profiles \u{2014} {why}")),
            State::Listed(_) => self
                .gone
                .as_ref()
                .map(|name| format!("{name} is gone from this host.")),
        }
    }
}

impl ScreenView for ProfilesScreen {
    fn title(&self) -> String {
        format!("Who\u{2019}s playing on {}?", self.ask.name)
    }

    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Move(dir) => self.step(dir).or(Some(MenuPulse::Boundary)),
            MenuEvent::Confirm => self.choose(fx).or(Some(MenuPulse::Boundary)),
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
        self.tree.drag(Id::new(GRID, 0), p)
    }

    /// Hover focuses; a press on the focused circle picks it.
    fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match p.kind {
            PointerKind::Scroll { up } => {
                self.tree
                    .pan(Id::new(GRID, 0), if up { -80.0 } else { 80.0 });
                self.tree.release(Id::new(GRID, 0), 0.0);
                true
            }
            PointerKind::Move => match p.pick(&self.geom) {
                Some(i) if i != self.cursor => {
                    self.cursor = i;
                    true
                }
                _ => false,
            },
            PointerKind::Press => match p.pick(&self.geom) {
                Some(i) if i == self.cursor => {
                    self.choose(fx);
                    true
                }
                Some(i) => {
                    self.cursor = i;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        let p = self.listed().get(self.cursor)?;
        let mut say = p.display_name.clone();
        if self.ask.saved.as_ref().is_some_and(|s| s.id == p.id) {
            say.push_str(", selected");
        }
        if let Some(note) = p.note() {
            say = format!("{say}, {note}");
        }
        Some(say)
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let mut hints = Vec::new();
        if !self.listed().is_empty() {
            let verb = if self.then.is_some() {
                "Play"
            } else {
                "Select"
            };
            hints.push(Hint::new(HintKey::Confirm, verb));
        }
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
        let mut rect = rect;
        // The line sits centred over the grid it speaks for.
        if let Some(line) = self.line() {
            let (size, max_w) = (15.0 * k, f64::from(rect.width()) - 2.0 * edge(k));
            let tw = f64::from(fonts.measure(&line, W::Regular, size)).min(max_w);
            let x = f64::from(rect.center_x()) - tw / 2.0;
            let baseline = f64::from(rect.top) + 20.0 * k;
            fonts.draw_clipped(canvas, &line, x, baseline, W::Regular, size, fg(0.7), max_w);
            rect.top = (baseline + 12.0 * k) as f32;
        }
        if self.waiting() {
            crate::theme::spinner(
                canvas,
                f64::from(rect.center_x()),
                f64::from(rect.top) + 60.0 * k,
                14.0 * k,
                ctx.t,
            );
        }
        let listed = match &self.state {
            State::Listed(l) => l,
            _ => {
                self.geom.clear();
                return;
            }
        };
        let grid = Id::new(GRID, 0);
        let avail = f64::from(rect.width()) - 2.0 * edge(k);
        let gap = CELL_GAP * k;
        let cols =
            (((avail + gap) / (CELL_W * k + gap)).floor() as usize).clamp(1, listed.len().max(1));
        let cw = ((avail - gap * (cols - 1) as f64) / cols as f64).min(CELL_W * k);
        let face = (FACE * k).min(cw * 0.8);
        let grid_w = cw * cols as f64 + gap * (cols - 1) as f64;
        let left = (f64::from(rect.width()) - grid_w) / 2.0;
        let air = (24.0 * k) as f32;

        let saved = self.ask.saved.as_ref().map(|s| s.id.as_str());
        let mut tree = std::mem::take(&mut self.tree);
        tree.tick(dt as f32);
        let rows = listed.chunks(cols).enumerate().map(|(r, chunk)| {
            El::row()
                .gap(gap as f32)
                .children(chunk.iter().enumerate().map(move |(c, p)| {
                    let i = r * cols + c;
                    let chosen = saved == Some(p.id.as_str());
                    El::column()
                        .gap((14.0 * k) as f32)
                        .style(|s| s.align_items = Some(taffy::AlignItems::CENTER))
                        .child(
                            El::paint(move |canvas, cell| {
                                let (cx, cy) = (cell.center_x(), cell.center_y());
                                let r = f64::from(cell.width()) / 2.0;
                                draw_face(
                                    canvas,
                                    fonts,
                                    &p.display_name,
                                    p.accent.as_deref(),
                                    cx,
                                    cy,
                                    r,
                                );
                                if chosen {
                                    let ring = (3.0 * k) as f32;
                                    let mut paint = crate::theme::stroke(fg(0.9), ring);
                                    paint.set_anti_alias(true);
                                    canvas.draw_circle((cx, cy), r as f32 + ring * 2.0, &paint);
                                }
                            })
                            .id(card_id(i))
                            .focusable((face / 2.0) as f32)
                            .size(face as f32, face as f32),
                        )
                        .child(
                            El::paint(move |canvas, cell| {
                                draw_caption(canvas, fonts, p, cell, k);
                            })
                            .size(cw as f32, (52.0 * k) as f32),
                        )
                }))
        });
        let root = El::scroll(grid, Axis::Vertical)
            .gap(gap as f32)
            .style(|s| {
                s.align_items = Some(taffy::AlignItems::START);
                s.padding.left = taffy::LengthPercentage::length(left as f32);
                s.padding.top = taffy::LengthPercentage::length(air);
                s.padding.bottom = taffy::LengthPercentage::length(air);
            })
            .children(rows);
        let frame = tree.layout(root, rect);
        let (_, max) = frame.scroll(grid).expect("the grid is a scroll");
        let target = frame
            .rect(card_id(self.cursor))
            .map_or(0.0, |r| (r.center_y() - rect.center_y()).clamp(0.0, max));
        if !tree.moving(grid) {
            let next = approach(f64::from(tree.offset(grid)), f64::from(target), dt, 0.08) as f32;
            tree.set_offset(
                grid,
                if (next - target).abs() < 0.25 {
                    target
                } else {
                    next
                },
            );
        }
        tree.set_focus(Some(card_id(self.cursor)));
        let cheap = super::settings::rows::reduce_ui_res(
            ctx.settings,
            ctx.device.platform,
            ctx.device.fallback_ui,
        );
        tree.paint_focus(canvas, frame, k as f32, dt, cheap);
        self.geom = (0..listed.len())
            .map(|i| tree.rect(card_id(i)).unwrap_or_else(Rect::new_empty))
            .collect();
        self.tree = tree;
    }
}

/// The name under a circle, and its one line where it has one (`ListedProfile::note`).
fn draw_caption(canvas: &Canvas, fonts: &Fonts, p: &ListedProfile, cell: Rect, k: f64) {
    let w = f64::from(cell.width());
    let centred = |text: &str, weight: W, size: f64, ink: Color4f, baseline: f64| {
        let tw = f64::from(fonts.measure(text, weight, size)).min(w);
        let x = f64::from(cell.center_x()) - tw / 2.0;
        fonts.draw_clipped(canvas, text, x, baseline, weight, size, ink, w);
    };
    let top = f64::from(cell.top);
    centred(&p.display_name, W::Bold, 19.0 * k, fg(1.0), top + 20.0 * k);
    if let Some(note) = p.note() {
        centred(&note, W::Regular, 14.0 * k, fg(0.6), top + 42.0 * k);
    }
}

/// `#RRGGBB`, else the palette's accent.
fn accent_of(hex: Option<&str>) -> Color4f {
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
        None => crate::theme::accent(1.0),
    }
}

/// A profile's face: its initials on its accent, in a circle of radius `r` at `cx, cy`.
pub(crate) fn draw_face(
    canvas: &Canvas,
    fonts: &Fonts,
    name: &str,
    accent: Option<&str>,
    cx: f32,
    cy: f32,
    r: f64,
) {
    let bg = accent_of(accent);
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

/// The saved pick on a host card: its face, small, on the palette's accent.
pub(crate) fn draw_pick(
    canvas: &Canvas,
    fonts: &Fonts,
    pick: &ProfilePick,
    cx: f32,
    cy: f32,
    r: f64,
) {
    draw_face(canvas, fonts, &pick.display_name, None, cx, cy, r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::Nav;
    use pf_client_core::profiles::{Seat, SeatState};
    use pf_client_core::trust::Settings;

    fn profile(id: &str, name: &str) -> ListedProfile {
        ListedProfile {
            id: id.into(),
            display_name: name.into(),
            ..Default::default()
        }
    }

    fn host(saved: Option<&str>) -> HostRow {
        HostRow {
            profile: saved.map(|id| ProfilePick {
                id: id.into(),
                display_name: "Kid".into(),
            }),
            ..HostRow::fixture("aa", "Desk")
        }
    }

    fn listed() -> Vec<ListedProfile> {
        vec![profile("own", "Ben"), profile("kid", "Kid")]
    }

    fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        f(&mut ctx)
    }

    /// The saved pick leads; OK saves the focused one and says so; nothing connects.
    #[test]
    fn switch_puts_the_saved_pick_first_and_saves_the_choice() {
        let mut s = ProfilesScreen::switch(&host(Some("kid"))).expect("a paired host");
        assert!(s.waiting());
        s.set_answer(ProfilesAnswer::Listed(listed()));
        assert_eq!(s.listed()[0].id, "kid");
        assert_eq!(
            with_ctx(|ctx| s.announcement(ctx)).as_deref(),
            Some("Kid, selected")
        );
        s.cursor = 1;
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetProfile {
                key: "aa".into(),
                profile: Some(ProfilePick {
                    id: "own".into(),
                    display_name: "Ben".into()
                }),
            }]
        );
        assert!(fx.connect.is_none());
        assert_eq!(fx.toast.as_deref(), Some("Playing as Ben on Desk"));
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    /// A picker raised by a connect continues it as the pick, unchecked a second time.
    #[test]
    fn a_pick_continues_the_waiting_connect() {
        let h = host(Some("gone"));
        let intent = ConnectIntent::to_host(&h, None);
        let ask = intent.ask.clone().expect("a paired card asks");
        let mut s = ProfilesScreen::before(intent, ask, listed(), Some("Kid".into()));
        assert_eq!(s.line().as_deref(), Some("Kid is gone from this host."));
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        let go = fx.connect.expect("the connect goes on");
        assert_eq!(go.profile.as_deref(), Some("own"));
        assert!(go.ask.is_none());
        assert!(fx.toast.is_none());
    }

    /// Back keeps the saved pick and drops a waiting connect.
    #[test]
    fn back_saves_nothing() {
        let h = host(None);
        let intent = ConnectIntent::to_host(&h, None);
        let ask = intent.ask.clone().unwrap();
        let mut s = ProfilesScreen::before(intent, ask, listed(), None);
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Back, ctx, &mut fx));
        assert!(fx.cmds.is_empty() && fx.connect.is_none());
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    /// The one line under a card is the shared core's; an empty or missing list says so.
    #[test]
    fn notes_and_empty_states_read_as_one_line() {
        let mut s = ProfilesScreen::switch(&host(None)).unwrap();
        let busy = ListedProfile {
            seat: Some(Seat {
                state: SeatState::Occupied,
                occupant: Some("Ben's Apple TV".into()),
                ..Default::default()
            }),
            ..profile("kid", "Kid")
        };
        s.set_answer(ProfilesAnswer::Listed(vec![busy]));
        assert_eq!(
            with_ctx(|ctx| s.announcement(ctx)).as_deref(),
            Some("Kid, In use by Ben's Apple TV")
        );
        s.set_answer(ProfilesAnswer::NoProfiles);
        assert_eq!(s.line().as_deref(), Some("Desk has no profiles."));
        s.set_answer(ProfilesAnswer::Failed("the host refused it (500)".into()));
        assert_eq!(
            s.line().as_deref(),
            Some("Couldn't load the profiles \u{2014} the host refused it (500)")
        );
        let mut fx = Outbox::default();
        with_ctx(|ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        assert!(fx.cmds.is_empty(), "nothing to pick");
    }

    #[test]
    fn an_unpaired_card_has_no_profiles_to_switch() {
        let h = HostRow {
            paired: false,
            ..host(None)
        };
        assert!(ProfilesScreen::switch(&h).is_none());
    }
}
