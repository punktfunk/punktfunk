//! General: the session, the console, Omarchy, the statistics overlay, Show advanced.

use super::{Build, PageB};
use crate::settings::choice::set_row_subtitle;
use crate::settings::field::Field;
use crate::settings::spec::{self, Page};
use crate::settings::tables::{at, index};
use crate::store::Store;
use adw::prelude::*;
use pf_client_core::settings::GamepadUi;
use pf_client_core::start::{self, StartIn};
use pf_client_core::trust::{HudCorner, StatsVerbosity};
use punktfunk_core::hud::STATS_SCALE_PCTS;

pub fn general(b: &mut Build, switcher: adw::PreferencesGroup) {
    let mut p = b.page(Page::General, "preferences-system-symbolic");
    // The scope switcher heads the first page: it is about which layer you edit.
    p.page.add(&switcher);
    session(b, &mut p);
    if cfg!(feature = "console") {
        console(b, &mut p);
    }
    // Only where the theme exists, rather than sitting disabled.
    if pf_client_core::omarchy::present() {
        omarchy(b, &mut p);
    }

    let stats = p.group("Statistics", "");
    let row = b.choice(&spec::STATS, &["Off", "Compact", "Normal", "Detailed"]);
    b.put(
        &mut p,
        Some(&stats),
        Field::choice(&spec::STATS, &row, index::stats, |s, i| {
            s.set_stats_verbosity(*at(&StatsVerbosity::ALL, i))
        }),
    );
    p.row(&stats, &docs_row());

    let advanced = p.group("", "");
    let (field, show) = Field::switch(
        &spec::SHOW_ADVANCED,
        |s| s.show_advanced,
        |s, v| s.show_advanced = v,
    );
    b.put(&mut p, Some(&advanced), field);
    b.show_advanced = Some(show);

    let (field, _) = Field::switch(
        &spec::ADVANCED_STATS,
        |s| s.advanced_stats,
        |s, v| s.advanced_stats = v,
    );
    b.put(&mut p, None, field);
    let corners: Vec<&str> = HudCorner::ALL.iter().map(|c| c.label()).collect();
    let row = b.choice(&spec::STATS_POSITION, &corners);
    b.put(
        &mut p,
        None,
        Field::choice(
            &spec::STATS_POSITION,
            &row,
            index::stats_position,
            |s, i| s.hud_placement = at(&HudCorner::ALL, i).as_name().to_string(),
        ),
    );
    let sizes: Vec<String> = STATS_SCALE_PCTS.iter().map(|p| format!("{p} %")).collect();
    let row = b.choice(
        &spec::STATS_SIZE,
        &sizes.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    b.put(
        &mut p,
        None,
        Field::choice(&spec::STATS_SIZE, &row, index::stats_size, |s, i| {
            s.stats_scale_pct = *at(&STATS_SCALE_PCTS, i)
        }),
    );
    let (field, _) = Field::switch(&spec::EXIT_HINT, |s| s.exit_hint, |s, v| s.exit_hint = v);
    b.put(&mut p, None, field);
    if !b.preset_scope {
        p.advanced_row(&clear_art_row(&b.dialog));
    }
    b.finish(p);
}

fn session(b: &mut Build, p: &mut PageB) {
    let g = p.group("Session", "");
    // A preset carries only `fullscreen_on_stream`; the defaults show the three-way picker,
    // which also sets `fullscreen_always`.
    let (field, _) = Field::switch(
        &spec::FULLSCREEN_ON_STREAM,
        |s| s.fullscreen_on_stream,
        |s, v| s.fullscreen_on_stream = v,
    );
    b.put(p, Some(&g), field);
    let row = b.choice(&spec::FULLSCREEN, &["Off", "While streaming", "Always"]);
    b.put(
        p,
        Some(&g),
        Field::choice(
            &spec::FULLSCREEN,
            &row,
            |s| {
                if s.fullscreen_always() {
                    2
                } else {
                    u32::from(s.fullscreen_on_stream)
                }
            },
            |s, i| {
                s.fullscreen_on_stream = i >= 1;
                s.set_fullscreen_always(i == 2);
            },
        ),
    );
    let (field, _) = Field::switch(&spec::AUTO_WAKE, |s| s.auto_wake, |s, v| s.auto_wake = v);
    b.put(p, Some(&g), field);
    let row = b.choice(&spec::START_IN, &StartIn::ALL.map(StartIn::label));
    set_row_subtitle(row.widget(), &start_in_subtitle(b.store));
    b.put(
        p,
        Some(&g),
        Field::choice(
            &spec::START_IN,
            &row,
            |s| {
                let want = StartIn::parse(&s.start_in);
                StartIn::ALL.iter().position(|v| *v == want).unwrap_or(1) as u32
            },
            |s, i| s.start_in = at(&StartIn::ALL, i).as_str().to_string(),
        ),
    );
}

/// Which interface this device opens: the device's, never a preset's.
fn console(b: &mut Build, p: &mut PageB) {
    let g = p.group("Console", "");
    let (field, on) = Field::switch(
        &spec::GAMEPAD_UI,
        |s| s.gamepad_ui() != GamepadUi::Off,
        |s, v| s.set_gamepad_ui_enabled(v),
    );
    b.put(p, Some(&g), field);
    let mode = b.choice(&spec::GAMEPAD_UI_MODE, &["With a controller", "Always"]);
    let w = mode.widget().clone();
    w.set_visible(false);
    on.connect_active_notify(move |r| w.set_visible(r.is_active()));
    b.put(
        p,
        Some(&g),
        Field::choice(
            &spec::GAMEPAD_UI_MODE,
            &mode,
            |s| u32::from(s.gamepad_ui_always()),
            |s, i| s.set_gamepad_ui_always(i == 1),
        ),
    );
}

fn omarchy(b: &mut Build, p: &mut PageB) {
    let g = p.group("Omarchy", "");
    let (field, _) = Field::switch(
        &spec::FOLLOW_OS_THEME,
        |s| s.follow_os_theme,
        |s, v| {
            s.follow_os_theme = v;
            // Live: the switch must not wait out the shell's 2 s poll.
            crate::desktop::omarchy::set_enabled(v);
        },
    );
    b.put(p, Some(&g), field);
    b.put(p, Some(&g), omarchy_menu());
}

/// Not a setting: the block's presence in the user's omarchy-menu.jsonc is the state, so two
/// installs cannot disagree with it.
fn omarchy_menu() -> Field {
    use pf_client_core::omarchy_menu as menu;
    let row = adw::SwitchRow::builder()
        .title(spec::OMARCHY_MENU.title)
        .subtitle(spec::OMARCHY_MENU.caption)
        .build();
    let r = row.clone();
    Field::new(
        &spec::OMARCHY_MENU,
        &row,
        vec![row.clone().upcast()],
        {
            let r = r.clone();
            move |_| r.set_active(menu::enabled())
        },
        {
            let r = r.clone();
            move |_| {
                let res = match (r.is_active(), menu::enabled()) {
                    (true, false) => menu::enable(),
                    (false, true) => menu::disable(),
                    _ => Ok(()),
                };
                if let Err(e) = res {
                    tracing::warn!(error = %e, "omarchy menu not written");
                }
            }
        },
        || false,
        move |f| {
            r.connect_active_notify(move |_| f());
        },
    )
}

/// Names the host Start in resolves to, and says when it resolves to nothing, which is what
/// every value does until a host is paired.
fn start_in_subtitle(store: &Store) -> String {
    let known = store.hosts();
    match start::default_host(&store.settings(), &known) {
        Some(i) => format!(
            "Library opens {}'s games; Stream also connects to its desktop",
            known.hosts[i].name
        ),
        None => "Opens on the host list: there is no default host yet".into(),
    }
}

fn docs_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("What each number means")
        .subtitle("docs.punktfunk.unom.io/docs/stats")
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("adw-external-link-symbolic"));
    row.connect_activated(|_| {
        gtk::UriLauncher::new("https://docs.punktfunk.unom.io/docs/stats").launch(
            None::<&gtk::Window>,
            gtk::gio::Cancellable::NONE,
            |_| {},
        );
    });
    row
}

fn clear_art_row(dialog: &adw::PreferencesDialog) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Clear cached art")
        .subtitle("Posters are kept on this device so a library opens before the host answers")
        .activatable(true)
        .build();
    row.add_suffix(&crate::widgets::lucide::row_icon("trash-2"));
    let dialog = dialog.downgrade();
    row.connect_activated(move |_| {
        let toast = match pf_client_core::art_cache::clear() {
            Ok(()) => "Cached art cleared".to_string(),
            Err(e) => format!("Couldn't clear the cached art \u{2014} {e}"),
        };
        if let Some(d) = dialog.upgrade() {
            d.add_toast(adw::Toast::new(&toast));
        }
    });
    row
}
