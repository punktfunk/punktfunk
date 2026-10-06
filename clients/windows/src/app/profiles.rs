//! The profile picker (`design/profiles-and-seats.md` §10): who plays on a paired host, asked
//! before a connect and from a host card's "Switch profile…". The rule is
//! `pf_client_core::profiles::picker_decision`; this file runs the fetch and draws the sheet.
//!
//! The sheet is an in-tree overlay like the host editor: a ContentDialog takes text only, and
//! the picker needs circles. It lives at root, so a worker thread can raise it over any screen.
//! A profile whose seat is starting gets a second sheet of the same frame, [`SeatWait`].

use super::style::*;
use super::{AppCtx, Screen, Target};
use crate::trust::KnownHosts;
use pf_client_core::profiles::{self, Decision, ListedProfile, ProfilePick, SeatGate};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_reactor::*;

/// How long a connect waits for the host's list before it dials as it would have.
const FETCH_CUTOFF: Duration = Duration::from_secs(3);
/// How often the waiting sheet re-reads the seat.
const SEAT_POLL: Duration = Duration::from_secs(2);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What a pick does: saves it, then connects (or just closes, for "Switch profile…").
type OnPick = Arc<Mutex<Option<Box<dyn FnOnce(ProfilePick) + Send>>>>;

/// One open picker. Root state, so equality is the ask's identity.
#[derive(Clone)]
pub(crate) struct PickerAsk {
    id: u64,
    host: String,
    /// `None`: the list didn't load.
    listed: Option<Arc<Vec<ListedProfile>>>,
    saved: Option<ProfilePick>,
    gone: Option<String>,
    on_pick: OnPick,
    /// A connect waiting on the pick returns to the host list when the picker is cancelled.
    cancel_to: Option<AsyncSetState<Screen>>,
}

impl PartialEq for PickerAsk {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl PickerAsk {
    fn new(
        target: &Target,
        listed: Option<Vec<ListedProfile>>,
        saved: Option<ProfilePick>,
        gone: Option<String>,
        on_pick: impl FnOnce(ProfilePick) + Send + 'static,
        cancel_to: Option<AsyncSetState<Screen>>,
    ) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            host: target.name.clone(),
            listed: listed.map(Arc::new),
            saved,
            gone,
            on_pick: Arc::new(Mutex::new(Some(Box::new(on_pick)))),
            cancel_to,
        }
    }
}

/// A profile's seat coming up. Root state, so each new line re-renders the sheet.
#[derive(Clone)]
pub(crate) struct SeatWait {
    title: String,
    detail: Option<String>,
    /// Cancel sets it; the poll stops and nothing dials.
    cancel: Arc<AtomicBool>,
    /// Cancel returns to the host list.
    back: AsyncSetState<Screen>,
}

impl PartialEq for SeatWait {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title
            && self.detail == other.detail
            && Arc::ptr_eq(&self.cancel, &other.cancel)
    }
}

/// Writes `pick` as the host's saved profile (`None` drops it). Every other field stays.
pub(crate) fn save_pick(fp_hex: Option<&str>, addr: &str, port: u16, pick: Option<ProfilePick>) {
    let mut known = KnownHosts::load();
    let Some(i) = known.resolve_index(fp_hex, addr, port) else {
        return;
    };
    if known.hosts[i].profile == pick {
        return;
    }
    known.hosts[i].profile = pick;
    if let Err(e) = known.save() {
        tracing::warn!(error = %format!("{e:#}"), "saving the profile pick");
    }
}

/// The host's list within [`FETCH_CUTOFF`]. `None`: it failed or came late.
fn fetch(
    ctx: &AppCtx,
    target: &Target,
    pin: Option<[u8; 32]>,
) -> Option<Option<Vec<ListedProfile>>> {
    let addr = target.addr.clone();
    let mgmt = target
        .mgmt_port
        .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
    let identity = ctx.identity.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pf-profiles-fetch".into())
        .spawn(move || {
            let _ = tx.send(profiles::fetch_enumerate(&addr, mgmt, &identity, pin));
        })
        .ok()?;
    match rx.recv_timeout(FETCH_CUTOFF) {
        Ok(Ok(listed)) => Some(listed),
        Ok(Err(e)) => {
            tracing::info!(error = %e, "profile list unavailable");
            None
        }
        Err(_) => {
            tracing::info!("profile list late");
            None
        }
    }
}

