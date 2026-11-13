//! One saved host's page: how to reach it, what a connect uses and streams, its pairing, its
//! power, and the way to remove it. The Hosts list's card connects; everything else about a
//! host is here, in the groups the other clients show. The page reads the live record by its
//! [`HostRef`], so an edit shows at once.
//!
//! A field saves on Enter and when focus leaves it. What the rules refuse goes to the status
//! line and the box shows the record again.

use super::hosts::{self, CardPreset, Draft, HostRef};
use super::library::open_library;
use super::speed::SpeedState;
use super::style::*;
use super::{lucide, saved, Screen, Svc, Target};
use crate::trust::{HostEdit, KnownHosts, Settings};
use pf_client_core::discovery::DiscoveredHost;
use pf_client_core::display_targets::{self, ScreenRow};
use pf_client_core::host_actions::ActionInfo;
use pf_client_core::host_status::Status;
use pf_client_core::orchestrate::{self, HostTarget};
use pf_client_core::start::{self, StartIn};
use pf_client_core::trust::KnownHost;
use pf_fluent::Palette;
use pf_l10n::{client, common, desktop, host};
use std::collections::HashMap;
use std::rc::Rc;
use windows_reactor::*;

/// The width of a field's box beside its label, and in a window too narrow for that.
const FIELD_WIDTH: f64 = 260.0;
const FIELD_WIDTH_NARROW: f64 = 180.0;

#[derive(Clone)]
pub(crate) struct HostPageProps {
    pub(crate) svc: Svc,
    pub(crate) who: HostRef,
    pub(crate) hosts: Vec<DiscoveredHost>,
    /// Saved hosts the probe sweep reached, keyed by `KnownHost::card_key`.
    pub(crate) probed: HashMap<String, bool>,
    /// Bumped when a store write changes what the page shows.
    pub(crate) hosts_rev: u64,
    /// The width the page has beside the pane.
    pub(crate) width: f64,
}

impl PartialEq for HostPageProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc
            && self.who == other.who
            && self.hosts == other.hosts
            && self.probed == other.probed
            && self.hosts_rev == other.hosts_rev
            && self.width == other.width
    }
}

/// What the page asks before it acts.
#[derive(Clone)]
pub(crate) enum Confirm {
    Remove,
    Unpair,
    /// One of the host's own actions that drops its state.
    Power(ActionInfo),
}

pub(crate) enum PageMsg {
    Ask(Option<Confirm>),
    /// Show the record in every box again: an edit was saved, or refused.
    Snap,
}

#[derive(Clone, Copy)]
enum Field {
    Name,
    Addr,
    Port,
    Mac,
}

#[derive(Default)]
struct Fields {
    name: Draft<String>,
    addr: Draft<String>,
    port: Draft<String>,
    mac: Draft<String>,
}

pub(crate) struct HostPage {
    fields: Fields,
    /// The boxes are keyed by it: a bump mounts them afresh, on the record's text.
    epoch: u64,
    confirm: Option<Confirm>,
}

impl HostPage {
    /// Show the stored record in the boxes: all of them when `forced`, else the ones nobody is
    /// typing in.
    fn seed(&mut self, who: &HostRef, forced: bool) {
        let known = KnownHosts::load();
        let Some(k) = who.index(&known).map(|i| &known.hosts[i]) else {
            return;
        };
        let f = &mut self.fields;
        for (draft, stored) in [
            (&mut f.name, k.name.clone()),
            (&mut f.addr, k.addr.clone()),
            (&mut f.port, k.port.to_string()),
            (&mut f.mac, k.mac.join(", ")),
        ] {
            let untouched = *draft.live.borrow() == draft.seed;
            if forced || (untouched && draft.seed != stored) {
                draft.reset(stored);
            }
        }
    }
}

impl Component for HostPage {
    type Input = HostPageProps;
    type Message = PageMsg;

    fn create(props: &HostPageProps, _cx: &ComponentContext<Self>) -> Self {
        let mut page = HostPage {
            fields: Fields::default(),
            epoch: 0,
            confirm: None,
        };
        page.seed(&props.who, true);
        page
    }

