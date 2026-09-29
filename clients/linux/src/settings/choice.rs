//! The single-choice row every picker uses: a combo on a desktop, an in-window subpage under
//! gamescope, where popovers never map. Also the caption helpers every row shares.

use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// True inside a gamescope session (Steam game mode on the Deck / Bazzite): GTK popovers
/// are xdg_popups, which gamescope never maps for nested apps — a ComboRow's dropdown
/// flashes the row but no list ever appears. Selection UI must stay inside the toplevel.
pub fn gamescope_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.eq_ignore_ascii_case("gamescope"))
        || pf_client_core::gamescope::under_gamescope()
}

type ChangedFn = Rc<RefCell<Vec<Rc<dyn Fn(u32)>>>>;

/// A titled single-choice preference row. On a desktop this is a stock popover
/// [`adw::ComboRow`]; under gamescope (see [`gamescope_session`]) it becomes an activatable
/// row that pushes an in-window selection subpage onto the preferences dialog instead.
#[derive(Clone)]
pub struct ChoiceRow {
    row: adw::PreferencesRow,
    selected: Rc<Cell<u32>>,
    /// Fires on user changes only — handlers are installed after seeding, so programmatic
    /// `set_selected` during setup never fires them. A list, not one: a row can carry both a
    /// dynamic caption and (in preset scope) the override mark.
    changed: ChangedFn,
    /// Subpage mode only: the current value rendered as the row's suffix.
    value_label: Option<gtk::Label>,
    options: Rc<RefCell<Vec<String>>>,
}

pub fn string_list(options: &[String]) -> gtk::StringList {
    gtk::StringList::new(&options.iter().map(String::as_str).collect::<Vec<_>>())
}

impl ChoiceRow {
    /// `inline` = subpage mode (gamescope): computed once per dialog via
    /// [`gamescope_session`] and passed in so tests can drive both modes directly.
    pub fn new(
        dialog: &adw::PreferencesDialog,
        inline: bool,
        title: &str,
        subtitle: &str,
        options: &[&str],
    ) -> ChoiceRow {
        let options: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(
            options.iter().map(|s| s.to_string()).collect(),
        ));
        let selected = Rc::new(Cell::new(0u32));
        let changed: ChangedFn = Rc::new(RefCell::new(Vec::new()));

        if !inline {
            let row = adw::ComboRow::builder()
                .title(title)
                .subtitle(subtitle)
                .model(&string_list(&options.borrow()))
                .build();
            let (sel, chg) = (selected.clone(), changed.clone());
            row.connect_selected_notify(move |r| {
                if sel.replace(r.selected()) != r.selected() {
                    // Cloned out first, so a handler may touch the list while the loop runs.
                    let fns: Vec<Rc<dyn Fn(u32)>> = chg.borrow().clone();
                    for f in &fns {
                        f(r.selected());
                    }
                }
            });
            return ChoiceRow {
                row: row.upcast(),
                selected,
                changed,
                value_label: None,
                options,
            };
        }