fn open(ctx: &AppCtx, ask: PickerAsk) {
    if let Some(set) = ctx.shared.set_picker.lock().unwrap().as_ref() {
        set.call(Some(ask));
    }
}

/// `id`'s row in `listed`, the list the id came from.
fn row_of(listed: Option<&[ListedProfile]>, id: Option<&str>) -> Option<ListedProfile> {
    listed?.iter().find(|p| Some(p.id.as_str()) == id).cloned()
}

/// The host's list again, for the profile it just refused as unknown: that profile's row, or
/// `None` when the host no longer lists it. Blocks; run it off the UI thread.
pub(crate) fn relisted(
    ctx: &AppCtx,
    target: &Target,
    pin: Option<[u8; 32]>,
    id: &str,
) -> Option<ListedProfile> {
    let listed = fetch(ctx, target, pin)??;
    profiles::find(&listed, id).cloned()
}

/// A settled profile's seat decides what the connect does (§9.2): `go` dials now, a stopped or
/// starting seat gets the waiting sheet first, and one that can't play says why and stops.
/// `row` is the profile's row in the list it came from; without one the connect dials. Only a
/// dial runs on the calling thread, so the picker's click may call this.
pub(crate) fn seat_then(
    ctx: &Arc<AppCtx>,
    target: Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    row: Option<ListedProfile>,
    go: impl FnOnce() + Send + 'static,
) {
    let Some(row) = row else { return go() };
    let wake = match profiles::seat_gate(&row) {
        SeatGate::Dial => return go(),
        SeatGate::Refuse(line) => {
            set_status.call(line);
            set_screen.call(Screen::Hosts);
            return;
        }
        SeatGate::Wake => true,
        SeatGate::Wait { .. } => false,
    };
    let Some(set_seat) = ctx.shared.set_seat.lock().unwrap().clone() else {
        return go();
    };
    let (ctx, set_screen, set_status) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let _ = std::thread::Builder::new()
        .name("pf-seat".into())
        .spawn(move || {
            let cancel = Arc::new(AtomicBool::new(false));
            let title = profiles::waking_line(&row.display_name);
            // A request in flight when Cancel lands finishes into nothing: Cancel has already
            // closed the sheet and gone back to the host list.
            let show = |detail: Option<String>| {
                if !cancel.load(Ordering::SeqCst) {
                    set_seat.call(Some(SeatWait {
                        title: title.clone(),
                        detail,
                        cancel: cancel.clone(),
                        back: set_screen.clone(),
                    }));
                }
            };
            let stop = |line: String| {
                if !cancel.load(Ordering::SeqCst) {
                    set_seat.call(None);
                    set_status.call(line);
                    set_screen.call(Screen::Hosts);
                }
            };
            let mgmt = target
                .mgmt_port
                .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
            let mut row = row;
            show(None);
            if wake {
                match profiles::wake(&target.addr, mgmt, &ctx.identity, pin, &row.id) {
                    Ok(woken) => row = woken,
                    Err(e) => {
                        return stop(format!(
                            "Couldn't wake {}'s desk \u{2014} {e}",
                            row.display_name
                        ));
                    }
                }
            }
            loop {
                match profiles::seat_gate(&row) {
                    SeatGate::Dial => {
                        if !cancel.load(Ordering::SeqCst) {
                            set_seat.call(None);
                            go();
                        }
                        return;
                    }
                    SeatGate::Refuse(line) => return stop(line),
                    SeatGate::Wait { detail } => show(detail),
                    SeatGate::Wake => show(None),
                }
                std::thread::sleep(SEAT_POLL);
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                match profiles::fetch_enumerate(&target.addr, mgmt, &ctx.identity, pin) {
                    Ok(listed) => match row_of(listed.as_deref(), Some(&row.id)) {
                        Some(polled) => row = polled,
                        None => {
                            return stop(format!("{} is gone from this host.", row.display_name))
                        }
                    },
                    // A poll that fails waits for the next one.
                    Err(e) => tracing::debug!(error = %e, "seat poll"),
                }
            }
        });
}