    fn input_changed(&mut self, props: &HostPageProps, _cx: &ComponentContext<Self>) {
        self.seed(&props.who, false);
    }

    fn update(&mut self, msg: PageMsg, cx: &ComponentContext<Self>) {
        match msg {
            PageMsg::Ask(c) => self.confirm = c,
            PageMsg::Snap => {
                self.seed(&cx.input().who, true);
                self.epoch += 1;
            }
        }
    }

    fn view(&self, props: &HostPageProps, cx: &mut ViewContext<'_, Self>) -> View {
        host_page(self, props, cx.callback(|m: PageMsg| m))
    }
}

/// Save one field as typed. Untouched, it writes nothing; a pasted `address:port` moves both.
fn commit(
    svc: &Svc,
    who: &HostRef,
    field: Field,
    typed: &str,
    stored: &str,
    msgs: &Callback<PageMsg>,
) {
    let typed = typed.trim();
    if typed == stored.trim() {
        return;
    }
    let blank = HostEdit::default;
    let edit = match field {
        Field::Name if typed.is_empty() => Err(client::hosts_error_name_empty().to_string()),
        Field::Name => Ok(HostEdit {
            name: Some(typed.to_string()),
            ..blank()
        }),
        Field::Addr => HostEdit::parse_address(typed).map(|(addr, port)| HostEdit {
            addr: Some(addr),
            port,
            ..blank()
        }),
        Field::Port => HostEdit::parse_port(typed).map(|port| HostEdit {
            port: Some(port),
            ..blank()
        }),
        Field::Mac => HostEdit::parse_macs(typed).map(|macs| HostEdit {
            macs: Some(macs),
            ..blank()
        }),
    };
    match edit {
        Err(sentence) => svc.set_status.call(sentence),
        Ok(edit) => {
            let r = KnownHosts::update(|known| {
                let h = who.index(known).and_then(|i| known.hosts.get_mut(i))?;
                h.apply_edit(&edit);
                Some(HostRef::of(h))
            });
            let r = r.map_err(|e| desktop::desktop_hostcard_save_failed(format!("{e:#}")));
            match r {
                Err(sentence) => svc.set_status.call(sentence),
                Ok(now) => {
                    svc.set_status.call(String::new());
                    // The record's name or address moved: the page follows it.
                    if let Some(now) = now {
                        svc.set_host_page.call(Some(now));
                    }
                }
            }
            svc.bump_hosts.call(());
        }
    }
    msgs.call(PageMsg::Snap);
}

/// A field's box: it keeps what is typed in `draft` and saves on Enter and on losing focus.
fn field_box(
    page: &HostPage,
    props: &HostPageProps,
    field: Field,
    draft: &Draft<String>,
    hint: &str,
    msgs: &Callback<PageMsg>,
) -> View {
    let live = draft.live.clone();
    let save: Rc<dyn Fn()> = {
        let (svc, who, msgs) = (props.svc.clone(), props.who.clone(), msgs.clone());
        let (live, stored) = (draft.live.clone(), draft.seed.clone());
        Rc::new(move || commit(&svc, &who, field, &live.borrow(), &stored, &msgs))
    };
    let typed = TextBox::new()
        .text(&draft.seed)
        .placeholder_text(hint)
        .on_text_changed(move |t: Rc<str>| *live.borrow_mut() = t.to_string());
    let on_enter = save.clone();
    Border::new()
        .content(vstack(()).keyed_children([KeyedView::new(page.epoch.to_string(), typed)]))
        .width(match props.width < 560.0 {
            true => FIELD_WIDTH_NARROW,
            false => FIELD_WIDTH,
        })
        .on_lost_focus(move |_: FocusEventInfo| save())
        .on_preview_key_down(RoutedCallback::new(move |k: KeyEventInfo| {
            let enter = k.key == VirtualKey::ENTER;
            if enter {
                on_enter();
            }
            enter
        }))
        .into()
}