        let value = gtk::Label::builder().css_classes(["dim-label"]).build();
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .build();
        row.add_suffix(&value);
        row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
        {
            let dialog = dialog.downgrade();
            let (options, sel, chg, value) = (
                options.clone(),
                selected.clone(),
                changed.clone(),
                value.clone(),
            );
            let title = title.to_string();
            row.connect_activated(move |_| {
                let Some(dialog) = dialog.upgrade() else {
                    return;
                };
                let list = gtk::ListBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .css_classes(["boxed-list"])
                    .build();
                for (i, opt) in options.borrow().iter().enumerate() {
                    let check = crate::widgets::lucide::row_icon("check");
                    check.set_visible(i as u32 == sel.get());
                    let opt_row = adw::ActionRow::builder()
                        .title(opt)
                        .use_markup(false)
                        .activatable(true)
                        .build();
                    opt_row.add_suffix(&check);
                    let idx = i as u32;
                    let dlg = dialog.downgrade();
                    let (sel, chg, value, label) =
                        (sel.clone(), chg.clone(), value.clone(), opt.clone());
                    opt_row.connect_activated(move |_| {
                        let user_change = sel.replace(idx) != idx;
                        value.set_text(&label);
                        if user_change {
                            for f in chg.borrow().iter() {
                                f(idx);
                            }
                        }
                        if let Some(d) = dlg.upgrade() {
                            d.pop_subpage();
                        }
                    });
                    list.append(&opt_row);
                }
                let clamp = adw::Clamp::builder()
                    .child(&list)
                    .margin_top(24)
                    .margin_bottom(24)
                    .margin_start(12)
                    .margin_end(12)
                    .build();
                let scroll = gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&clamp)
                    .build();
                let view = adw::ToolbarView::new();
                view.add_top_bar(&adw::HeaderBar::new());
                view.set_content(Some(&scroll));
                dialog.push_subpage(&adw::NavigationPage::new(&view, &title));
            });
        }
        let cr = ChoiceRow {
            row: row.upcast(),
            selected,
            changed,
            value_label: Some(value),
            options,
        };
        cr.sync_value();
        cr
    }

    /// Subpage mode: reflect the current selection in the row's suffix label.
    fn sync_value(&self) {
        if let Some(l) = &self.value_label {
            let i = self.selected.get() as usize;
            l.set_text(
                self.options
                    .borrow()
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or(""),
            );
        }
    }

    /// Swap the option list without a change; the caller re-seats the selection. A combo lands
    /// on row 0 with its new model, so the cell moves there first and the notify finds nothing.
    pub fn set_options(&self, options: &[String]) {
        *self.options.borrow_mut() = options.to_vec();
        self.selected.set(0);
        if let Some(combo) = self.row.downcast_ref::<adw::ComboRow>() {
            combo.set_model(Some(&string_list(options)));
        }
        self.sync_value();
    }

    pub fn widget(&self) -> &adw::PreferencesRow {
        &self.row
    }

    pub fn selected(&self) -> u32 {
        self.selected.get()
    }

    pub fn set_selected(&self, i: u32) {
        if let Some(combo) = self.row.downcast_ref::<adw::ComboRow>() {
            combo.set_selected(i); // the notify handler syncs the cell and dispatches
        } else {
            let moved = self.selected.replace(i) != i;
            self.sync_value();
            // Subpage mode (gamescope) has no `notify` to ride, so it has to dispatch what
            // the combo branch gets for free — a per-row Reset reverts through here, and the
            // rows whose caption or visibility follow this one are updated by these handlers.
            if moved {
                // Cloned out first, for the same reason as the combo notify above.
                let fns: Vec<Rc<dyn Fn(u32)>> = self.changed.borrow().clone();
                for f in &fns {
                    f(i);
                }
            }
        }
    }

    pub fn connect_changed(&self, f: impl Fn(u32) + 'static) {
        self.changed.borrow_mut().push(Rc::new(f));
    }

    /// A handle for putting this row's selection back from inside its own handler. The cell
    /// is held weakly, so a row never owns the closure that owns the row.
    pub fn restorer(&self) -> RowRestore {
        RowRestore {
            row: self.row.clone(),
            selected: Rc::downgrade(&self.selected),
        }
    }
}

/// See [`ChoiceRow::restorer`].
pub struct RowRestore {
    row: adw::PreferencesRow,
    selected: std::rc::Weak<Cell<u32>>,
}

/// Move a row's selection without running its handlers — for a handler that has just decided
/// the change should not stand. The cell moves first: GObject replays a nested `notify` after
/// the running one returns, and the combo's handler then sees no change and dispatches nothing.
pub fn restore_selected(r: &RowRestore, i: u32) {
    let Some(selected) = r.selected.upgrade() else {
        return;
    };
    selected.set(i);
    if let Some(combo) = r.row.downcast_ref::<adw::ComboRow>() {
        combo.set_selected(i);
    }
}

