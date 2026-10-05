//! The profile picker (`design/profiles-and-seats.md` §10): who plays on a paired host, asked
//! before a connect and from a host card's "Switch profile…". The rule is
//! `pf_client_core::profiles::picker_decision`; this file runs the fetch and draws the sheet.
//!
//! The sheet is an in-tree overlay like the host editor: a ContentDialog takes text only, and
//! the picker needs circles. It lives at root, so a worker thread can raise it over any screen.

use super::style::*;
use super::{AppCtx, Screen, Target};
use crate::trust::KnownHosts;
use pf_client_core::profiles::{self, Decision, ListedProfile, ProfilePick};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_reactor::*;

/// How long a connect waits for the host's list before it dials as it would have.
const FETCH_CUTOFF: Duration = Duration::from_secs(3);
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

/// Before a connect to a paired host: ask who plays, then `go(profile id)`. Runs off the UI
/// thread. A failed or late answer goes on with the link's `as=`, else the saved pick.
pub(crate) fn then_connect(
    ctx: &Arc<AppCtx>,
    target: Target,
    saved: Option<ProfilePick>,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    go: impl FnOnce(Option<String>) + Send + 'static,
) {
    set_screen.call(Screen::Connecting);
    let (ctx, set_screen) = (ctx.clone(), set_screen.clone());
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
            if !d.picker {
                if d.remember != saved {
                    save_pick(fp.as_deref(), &target.addr, target.port, d.remember);
                }
                return go(d.send);
            }
            let (addr, port) = (target.addr.clone(), target.port);
            let on_pick = move |p: ProfilePick| {
                save_pick(fp.as_deref(), &addr, port, Some(p.clone()));
                go(Some(p.id));
            };
            let ask = PickerAsk::new(
                &target,
                fetched.flatten(),
                saved,
                d.gone,
                on_pick,
                Some(set_screen),
            );
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
    let quiet = |text: String| -> Element {
        text_block(text)
            .font_size(13.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText)
            .into()
    };
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