/// A row that is a button: what it does on the left, its mark on the right. A `danger` row
/// reads in the critical colour.
fn action_row(
    pal: &Palette,
    title: &str,
    caption: Option<&str>,
    mark: &str,
    (enabled, danger): (bool, bool),
    click: impl Fn() + 'static,
) -> View {
    let title = text(title).text_wrapping(TextWrapping::Wrap);
    let mut words: Vec<View> = vec![match danger && enabled {
        true => title.foreground(ThemeBrush::SystemCritical).into(),
        false => title.into(),
    }];
    words.extend(caption.filter(|c| !c.is_empty()).map(|c| {
        muted(pal, c)
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .into()
    }));
    let trailing: View = match mark.is_empty() {
        true => Border::new().into(),
        false => lucide::mark(mark)
            .font_size(16.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    };
    Button::new()
        .content(
            Grid::new()
                .children((
                    vstack(words)
                        .spacing(2.0)
                        .grid_column(0)
                        .vertical_alignment(VerticalAlignment::Center),
                    trailing,
                ))
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(16.0)
                .margin(edges(1.0, 5.0, 1.0, 5.0))
                .min_height(34.0),
        )
        .style(ButtonStyle::Subtle)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .is_enabled(enabled)
        .on_click(click)
        .margin(uniform(4.0))
        .into()
}

/// A group: its heading over a card of rows. Nothing when it has no rows.
fn group(pal: &Palette, heading: &str, rows: Vec<View>, note: Option<String>) -> Vec<View> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut out = vec![group_heading(heading), row_card(pal, rows)];
    out.extend(note.map(|n| {
        muted(pal, n)
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .into()
    }));
    out
}

/// The first eight and last four of a fingerprint.
fn short_fp(fp: &str) -> String {
    match fp.get(..8).zip(fp.get(fp.len().saturating_sub(4)..)) {
        Some((head, tail)) if fp.len() >= 12 => format!("{head}\u{2026}{tail}"),
        _ => fp.to_string(),
    }
}

/// The line under a Screen row: why it is greyed, else its hint, else the monitor's mode.
fn screen_line(r: &ScreenRow) -> Option<&str> {
    [r.reason.as_deref(), r.hint, Some(r.detail.as_str())]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
}

fn host_page(page: &HostPage, props: &HostPageProps, msgs: Callback<PageMsg>) -> View {
    let (svc, pal) = (&props.svc, &props.svc.pal);
    let known = KnownHosts::load();
    let Some(k) = props.who.index(&known).map(|i| &known.hosts[i]) else {
        // Removed from under the page, here or by another process.
        return notice_page(pal, None, client::lib_host_not_saved(), "", Vec::new());
    };
    let settings = Settings::load();
    let online = props.probed.get(&k.card_key()).copied().unwrap_or(false);
    hosts::keep_warm(k, &props.hosts, online);
    let status = hosts::status_of(k, online, settings.auto_wake);
    let target = Target {
        host: HostTarget::from(k),
        ..Target::default()
    };
    let presets = CardPreset::catalog();

    let mut body: Vec<View> = vec![header(svc, k, &status, &target, online)];
    body.extend(presets_group(svc, k, &props.who, &presets));
    body.extend(screen_groups(svc, k, &target, &presets, online));
    body.extend(connection_group(page, props, k, &target, &msgs));
    body.extend(pairing_group(svc, k, &known, &settings, &target, &msgs));
    body.extend(power_group(svc, k, &target, online, &msgs));
    // The upload authenticates with the paired identity, to a host that answers.
    let logs = action_row(
        pal,
        client::hostcard_send_logs(),
        Some(match online {
            true => client::hostcard_send_logs_hint(),
            false => desktop::desktop_hostcard_offline_hint(),
        }),
        "send",
        (k.paired && online, false),
        {
            let (svc, target) = (svc.clone(), target.clone());
            move || send_logs(&svc, &target)
        },
    );
    body.extend(group(
        pal,
        client::hostcard_section_logs(),
        vec![logs],
        None,
    ));
    let remove = action_row(
        pal,
        client::hostcard_remove(),
        None,
        "trash-2",
        (true, true),
        {
            let msgs = msgs.clone();
            move || msgs.call(PageMsg::Ask(Some(Confirm::Remove)))
        },
    );
    body.push(
        Border::new()
            .content(row_card(pal, vec![remove]))
            .margin(edges(0.0, 16.0, 0.0, 0.0))
            .into(),
    );
    page_form(body, props.width)
        .content_dialog(confirm_dialog(svc, &props.who, k, &target, page, msgs))
}

