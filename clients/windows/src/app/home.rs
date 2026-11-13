//! Home: the navigation pane with Hosts and Library, and the destination it points at. The
//! pane's footer leaves Home for the console, the shortcuts reference and Settings; each comes
//! back here.

use super::host_page::{HostPage, HostPageProps};
use super::hosts::{self, HostsPage, HostsProps};
use super::style::*;
use super::{library, lucide, rules, Dest, Msg, Shell};
use pf_l10n::{client, desktop};
use windows_reactor::*;

/// The pane's width open with its labels, and as the icon rail.
pub(super) const PANE_OPEN: f64 = 220.0;
pub(super) const PANE_RAIL: f64 = 48.0;

pub(super) fn home(shell: &Shell, cx: &mut ViewContext<'_, Shell>) -> View {
    let pal = &shell.pal;
    let item = |tag: &'static str, label: &str, mark: &str, picked: bool| {
        KeyedView::new(
            tag,
            NavigationViewItem::new()
                .tag(tag)
                .content(label)
                .icon(lucide::icon(mark, pal))
                .is_selected(picked),
        )
    };
    let dests = [
        item(
            "hosts",
            client::hosts_tab(),
            "monitor",
            shell.dest == Dest::Hosts,
        ),
        item(
            "library",
            client::hosts_tab_library(),
            "gamepad-2",
            shell.dest == Dest::Library,
        ),
    ];
    // The footer's items are ways out of Home, never the picked one.
    let mut footer = Vec::new();
    if hosts::CONSOLE_UI_AVAILABLE {
        footer.push(item(
            "console",
            desktop::desktop_menu_console_ui(),
            "tv",
            false,
        ));
    }
    footer.push(item(
        "help",
        desktop::desktop_menu_shortcuts(),
        "keyboard",
        false,
    ));
    footer.push(item(
        "settings",
        client::hosts_open_settings(),
        "settings",
        false,
    ));

    let svc = shell.svc();
    let page: View = match (shell.dest, &shell.host_page) {
        (Dest::Hosts, Some(who)) => component::<HostPage>(
            // Keyed by the record, so another host's page starts with its own drafts.
            format!("host:{}", who.seed_key()),
            HostPageProps {
                svc: svc.clone(),
                who: who.clone(),
                hosts: shell.hosts.clone(),
                probed: shell.probed.clone(),
                hosts_rev: shell.hosts_rev,
                width: shell.content_width(),
            },
        )
        .into(),
        (Dest::Hosts, None) => component::<HostsPage>(
            "hosts",
            HostsProps {
                svc: svc.clone(),
                hosts: shell.hosts.clone(),
                probed: shell.probed.clone(),
                show_add: shell.show_add,
                add_anim: shell.add_enter.eased(),
                hosts_rev: shell.hosts_rev,
                width: shell.content_width(),
                set_show_add: shell.set(Msg::ShowAdd),
            },
        )
        .into(),
        (Dest::Library, _) => library::library_page(shell),
    };
    // A destination, or a host's page, rises in under the pane, which stays put.
    let progress = shell.page_enter.eased();
    let page = Border::new()
        .content(page)
        .grid_row(1)
        .opacity(progress)
        .translation(0.0, (1.0 - progress) * 22.0);

    // Failures, progress and finished actions share this line, and the severity tells them
    // apart. Each message is a whole sentence, so the bar carries no title of its own.
    let status: View = match shell.status.is_empty() {
        true => Border::new().into(),
        false => Border::new()
            .content(
                InfoBar::new()
                    .message(&shell.status)
                    .severity(shell.status_severity)
                    .is_open(true)
                    .is_closable(false),
            )
            .margin(edges(GUTTER, 12.0, GUTTER, 0.0))
            .into(),
    };

    let (mode, open) = match rules::pane(shell.width, shell.pane_choice, shell.pane_over) {
        rules::Pane::Over(open) => (NavigationViewPaneDisplayMode::LeftMinimal, open),
        rules::Pane::Beside(open) => (NavigationViewPaneDisplayMode::Left, open),
    };
    NavigationView::new()
        .pane_display_mode(mode)
        .is_pane_open(open)
        .open_pane_length(PANE_OPEN)
        // The title bar carries the toggle and Back.
        .is_pane_toggle_button_visible(false)
        .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
        .is_settings_visible(false)
        .always_show_header(false)
        .keyed_menu_items(dests)
        .keyed_footer_menu_items(footer)
        .on_selected_tag_changed(
            cx.callback(|tag: Option<std::rc::Rc<str>>| Msg::Nav(tag.map(|t| t.to_string()))),
        )
        .on_is_pane_open_changed(cx.callback(Msg::PaneOver))
        .content(
            Grid::new()
                .children((status, page))
                .rows([GridLength::Auto, GridLength::STAR]),
        )
        .into()
}