/// Before a connect to a paired host: ask who plays, then `go(profile id)` once that profile's
/// seat allows it. Runs off the UI thread. A failed or late answer goes on with the link's
/// `as=`, else the saved pick.
pub(crate) fn then_connect(
    ctx: &Arc<AppCtx>,
    target: Target,
    saved: Option<ProfilePick>,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    go: impl FnOnce(Option<String>) + Send + 'static,
) {
    set_screen.call(Screen::Connecting);
    let (ctx, set_screen, set_status) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let _ = std::thread::Builder::new()
        .name("pf-profiles".into())
        .spawn(move || {
            let link = target.link_profile.clone();
            let fetched = fetch(&ctx, &target, pin);
            let d = match &fetched {
                Some(listed) => {
                    profiles::picker_decision(listed.as_deref(), saved.as_ref(), link.as_deref())
                }
                None => Decision {
                    send: link.or_else(|| saved.as_ref().map(|p| p.id.clone())),
                    remember: saved.clone(),
                    ..Decision::default()
                },
            };
            let fp = target.fp_hex.clone();
            let listed = fetched.flatten();
            if !d.picker {
                if d.remember != saved {
                    save_pick(fp.as_deref(), &target.addr, target.port, d.remember);
                }
                let row = row_of(listed.as_deref(), d.send.as_deref());
                return seat_then(
                    &ctx,
                    target,
                    pin,
                    &set_screen,
                    &set_status,
                    row,
                    move || go(d.send),
                );
            }
            let rows = listed.clone();
            let (ctx2, t, ss, st) = (
                ctx.clone(),
                target.clone(),
                set_screen.clone(),
                set_status.clone(),
            );
            let on_pick = move |p: ProfilePick| {
                save_pick(t.fp_hex.as_deref(), &t.addr, t.port, Some(p.clone()));
                let row = row_of(rows.as_deref(), Some(&p.id));
                seat_then(&ctx2, t, pin, &ss, &st, row, move || go(Some(p.id)));
            };
            let ask = PickerAsk::new(&target, listed, saved, d.gone, on_pick, Some(set_screen));
            open(&ctx, ask);
        });
}

/// A host card's "Switch profile…": fetch, then the picker. A pick saves and doesn't connect;
/// `done` runs after it so the card can redraw.
pub(crate) fn switch(
    ctx: &Arc<AppCtx>,
    target: Target,
    saved: Option<ProfilePick>,
    pin: Option<[u8; 32]>,
    done: impl FnOnce() + Send + 'static,
) {
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("pf-profiles".into())
        .spawn(move || {
            // A box that lists nothing (404) reads as an empty list; a failed fetch as an error.
            let listed = fetch(&ctx, &target, pin).map(Option::unwrap_or_default);
            let (fp, addr, port) = (target.fp_hex.clone(), target.addr.clone(), target.port);
            let on_pick = move |p: ProfilePick| {
                save_pick(fp.as_deref(), &addr, port, Some(p));
                done();
            };
            open(
                &ctx,
                PickerAsk::new(&target, listed, saved, None, on_pick, None),
            );
        });
}

/// What a card's tap or button does: close the sheet, then hand the pick on once.
fn chooser(
    set_picker: &AsyncSetState<Option<PickerAsk>>,
    on_pick: &OnPick,
    pick: ProfilePick,
) -> impl Fn() + Clone + 'static {
    let (set, on_pick) = (set_picker.clone(), on_pick.clone());
    move || {
        set.call(None);
        if let Some(f) = on_pick.lock().unwrap().take() {
            f(pick.clone());
        }
    }
}