/// The page's head: the host's tile, name and state, then what a card's click does.
fn header(svc: &Svc, k: &KnownHost, status: &Status, target: &Target, online: bool) -> View {
    let pal = &svc.pal;
    let connect = icon_button(
        "play",
        match status {
            Status::Playing(_) => client::hostcard_resume(),
            _ => common::common_connect(),
        },
    )
    .style(ButtonStyle::Accent)
    .on_click({
        let (svc, target) = (svc.clone(), target.clone());
        move || hosts::connect_saved(&svc, target.clone(), online)
    });
    let mut acts: Vec<View> = vec![connect.into()];
    // The library needs the paired identity.
    if k.paired {
        acts.push(
            icon_button("gamepad-2", client::hostcard_browse_library())
                .on_click({
                    let (svc, target) = (svc.clone(), target.clone());
                    move || open_library(&svc, target.clone())
                })
                .into(),
        );
    }
    let words = vstack((
        page_title(&k.name),
        hosts::status_line(pal, status),
        hstack(acts).spacing(8.0).margin(edges(0.0, 10.0, 0.0, 0.0)),
    ))
    .spacing(4.0)
    .grid_column(1)
    .vertical_alignment(VerticalAlignment::Center);
    Grid::new()
        .children((
            host_tile(pal, &k.name, &k.os, !k.fp_hex.is_empty(), 64.0)
                .opacity(if online { 1.0 } else { 0.55 })
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Top),
            words,
        ))
        .columns([GridLength::Auto, GridLength::STAR])
        .column_spacing(16.0)
        .margin(edges(0.0, 0.0, 0.0, 4.0))
        .into()
}

/// What a click on the host's card connects with, and which presets have a card of their own.
fn presets_group(svc: &Svc, k: &KnownHost, who: &HostRef, presets: &[CardPreset]) -> Vec<View> {
    let pal = &svc.pal;
    if presets.is_empty() {
        let settings = action_row(
            pal,
            desktop::desktop_hostcard_no_presets(),
            Some(desktop::desktop_hostcard_no_presets_hint()),
            "chevron-right",
            (true, false),
            {
                let svc = svc.clone();
                move || {
                    super::settings::refresh_snapshot(&svc.ctx);
                    svc.set_screen.call(Screen::Settings)
                }
            },
        );
        return group(
            pal,
            client::hostcard_section_presets(),
            vec![settings],
            None,
        );
    }
    let mut names = vec![client::hostcard_default_settings().to_string()];
    names.extend(presets.iter().map(|p| p.name.clone()));
    // A binding whose preset is gone reads as Default settings, as a connect resolves it.
    let bound = k
        .preset_id
        .as_ref()
        .and_then(|id| presets.iter().position(|p| &p.id == id))
        .map_or(0, |i| i + 1);
    let ids: Vec<String> = presets.iter().map(|p| p.id.clone()).collect();
    let binding = ComboBox::new()
        .items_source(names)
        .selected_index(bound)
        .on_selection_changed({
            let (svc, who) = (svc.clone(), who.clone());
            move |i: Option<usize>| {
                // None is "nothing selected", which an in-place items rebuild raises.
                let Some(i) = i else { return };
                let id = i.checked_sub(1).and_then(|i| ids.get(i)).cloned();
                let r = KnownHosts::update(|known| {
                    if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
                        h.preset_id = id;
                    }
                });
                saved(r, &svc.set_status, None);
                svc.bump_hosts.call(());
            }
        });
    let mut rows = vec![row(
        pal,
        client::hostcard_connect_with(),
        Some(desktop::desktop_hostcard_connect_with_hint()),
        binding,
    )];
    for p in presets {
        let on = k.pinned_presets.iter().any(|id| id == &p.id);
        let (svc, who, id) = (svc.clone(), who.clone(), p.id.clone());
        rows.push(row(
            pal,
            &client::hostcard_pin_preset(&p.name),
            None,
            switch(pal, on, move |on: bool| {
                hosts::pin_card(&svc, &who, &id, on)
            }),
        ));
    }
    group(pal, client::hostcard_section_presets(), rows, None)
}