/// Update a row's caption after construction — the dynamic-caption hook (touch mode,
/// resolution, codec). Both ChoiceRow shapes carry their subtitle on [`adw::ActionRow`]
/// ([`adw::ComboRow`] derives from it), so one downcast covers desktop and gamescope mode.
pub fn set_row_subtitle(row: &adw::PreferencesRow, text: &str) {
    if let Some(r) = row.downcast_ref::<adw::ActionRow>() {
        r.set_subtitle(text);
        cap_subtitle(r);
    }
}

/// How wide a row's caption may get before it wraps. Rows put the title and subtitle in one
/// box and the control in the suffix, and that box asks for as much width as its longest line
/// wants — so a long caption starves the value label next to it, which then ellipsizes ("2×
/// (su…"). Capping the caption gives the width back to the control.
///
/// The repo's standing rule was "keep captions to one line (~66 chars)", which works until a
/// row grows another suffix — the override marker's reset button did exactly that. A cap is
/// the structural version of the same rule: it holds however many suffixes a row ends up with.
const CAPTION_CHARS: i32 = 46;

/// Cap a row's caption width. The subtitle label isn't exposed by libadwaita, so it is found by
/// matching its text — a miss simply leaves the row as it was, which is the pre-cap behaviour.
pub fn cap_subtitle(row: &adw::ActionRow) {
    let subtitle = row.subtitle().unwrap_or_default();
    if subtitle.is_empty() {
        return;
    }
    fn walk(w: &gtk::Widget, want: &str) -> Option<gtk::Label> {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            if l.label() == want {
                return Some(l.clone());
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(hit) = walk(&c, want) {
                return Some(hit);
            }
            child = c.next_sibling();
        }
        None
    }
    if let Some(label) = walk(row.upcast_ref(), &subtitle) {
        label.set_wrap(true);
        label.set_max_width_chars(CAPTION_CHARS);
        label.set_xalign(0.0);
        // `max_width_chars` alone only caps what the label ASKS for; an expanding label still
        // fills whatever the row hands it and goes back to one long line. Refusing the expand
        // is what actually leaves the width for the control beside it.
        label.set_hexpand(false);
        // …and Start, not the default Fill: a filled label is allocated the whole box and
        // draws on one long line regardless of the cap. Start alignment makes the allocation
        // equal the (now capped) natural width, which is what makes the cap visible.
        label.set_halign(gtk::Align::Start);
    }
}

