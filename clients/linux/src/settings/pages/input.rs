//! Input: touch, keyboard and mouse, the quick-action dial, and the in-stream shortcuts.

use super::display::caption_follows;
use super::Build;
use crate::settings::field::Field;
use crate::settings::quick_actions::QuickActions;
use crate::settings::shortcuts::{ring_chords, ShortcutsPage};
use crate::settings::spec::{self, Page};
use crate::settings::tables::*;
use adw::prelude::*;
use pf_l10n::{client, desktop};

pub fn input(b: &mut Build) {
    let mut p = b.page(Page::Input, "input-keyboard-symbolic");
    let touch = p.group(client::prefs_group_touch_pointer(), "");
    let row = b.choice(&spec::TOUCH, &touch_mode_labels());
    caption_follows(&row, TOUCH_MODE_CAPTIONS);
    b.put(
        &mut p,
        Some(&touch),
        Field::choice(&spec::TOUCH, &row, index::touch, |s, i| {
            s.touch_mode = at(TOUCH_MODES, i).to_string()
        }),
    );
    let kbm = p.group(client::prefs_group_keyboard_mouse(), "");
    let row = b.choice(&spec::MOUSE, &mouse_mode_labels());
    caption_follows(&row, MOUSE_MODE_CAPTIONS);
    b.put(
        &mut p,
        Some(&kbm),
        Field::choice(&spec::MOUSE, &row, index::mouse, |s, i| {
            s.mouse_mode = at(MOUSE_MODES, i).to_string()
        }),
    );
    let (field, _) = Field::switch(
        &spec::SHORTCUTS,
        |s| s.inhibit_shortcuts,
        |s, v| s.inhibit_shortcuts = v,
    );
    b.put(&mut p, Some(&kbm), field);
    let (field, _) = Field::switch(
        &spec::INVERT_SCROLL,
        |s| s.invert_scroll,
        |s, v| s.invert_scroll = v,
    );
    b.put(&mut p, Some(&kbm), field);

    let opens = ring_chords()
        .map(|(keys, pad)| desktop::desktop_player_overlay_actions_hint(keys, pad))
        .unwrap_or_default();
    let ring = p.group(client::player_overlay_actions(), &opens);
    let quick = QuickActions::new(&b.dialog, &b.seed.overlay_actions);
    let keys = p.group("", "");
    let bindings = shortcuts(b, &quick);
    b.put(&mut p, Some(&ring), quick_actions(quick));
    b.put(&mut p, Some(&keys), bindings);
    b.finish(p);
}

/// The ring's row opens its editor; every edit the editor makes is a change.
fn quick_actions(quick: QuickActions) -> Field {
    let row = quick.row().clone();
    Field::new(
        &spec::QUICK_ACTIONS,
        &row,
        vec![row.clone().upcast()],
        {
            let q = quick.clone();
            move |s| q.set_blob(&s.overlay_actions)
        },
        {
            let blob = quick.blob();
            move |s| s.overlay_actions = blob.borrow().clone()
        },
        || false,
        move |f| quick.connect_changed(move || f()),
    )
}

/// The shortcuts' row opens their page; every edit the page makes is a change. Its key
/// combinations are the ring editor's as edited so far.
fn shortcuts(b: &Build, quick: &QuickActions) -> Field {
    let page = ShortcutsPage::new(&b.dialog, b.seed, quick.blob());
    let row = page.row().clone();
    Field::new(
        &spec::PLAYER_SHORTCUTS,
        &row,
        vec![row.clone().upcast()],
        {
            let page = page.clone();
            move |s| page.set(s)
        },
        {
            let blob = page.blob();
            move |s| s.shortcuts = blob.borrow().clone()
        },
        || false,
        move |f| page.connect_changed(move || f()),
    )
}