/// The Screen group from core's rows: the roles, then the host's monitors under their header.
/// The pick carries the check; a greyed row says why. Under a note, the row the host or the
/// bound preset leaves, greyed, and the note under the card. Nothing until the host answered.
fn screen_groups(
    svc: &Svc,
    k: &KnownHost,
    target: &Target,
    presets: &[CardPreset],
    online: bool,
) -> Vec<View> {
    let pal = &svc.pal;
    let Some(targets) = display_targets::cached(&k.fp_hex).filter(|_| k.paired) else {
        return Vec::new();
    };
    let bound = hosts::bound_screen_preset(k, presets);
    let note = targets.note_under(bound);
    let (mut roles, mut monitors, mut header) = (Vec::new(), Vec::new(), None);
    for r in targets.rows_under(bound) {
        let pick = r.pick.clone().filter(|_| online);
        let view = action_row(
            pal,
            &r.label,
            screen_line(&r),
            if r.checked { "check" } else { "" },
            (r.checked || pick.is_some(), false),
            {
                let (svc, target, name) = (svc.clone(), target.clone(), r.label.clone());
                move || {
                    if let Some(pick) = &pick {
                        hosts::set_host_screen(&svc, &target, pick, &name);
                    }
                }
            },
        );
        header = header.or(r.header);
        match header {
            Some(_) => monitors.push(view),
            None => roles.push(view),
        }
    }
    // An older host's rows keep the hint on what a screen is.
    let hint = (!roles.is_empty() || !monitors.is_empty()).then(|| {
        note.clone()
            .unwrap_or_else(|| client::screen_hint().to_string())
    });
    let (role_note, monitor_note) = match monitors.is_empty() {
        true => (hint, None),
        false => (None, hint),
    };
    let mut out = group(pal, client::hostcard_section_screen(), roles, role_note);
    out.extend(group(
        pal,
        header.unwrap_or_default(),
        monitors,
        monitor_note,
    ));
    out
}