/// Walk a page and cap every caption it contains (see [`cap_subtitle`]).
pub fn cap_captions(root: &gtk::Widget) {
    if let Some(row) = root.downcast_ref::<adw::ActionRow>() {
        cap_subtitle(row);
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        cap_captions(&c);
        child = c.next_sibling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Depth-first search for an [`adw::ActionRow`] with the given title.
    fn find_action_row(root: &gtk::Widget, title: &str) -> Option<adw::ActionRow> {
        if let Some(row) = root.downcast_ref::<adw::ActionRow>() {
            if row.title() == title {
                return Some(row.clone());
            }
        }
        let mut child = root.first_child();
        while let Some(c) = child {
            if let Some(hit) = find_action_row(&c, title) {
                return Some(hit);
            }
            child = c.next_sibling();
        }
        None
    }

    fn pump() {
        let ctx = gtk::glib::MainContext::default();
        while ctx.iteration(false) {}
    }

    /// Both ChoiceRow modes in ONE test (GTK is thread-affine and libtest gives every test
    /// its own thread, so the display tests can't be split). Gamescope mode: activating the
    /// row pushes the in-window selection subpage; activating an option updates the
    /// selection + suffix label, fires the change callback, and pops the subpage. Combo
    /// mode: cell sync + change callback. Needs a display AND its own process: `--ignored` alone
    /// starts every display test in one process, and GTK refuses a second init from a second
    /// thread. Run it by name on a session box:
    /// `cargo test -p punktfunk-client-linux -- --ignored choice_row_modes`.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn choice_row_modes() {
        assert!(gtk::init().is_ok() && adw::init().is_ok(), "no display");
        let win = adw::Window::new();
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let row = ChoiceRow::new(&dialog, true, "Resolution", "sub", &["A", "B", "C"]);
        group.add(row.widget());
        page.add(&group);
        dialog.add(&page);
        let fired = Rc::new(Cell::new(u32::MAX));
        {
            let f = fired.clone();
            row.connect_changed(move |i| f.set(i));
        }
        win.present();
        dialog.present(Some(&win));
        pump();

        // Suffix label reflects the seed.
        assert_eq!(row.value_label.as_ref().unwrap().text(), "A");

        // Row activation → subpage with the options list.
        row.widget()
            .downcast_ref::<adw::ActionRow>()
            .unwrap()
            .emit_by_name::<()>("activated", &[]);
        pump();
        let opt_b = find_action_row(dialog.upcast_ref(), "B").expect("subpage option missing");

        // Option activation → state + label + callback, subpage popped.
        opt_b.emit_by_name::<()>("activated", &[]);
        pump();
        assert_eq!(row.selected(), 1);
        assert_eq!(fired.get(), 1);
        assert_eq!(row.value_label.as_ref().unwrap().text(), "B");

        // The dynamic-caption hook drives the same subtitle both row shapes expose.
        set_row_subtitle(row.widget(), "swapped");
        assert_eq!(
            row.widget()
                .downcast_ref::<adw::ActionRow>()
                .unwrap()
                .subtitle()
                .as_deref(),
            Some("swapped")
        );

        // Re-activating shows the check on the new selection (fresh subpage each time).
        row.widget()
            .downcast_ref::<adw::ActionRow>()
            .unwrap()
            .emit_by_name::<()>("activated", &[]);
        pump();
        assert!(find_action_row(dialog.upcast_ref(), "B").is_some());

        // Desktop (ComboRow) mode: cell sync + change callback on selection change.
        let combo = ChoiceRow::new(&dialog, false, "Codec", "", &["X", "Y"]);
        combo.set_selected(1);
        assert_eq!(combo.selected(), 1);
        let combo_fired = Rc::new(Cell::new(u32::MAX));
        {
            let f = combo_fired.clone();
            combo.connect_changed(move |i| f.set(i));
        }
        combo.set_selected(0);
        assert_eq!(combo.selected(), 0);
        assert_eq!(combo_fired.get(), 0);
        // ComboRow derives from ActionRow, so the caption hook reaches it too.
        set_row_subtitle(combo.widget(), "combo caption");
        assert_eq!(
            combo
                .widget()
                .downcast_ref::<adw::ActionRow>()
                .unwrap()
                .subtitle()
                .as_deref(),
            Some("combo caption")
        );
    }

    /// A handler may put its own row back (`restore_selected`, behind "New preset…"). That
    /// neither aborts on a borrowed handler list nor runs the handlers again. Both modes.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn choice_row_handler_may_restore_selection() {
        assert!(gtk::init().is_ok() && adw::init().is_ok(), "no display");
        let dialog = adw::PreferencesDialog::new();
        for inline in [true, false] {
            let row = ChoiceRow::new(&dialog, inline, "Editing", "sub", &["Global", "New…"]);
            let restore = row.restorer();
            let fired = Rc::new(Cell::new(0u32));
            let f = fired.clone();
            row.connect_changed(move |i| {
                f.set(f.get() + 1);
                if i == 1 {
                    restore_selected(&restore, 0);
                }
            });
            row.set_selected(1);
            assert_eq!(fired.get(), 1, "inline={inline}: the handler ran once");
            assert_eq!(
                row.selected(),
                0,
                "inline={inline}: restored without a re-dispatch"
            );
        }
    }
}