/// One profile: its circle (tap), its name as the focusable button (keyboard, pad), and the
/// seat's note. The saved pick is ringed.
fn profile_card(p: &ListedProfile, saved: bool, go: impl Fn() + Clone + 'static) -> Element {
    let fill = p.accent.as_deref().and_then(super::settings::hex_color);
    let mut ring = border(monogram(&profiles::initials(&p.display_name), 60.0, fill))
        .border_thickness(uniform(3.0))
        .corner_radius(33.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .on_tapped(go.clone());
    if saved {
        ring = ring.border_brush(ThemeRef::Accent);
    }
    let mut parts: Vec<Element> = vec![
        ring.into(),
        button(p.display_name.clone())
            .subtle()
            .on_click(go)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    ];
    if let Some(note) = p.note() {
        parts.push(
            text_block(note)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    vstack(parts).spacing(6.0).into()
}

fn quiet(text: String) -> Element {
    text_block(text)
        .font_size(13.0)
        .wrap()
        .foreground(ThemeRef::SecondaryText)
        .into()
}

/// A sheet over the screen: `body` in a dialog surface on a scrim. `cancel` runs on Escape and
/// on a tap outside the surface.
fn sheet(body: Vec<Element>, cancel: impl Fn() + Clone + 'static) -> Element {
    // A tap inside the card bubbles to the scrim; the flag makes the scrim swallow it.
    let inside_tap = std::rc::Rc::new(std::cell::Cell::new(false));
    let modal = dialog_surface(scroll_view(vstack(body).spacing(14.0)))
        .on_tapped({
            let inside_tap = inside_tap.clone();
            move || inside_tap.set(true)
        })
        .max_width(640.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(uniform(24.0));
    let scrim_cancel = cancel.clone();
    Element::from(
        border(modal)
            .background(Color {
                a: 140,
                r: 0,
                g: 0,
                b: 0,
            })
            .on_tapped(move || {
                if inside_tap.replace(false) {
                    return;
                }
                scrim_cancel();
            }),
    )
    .keyboard_accelerator(KeyboardAccelerator::new(
        VirtualKey::Escape,
        VirtualKeyModifiers::None,
        cancel,
    ))
}

/// The seat wait overlay, in its own stable root slot: the line, the host's detail and Cancel.
pub(crate) fn seat_slot(
    wait: &Option<SeatWait>,
    set_seat: &AsyncSetState<Option<SeatWait>>,
) -> Element {
    let Some(w) = wait else {
        return border(vstack(Vec::<Element>::new())).into();
    };
    let mut body: Vec<Element> = vec![
        ProgressRing::indeterminate()
            .width(32.0)
            .height(32.0)
            .horizontal_alignment(HorizontalAlignment::Left)
            .into(),
        text_block(w.title.clone())
            .font_size(20.0)
            .bold()
            .wrap()
            .into(),
    ];
    if let Some(detail) = &w.detail {
        body.push(quiet(detail.clone()));
    }
    let cancel = {
        let (set, flag, back) = (set_seat.clone(), w.cancel.clone(), w.back.clone());
        move || {
            flag.store(true, Ordering::SeqCst);
            set.call(None);
            back.call(Screen::Hosts);
        }
    };
    body.push(
        button("Cancel")
            .on_click(cancel.clone())
            .horizontal_alignment(HorizontalAlignment::Right)
            .into(),
    );
    sheet(body, cancel)
}

/// The picker overlay, in a stable root slot: a same-kind empty border while closed.
pub(crate) fn picker_slot(
    ask: &Option<PickerAsk>,
    set_picker: &AsyncSetState<Option<PickerAsk>>,
) -> Element {
    let Some(a) = ask else {
        return border(vstack(Vec::<Element>::new())).into();
    };
    let mut body: Vec<Element> = vec![text_block(format!("Who\u{2019}s playing on {}?", a.host))
        .font_size(20.0)
        .bold()
        .wrap()
        .into()];
    if let Some(gone) = &a.gone {
        body.push(quiet(format!("{gone} is gone from this host.")));
    }
    let mut close_label = "Cancel";
    match a.listed.as_deref() {
        None => {
            body.push(quiet("Couldn\u{2019}t load the profiles.".into()));
            close_label = "Close";
        }
        Some(l) if l.is_empty() => {
            body.push(quiet("No profiles on this host.".into()));
            close_label = "Close";
        }
        Some(l) => {
            // The saved pick leads; the sort is stable, so the host's order holds after it.
            let is_saved = |p: &ListedProfile| a.saved.as_ref().is_some_and(|s| s.id == p.id);
            let mut rows: Vec<&ListedProfile> = l.iter().collect();
            rows.sort_by_key(|p| !is_saved(p));
            let cards: Vec<Element> = rows
                .iter()
                .map(|p| {
                    let go = chooser(set_picker, &a.on_pick, p.pick());
                    profile_card(p, is_saved(p), go)
                })
                .collect();
            body.push(tile_grid(cards, l.len().clamp(1, 4), 12.0));
        }
    }
    let cancel = {
        let (set, back) = (set_picker.clone(), a.cancel_to.clone());
        move || {
            set.call(None);
            if let Some(back) = &back {
                back.call(Screen::Hosts);
            }
        }
    };
    body.push(
        button(close_label)
            .on_click(cancel.clone())
            .horizontal_alignment(HorizontalAlignment::Right)
            .into(),
    );
    sheet(body, cancel)
}