/// How to reach the host, edited in place.
fn connection_group(
    page: &HostPage,
    props: &HostPageProps,
    k: &KnownHost,
    target: &Target,
    msgs: &Callback<PageMsg>,
) -> Vec<View> {
    let (svc, pal) = (&props.svc, &props.svc.pal);
    let f = &page.fields;
    let field = |field, draft, hint| field_box(page, props, field, draft, hint, msgs);
    let mgmt = match k.mgmt_port {
        Some(port) => port.to_string(),
        None => {
            desktop::desktop_hostcard_mgmt_port_default(pf_client_core::library::DEFAULT_MGMT_PORT)
        }
    };
    let clipboard = switch(pal, k.clipboard_sync, {
        let (svc, who) = (svc.clone(), props.who.clone());
        move |on: bool| {
            let r = KnownHosts::update(|known| {
                if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
                    h.clipboard_sync = on;
                }
            });
            saved(r, &svc.set_status, None);
            svc.bump_hosts.call(());
        }
    });
    let has_pin = !k.fp_hex.is_empty();
    let link = pf_client_core::deeplink::DeepLink::for_host(k, None, None).to_url();
    let rows = vec![
        row(
            pal,
            client::hosts_field_name(),
            None,
            field(Field::Name, &f.name, client::hosts_field_name_placeholder()),
        ),
        row(
            pal,
            client::hosts_field_address(),
            None,
            field(
                Field::Addr,
                &f.addr,
                client::hosts_field_address_placeholder(),
            ),
        ),
        row(
            pal,
            client::hosts_field_port(),
            None,
            field(Field::Port, &f.port, ""),
        ),
        row(
            pal,
            client::hosts_field_mac(),
            Some(desktop::desktop_hosts_field_mac_hint()),
            field(Field::Mac, &f.mac, client::hosts_field_mac_placeholder()),
        ),
        row(
            pal,
            client::hostcard_management_port(),
            None,
            muted(pal, mgmt),
        ),
        row(
            pal,
            client::hostcard_share_clipboard(),
            Some(desktop::desktop_hostcard_share_clipboard_hint()),
            clipboard,
        ),
        action_row(
            pal,
            client::hostcard_speed_test(),
            Some(match has_pin {
                true => desktop::desktop_hostcard_speed_test_hint(),
                false => client::hostcard_pair_first(),
            }),
            "gauge",
            (has_pin, false),
            {
                let (svc, target) = (svc.clone(), target.clone());
                move || {
                    *svc.ctx.shared.target.lock().unwrap() = target.clone();
                    // New run: invalidate any still-in-flight probe, reset the screen.
                    svc.ctx
                        .shared
                        .speed_gen
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    svc.set_speed.call(SpeedState::Running);
                    svc.set_screen.call(Screen::SpeedTest);
                }
            },
        ),
        action_row(
            pal,
            client::hostcard_copy_link(),
            Some(desktop::desktop_hostcard_copy_link_hint()),
            "link",
            (true, false),
            {
                let (svc, link) = (svc.clone(), link.clone());
                move || {
                    pf_client_core::clipboard::set_text(&link);
                    svc.note(client::hostcard_link_copied().to_string());
                }
            },
        ),
        action_row(
            pal,
            desktop::desktop_hostcard_create_shortcut(),
            Some(desktop::desktop_hostcard_create_shortcut_hint()),
            "plus",
            (true, false),
            {
                let (svc, name) = (svc.clone(), k.name.clone());
                move || match crate::deeplink::write_shortcut(&name, &link) {
                    Ok(p) => tracing::info!(path = %p.display(), "shortcut written"),
                    Err(e) => {
                        tracing::warn!(error = %e, "shortcut not written");
                        svc.set_status.call(desktop::desktop_shortcut_failed(e));
                    }
                }
            },
        ),
    ];
    group(pal, client::hostcard_section_connection(), rows, None)
}

/// Whether this device is paired with the host, and what follows from it: the default host,
/// who plays, and unpairing.
fn pairing_group(
    svc: &Svc,
    k: &KnownHost,
    known: &KnownHosts,
    settings: &Settings,
    target: &Target,
    msgs: &Callback<PageMsg>,
) -> Vec<View> {
    let pal = &svc.pal;
    let state = match (k.fp_hex.is_empty(), k.paired) {
        (true, _) => row(
            pal,
            client::hostcard_status_not_paired(),
            Some(desktop::desktop_hostcard_pair_by_click_hint()),
            Border::new(),
        ),
        (false, paired) => row(
            pal,
            match paired {
                true => client::hostcard_paired(),
                false => client::hostcard_tofu(),
            },
            None,
            muted(pal, short_fp(&k.fp_hex)).font_family("Consolas"),
        ),
    };
    let mut rows = vec![
        state,
        action_row(
            pal,
            client::hostcard_pair_with_pin(),
            Some(desktop::desktop_hostcard_pair_with_pin_hint()),
            "chevron-right",
            (true, false),
            {
                let (svc, target) = (svc.clone(), target.clone());
                move || {
                    *svc.ctx.shared.target.lock().unwrap() = target.clone();
                    svc.set_status.call(String::new());
                    svc.set_screen.call(Screen::Pair);
                }
            },
        ),
    ];
    if k.paired {
        // The host Start in opens on: the explicit pointer, or the only paired host unasked.
        let default = start::default_host(settings, known)
            .is_some_and(|i| known.hosts[i].id == k.id && k.id.is_some());
        let record = k.id.clone();
        rows.push(row(
            pal,
            client::hostcard_default_host(),
            Some(match StartIn::parse(&settings.start_in) {
                StartIn::Hosts => desktop::desktop_hostcard_default_start_hosts_hint(),
                StartIn::Library => desktop::desktop_hostcard_default_start_library_hint(),
                StartIn::Stream => desktop::desktop_hostcard_default_start_stream_hint(),
            }),
            switch(pal, default, {
                let svc = svc.clone();
                move |on: bool| {
                    Settings::update(|s| {
                        if on {
                            s.default_host = record.clone();
                        } else if s.default_host == record {
                            s.default_host = None;
                        }
                        true
                    });
                    svc.bump_hosts.call(());
                }
            }),
        ));
    }
    // Re-pick who plays on this host, only where a pick is saved.
    if let Some(pick) = k.profile.clone().filter(|_| k.paired) {
        rows.push(action_row(
            pal,
            client::hostcard_switch_profile(),
            Some(&client::hostcard_playing_as(&pick.display_name)),
            "chevron-right",
            (true, false),
            {
                let (svc, target) = (svc.clone(), target.clone());
                let pin = crate::trust::parse_hex32(&k.fp_hex);
                move || {
                    let bump = svc.bump_hosts.clone();
                    super::profiles::switch(
                        &svc.ctx,
                        target.clone(),
                        Some(pick.clone()),
                        pin,
                        move || bump.call(()),
                    );
                }
            },
        ));
    }
    if !k.fp_hex.is_empty() {
        rows.push(action_row(
            pal,
            client::hostcard_forget_identity(),
            Some(desktop::desktop_hostcard_forget_identity_hint()),
            "",
            (true, true),
            {
                let msgs = msgs.clone();
                move || msgs.call(PageMsg::Ask(Some(Confirm::Unpair)))
            },
        ));
    }
    group(pal, client::hostcard_section_pairing(), rows, None)
}

/// Waking the host, and the host's own actions: sleep, restart, shut down, from what it said
/// it lets this device do. A device without the grant sees none.
fn power_group(
    svc: &Svc,
    k: &KnownHost,
    target: &Target,
    online: bool,
    msgs: &Callback<PageMsg>,
) -> Vec<View> {
    let pal = &svc.pal;
    let can_wake = !online && !k.mac.is_empty();
    let mut rows = vec![action_row(
        pal,
        client::hostcard_wake(),
        Some(match (online, k.mac.is_empty()) {
            (true, _) => desktop::desktop_hostcard_wake_on_hint(),
            (false, true) => desktop::desktop_hostcard_wake_no_mac_hint(),
            (false, false) => desktop::desktop_hostcard_wake_hint(),
        }),
        "power",
        (can_wake, false),
        {
            let target = target.clone();
            move || crate::wol::wake(&target.host.mac, target.host.addr.parse().ok())
        },
    )];
    for a in pf_client_core::host_actions::cached(&k.fp_hex) {
        let why = match online {
            false => Some(desktop::desktop_hostcard_offline_hint().to_string()),
            true => a.unavailable_reason.clone().filter(|_| !a.available),
        };
        let (svc, target, msgs, action) = (svc.clone(), target.clone(), msgs.clone(), a.clone());
        rows.push(action_row(
            pal,
            a.label(),
            why.as_deref(),
            "power",
            (a.available && online, a.danger),
            move || match action.danger {
                // Restart and shut down lose whatever runs on that machine.
                true => msgs.call(PageMsg::Ask(Some(Confirm::Power(action.clone())))),
                false => run_host_action(&svc, &target, &action),
            },
        ));
    }
    group(pal, client::hostcard_section_power(), rows, None)
}

/// The page's one confirmation, armed by `page.confirm`: removing the host, unpairing it, or
/// one of its own actions that drops its state.
fn confirm_dialog(
    svc: &Svc,
    who: &HostRef,
    k: &KnownHost,
    target: &Target,
    page: &HostPage,
    msgs: Callback<PageMsg>,
) -> ContentDialog {
    let pending = page.confirm.clone();
    let (title, body, yes) = match &pending {
        Some(Confirm::Remove) => (
            client::hostcard_remove_title(&k.name),
            client::hostcard_remove_body().to_string(),
            common::common_remove().to_string(),
        ),
        Some(Confirm::Unpair) => (
            client::hostcard_forget_identity_title(&k.name),
            client::hostcard_forget_identity_body().to_string(),
            client::hostcard_forget_identity_confirm().to_string(),
        ),
        Some(Confirm::Power(a)) => (
            host::host_power_confirm_title(a.label()),
            client::hostcard_action_confirm_body(&k.name),
            a.label().to_string(),
        ),
        None => Default::default(),
    };
    let (svc, who, target) = (svc.clone(), who.clone(), target.clone());
    ContentDialog::new()
        .title(title)
        .content(body)
        .primary_button_text(yes)
        .close_button_text(common::common_cancel())
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            msgs.call(PageMsg::Ask(None));
            if r != ContentDialogResult::Primary {
                return;
            }
            match &pending {
                Some(Confirm::Remove) => {
                    match orchestrate::forget_host(|known| who.index(known)) {
                        // The record is gone: so is its page.
                        Ok(_) => svc.set_host_page.call(None),
                        Err(e) => svc
                            .set_status
                            .call(desktop::desktop_hostcard_remove_failed(format!("{e:#}"))),
                    }
                    svc.bump_hosts.call(());
                }
                Some(Confirm::Unpair) => {
                    if let Err(e) = orchestrate::unpair_host(|known| who.index(known)) {
                        svc.set_status
                            .call(desktop::desktop_hostcard_save_failed(format!("{e:#}")));
                    }
                    svc.bump_hosts.call(());
                }
                Some(Confirm::Power(a)) => run_host_action(&svc, &target, a),
                None => {}
            }
        })
}

/// Runs one of the host's own actions on a worker thread, the outcome on the status line.
fn run_host_action(svc: &Svc, target: &Target, a: &ActionInfo) {
    let (action_id, label) = (a.id.clone(), a.label().to_string());
    if !a.available {
        // The host already said it cannot do this right now.
        svc.note(
            a.unavailable_reason
                .clone()
                .unwrap_or_else(|| client::hostcard_action_unavailable_body(&label)),
        );
        return;
    }
    let identity = svc.ctx.identity.clone();
    let (svc, target) = (svc.clone(), target.clone());
    svc.note(client::hostcard_action_asking(&label, &target.host.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-hostaction".into())
        .spawn(move || {
            svc.report(pf_client_core::host_actions::run(
                &target.host.name,
                &target.host.addr,
                target
                    .host
                    .mgmt_port
                    .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                target.host.port,
                &identity,
                target.host.fp_hex.as_deref().unwrap_or_default(),
                &action_id,
                &label,
            ));
        });
}

/// Uploads this device's log ring to the host. Blocking network (the library agent's 5 s
/// connect / 10 s global budgets), so on a worker thread, the outcome on the status line.
fn send_logs(svc: &Svc, target: &Target) {
    let identity = svc.ctx.identity.clone();
    let (svc, target) = (svc.clone(), target.clone());
    svc.note(client::hostcard_sending_logs(&target.host.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-sendlogs".into())
        .spawn(move || {
            svc.report(pf_client_core::logring::send_bundle(
                "punktfunk-client",
                &target.host.name,
                &target.host.addr,
                (
                    target
                        .host
                        .mgmt_port
                        .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                    target.host.port,
                ),
                &identity,
                target.host.fp_hex.as_deref().unwrap_or_default(),
            ));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_shows_its_ends() {
        assert_eq!(
            short_fp("0123456789abcdef0123456789abcdef"),
            "01234567\u{2026}cdef"
        );
        assert_eq!(short_fp("abcd"), "abcd");
        assert_eq!(short_fp(""), "");
    }
}
