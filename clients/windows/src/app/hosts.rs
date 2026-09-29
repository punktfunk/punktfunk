//! The hosts page: saved (trusted/paired) hosts and live mDNS discovery as tap-to-connect
//! tiles in a responsive grid, with a per-host "…" menu (connect / speed test / edit /
//! forget) and a manual connect entry — the same card layout as the Linux and Apple clients.

use super::connect::{initiate, initiate_waking, open_console};
use super::lucide;
use super::speed::SpeedState;
use super::style::*;
use super::{Screen, Svc, Target};
use crate::trust::{HostEdit, KnownHosts, Settings};
use pf_client_core::discovery::DiscoveredHost;
use std::collections::HashMap;
use windows_reactor::*;

/// Overflow-menu item labels — `on_item_clicked` reports the clicked item by its text.
const MENU_CONNECT: &str = "Connect";
const MENU_LIBRARY: &str = "Browse library\u{2026}";
const MENU_SPEED: &str = "Test network speed\u{2026}";
/// Upload this device's recent log ring to the host (`logring::send_to_host`), where the web
/// console's Logs page lists it beside the host's own log. Paired + online only — the same
/// gate as the console UI's row, because the upload authenticates with the paired identity
/// and an offline host could only ever report an error.
const MENU_SEND_LOGS: &str = "Send logs to host";
const MENU_WAKE: &str = "Wake host";
/// The host's OWN actions — sleep / restart / shut down it (`design/host-actions.md` §7) —
/// each prefixed so the shared click callback can tell them from the fixed entries and recover
/// which one was picked. The rows come from what the HOST said it lets this device do, so a
/// device without the Host-power grant sees none, and a later host can add one without a
/// client release. Same shape as [`MENU_PIN`]'s dynamic family, for the same reason.
const MENU_HOST_ACTION: &str = "\u{23fb} ";

/// One host action's menu label. Used to BUILD the row and to recognise it again in the click
/// callback — one function, so the two can never disagree, and the match stays exact rather
/// than a prefix test that two similarly-named actions could both satisfy.
#[cfg(windows)]
fn host_action_label(a: &pf_client_core::host_actions::ActionInfo) -> String {
    format!(
        "{MENU_HOST_ACTION}{}{}",
        a.label(),
        if a.available { "" } else { " (unavailable)" }
    )
}
/// One entry for every per-host property (name, address, MAC, clipboard sharing) — the
/// Apple client's add/edit sheet. A menu item per field read as clutter and buried the ones
/// that matter.
const MENU_EDIT: &str = "Edit\u{2026}";
/// The per-preset families nest in submenus. Submenu LEAVES are what the shared click
/// callback reports (the backend wires clicks recursively and hands back the leaf text):
/// "Connect with"'s leaves are the bare preset names + [`SUB_WITH_DEFAULT`]; "Pin tiles"'s
/// leaves keep a verb prefix, which is what tells the two families apart in the callback.
/// (A preset literally named like a fixed entry, e.g. "Connect", is shadowed by it — the
/// same last-wins rule the scope dropdown documents.)
const SUB_WITH: &str = "Connect with";
const SUB_WITH_DEFAULT: &str = "Default settings";
const SUB_PIN: &str = "Pin tiles";
const MENU_COPY_LINK: &str = "Copy link";
const MENU_SHORTCUT: &str = "Create shortcut\u{2026}";
const MENU_PIN: &str = "Pin tile: ";
const MENU_UNPIN: &str = "Unpin tile: ";
const MENU_FORGET: &str = "Forget\u{2026}";
/// Point `Settings::default_host` at this host, or clear it. Two strings rather than a
/// checkmark toggle: the flyout reports the leaf TEXT, so the two states must not share one.
const MENU_DEFAULT: &str = "Make default host";
const MENU_DEFAULT_SET: &str = "Default host \u{2713}";

/// Whether the console (gamepad) UI is available in this build: the session binary ships
/// its Skia `ui` feature on x64 only (no skia prebuilts for aarch64 yet) — the entry
/// points compile everywhere but only show where `--browse` can actually run.
const CONSOLE_UI_AVAILABLE: bool = cfg!(target_arch = "x86_64");

/// Tile-grid metrics: minimum tile width before dropping a column, and the gap between tiles.
const TILE_MIN_WIDTH: f64 = 320.0;
const TILE_GAP: f64 = 12.0;

/// Props for the hosts page: the services plus the changing discovery/status data that must
/// drive its re-render (compared by value, so a new host list or error refreshes the page).
///
/// Which saved record a per-host action means. NOT the fingerprint: a host added by address
/// has none, and keying on `""` matched the first such record — Forget removed the wrong set,
/// Edit and pinning wrote to the wrong host. `addr`/`port` are the fallback for a record older
/// than the minted ids, and are also why the id is the durable key: the edit sheet can change
/// them under itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct HostRef {
    pub(crate) id: Option<String>,
    pub(crate) addr: String,
    pub(crate) port: u16,
    pub(crate) name: String,
}

impl HostRef {
    fn of(h: &pf_client_core::trust::KnownHost) -> Self {
        Self {
            id: h.id.clone(),
            addr: h.addr.clone(),
            port: h.port,
            name: h.name.clone(),
        }
    }

    /// The record this names, if it is still in the store.
    fn index(&self, known: &KnownHosts) -> Option<usize> {
        known.index_of_card(self.id.as_deref(), &self.addr, self.port)
    }
}

/// `forget` and `rename` are the per-host action state, and they live in ROOT (not this page's
/// own `use_state`) on purpose: the "…" overflow is a WinUI `MenuFlyout`, whose item clicks are
/// wired directly in the reactor backend (`add_Click`) and so bypass the normal event-dispatch
/// flush — a *sync* child `SetState` from that handler marks state dirty but never pumps the
/// reconciler, so nothing re-renders. Root `AsyncSetState` re-renders the whole tree; because
/// these values are props, the changed value propagates back into this page (a child's own async
/// state would be memoised away when its props are unchanged). `(fp_hex, _)` in each identifies
/// the target saved host; `rename`'s second field is the in-progress draft name.
#[derive(Clone)]
pub(crate) struct HostsProps {
    pub(crate) svc: Svc,
    pub(crate) hosts: Vec<DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe, keyed by `KnownHost::card_key` —
    /// the whole of the Online pip. A routed host (Tailscale/VPN) that never advertises reads
    /// Online here, and a sleeping one whose advert has not aged out yet reads Offline.
    pub(crate) probed: HashMap<String, bool>,
    pub(crate) status: String,
    /// Connected-controller count (root state, mirrored from the gamepad service) — a
    /// pad plus a paired host surfaces the "Open console UI" hint card.
    pub(crate) pads: usize,
    pub(crate) forget: Option<HostRef>,
    pub(crate) rename: Option<HostRef>,
    /// Whether the "Add host" modal is open. Root state (like `forget`/`rename`), not the page's
    /// own `use_state`: a child component's sync `SetState` marks its slot dirty but does not
    /// re-render when its props are otherwise unchanged, so the toggle wouldn't take.
    pub(crate) show_add: bool,
    /// The modal's entrance-tween progress (0 → 1, root-driven): opacity + slide-up offset.
    pub(crate) add_anim: f64,
    /// The hovered tile's stable id (saved: fp_hex, discovered: `addr:port`) — root state because
    /// the pointer enter/exit handlers bypass the reconciler flush, like the flyout clicks above.
    pub(crate) hover: Option<String>,
    /// Bumped when a menu action changes what the page should SHOW without changing any
    /// state it already reads — pinning/unpinning a preset tile, which rewrites the
    /// known-hosts store behind the tiles (the hosts-page mirror of `settings_rev`).
    pub(crate) hosts_rev: u64,
    pub(crate) set_forget: AsyncSetState<Option<HostRef>>,
    pub(crate) set_rename: AsyncSetState<Option<HostRef>>,
    pub(crate) set_show_add: AsyncSetState<bool>,
    pub(crate) set_hover: AsyncSetState<Option<String>>,
    pub(crate) set_hosts_rev: AsyncSetState<u64>,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        // Setters are identity-stable; only the value fields drive re-render.
        self.svc == other.svc
            && self.hosts == other.hosts
            && self.probed == other.probed
            && self.status == other.status
            && self.pads == other.pads
            && self.forget == other.forget
            && self.rename == other.rename
            && self.show_add == other.show_add
            && self.add_anim == other.add_anim
            && self.hover == other.hover
            && self.hosts_rev == other.hosts_rev
    }
}

/// A host tile. The tap-to-connect summary (monogram, name, address, status row) and the
/// optional "…" menu button are SIBLINGS overlaid in one grid cell, never nested: WinUI bubbles
/// `Tapped` out of buttons (reactor doesn't mark it handled), so a button inside the tap target
/// would fire both its own click and the tile's connect (the old forget-also-connects bug).
///
/// Hover renders the WinUI card pointer-over look — the card background lifts to the control
/// hover fill while the pointer is inside the tile (tracked via `hover`, see `HostsProps`).
// Three call sites, each passing a different mix of the optional tail — grouping the eight into a
// props struct would make every one of them construct it inline for no reader gain.
#[allow(clippy::too_many_arguments)]
fn host_tile(
    id: &str,
    hover: &Hover,
    name: &str,
    // The host's OS-identity chain — the avatar's mark. Empty for a host that advertises
    // none, which falls the avatar back to the name's initial. (`//`, not `///`: rustc rejects
    // a doc comment on a parameter.)
    os: &str,
    sub: &str,
    status_row: Element,
    menu: Option<Button>,
    on_tap: Option<Box<dyn Fn()>>,
) -> Element {
    let mut summary = border(
        vstack((
            avatar(name, os).horizontal_alignment(HorizontalAlignment::Left),
            text_block(name)
                .font_size(15.0)
                .semibold()
                .wrap()
                .margin(edges(0.0, 12.0, 0.0, 0.0)),
            text_block(sub)
                .font_size(12.0)
                .font_family("Consolas")
                .foreground(ThemeRef::SecondaryText)
                .margin(edges(0.0, 2.0, 0.0, 0.0)),
            status_row,
        ))
        .spacing(0.0),
    )
    .background(hit_test_backstop())
    .padding(uniform(18.0));
    if let Some(f) = on_tap {
        summary = summary.on_tapped(f);
    }

    let mut children: Vec<Element> = vec![summary.into()];
    if let Some(m) = menu {
        children.push(
            m.horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(0.0, 8.0, 8.0, 0.0))
                .into(),
        );
    }
    let mut tile = card_flush(grid(children));
    if hover.current.as_deref() == Some(id) {
        tile = tile.background(ThemeRef::ControlFillSecondary);
    }
    let enter = {
        let (set, id) = (hover.set.clone(), id.to_string());
        move |_: PointerEventInfo| set.call(Some(id.clone()))
    };
    let exit = {
        let set = hover.set.clone();
        move || set.call(None)
    };
    tile.on_pointer_entered(enter)
        .on_pointer_exited(exit)
        .into()
}

/// The hover-tracking pair `host_tile` needs: the currently hovered tile id + its root setter.
pub(crate) struct Hover {
    pub(crate) current: Option<String>,
    pub(crate) set: AsyncSetState<Option<String>>,
}

/// The status row at the bottom of a tile: the host's OS mark (when advertised), presence
/// dot + Online/Offline, plus a trust chip only where it says something (see
/// [`status_row_with`]).
fn status_row(online: Option<bool>, badge: Option<(&str, Pill)>) -> Element {
    status_row_with(online, badge, None)
}

/// [`status_row`] plus the preset: what a plain click on THIS tile will use — its own
/// preset on a pinned tile, the host's binding on the primary one. A binding whose preset
/// was deleted shows nothing and resolves as the defaults, which is what will happen on
/// connect (design §6).
///
/// The row is METADATA, not a badge shelf — three chips side by side read as noise. Paired
/// is the normal resting state of a saved host, so it earns NO chip at all; a chip appears
/// only where it carries a decision ("Trusted" = TOFU without pairing, "PIN"/"Open" on a
/// discovered host). The preset is a small dot in the preset's own colour plus its name
/// in plain caption text — recognisable at a glance without competing with the host name.
fn status_row_with(
    online: Option<bool>,
    badge: Option<(&str, Pill)>,
    preset: Option<(&str, Option<String>)>,
) -> Element {
    let mut items: Vec<Element> = Vec::new();
    // No OS mark here any more: it moved up into the avatar, where it is the tile's leading
    // visual rather than the smallest thing in the status row.
    if let Some(online) = online {
        items.push(
            presence_dot(online)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        items.push(
            text_block(if online { "Online" } else { "Offline" })
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    if let Some((badge, kind)) = badge {
        items.push(
            pill(badge, kind)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    if let Some((name, accent)) = preset {
        // The preset's own colour where it has one, a neutral disc where it doesn't — the
        // palette stays opt-in, and an unparsable value falls back rather than being trusted.
        let colour = accent
            .as_deref()
            .and_then(super::settings::hex_color)
            .unwrap_or(Color {
                a: 120,
                r: 128,
                g: 128,
                b: 128,
            });
        items.push(
            border(vstack(Vec::<Element>::new()).width(8.0).height(8.0))
                .background(colour)
                .corner_radius(4.0)
                .margin(edges(
                    if items.is_empty() { 0.0 } else { 4.0 },
                    0.0,
                    0.0,
                    0.0,
                ))
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        items.push(
            text_block(name)
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    hstack(items)
        .spacing(6.0)
        .margin(edges(0.0, 12.0, 0.0, 0.0))
        .into()
}

/// The in-tile host editor (a ContentDialog can't hold text fields): every per-host
/// property in one place, mirroring the Apple client's add/edit sheet — name, address,
/// port, Wake-on-LAN MAC, and whether this machine shares its clipboard with the host.
///
/// Drafts live in refs owned by the page and are read at Save time; the root `edit` state
/// carries only the target's fingerprint + initial name, so typing doesn't round-trip
/// through a re-render.
fn edit_editor(
    who: &HostRef,
    initial_name: &str,
    drafts: EditDrafts,
    set_edit: AsyncSetState<Option<HostRef>>,
) -> Element {
    let EditDrafts {
        name: name_draft,
        addr: addr_draft,
        port: port_draft,
        mac: mac_draft,
        clip: clip_draft,
    } = drafts;
    let commit = {
        let (who, se) = (who.clone(), set_edit.clone());
        let (name_draft, addr_draft, port_draft, mac_draft, clip_draft) = (
            name_draft.clone(),
            addr_draft.clone(),
            port_draft.clone(),
            mac_draft.clone(),
            clip_draft.clone(),
        );
        move || {
            let mut known = KnownHosts::load();
            let target = who.index(&known);
            if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                // A cleared box leaves its field as stored, and so does MAC text that doesn't
                // parse; a cleared MAC box clears the MACs.
                let port = port_draft
                    .borrow()
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|&p| p != 0);
                h.apply_edit(&HostEdit {
                    name: Some(name_draft.borrow().clone()),
                    addr: Some(addr_draft.borrow().clone()),
                    port,
                    macs: pf_client_core::wol::parse_mac_list(&mac_draft.borrow()).ok(),
                });
                h.clipboard_sync = *clip_draft.borrow();
            }
            let _ = known.save();
            se.call(None);
        }
    };
    // The preset binding: what a plain click on this tile will use. It commits on change
    // rather than at Save — it is a picker with no draft ref, and the rest of the sheet's
    // fields are text boxes that genuinely need one.
    let preset_picker = {
        let catalog = pf_client_core::presets::PresetsFile::load();
        let known = KnownHosts::load();
        let stored = who
            .index(&known)
            .and_then(|i| known.hosts[i].preset_id.clone());
        let mut names = vec!["Default settings".to_string()];
        let mut ids: Vec<String> = vec![String::new()];
        for p in &catalog.presets {
            names.push(p.name.clone());
            ids.push(p.id.clone());
        }
        // A binding whose preset is gone reads as Default settings — the same "dangling
        // resolves as none" rule the connect path follows — and is cleaned up on the next pick.
        let current = stored
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let who = who.clone();
        ComboBox::new(names)
            .header("Preset")
            .selected_index(current as i32)
            .on_selection_changed(move |i: i32| {
                let Some(id) = ids.get(i.max(0) as usize) else {
                    return;
                };
                let mut known = KnownHosts::load();
                let target = who.index(&known);
                if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                    h.preset_id = (!id.is_empty()).then(|| id.clone());
                    let _ = known.save();
                }
            })
    };
    let field = |label: &str, value: String, placeholder: &str, draft: HookRef<String>| {
        vstack((
            text_block(label)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Left),
            text_box(&value)
                .placeholder_text(placeholder)
                .on_text_changed(move |t: String| draft.set(t)),
        ))
        .spacing(2.0)
    };
    let (name0, addr0, port0, mac0, clip0) = (
        name_draft.borrow().clone(),
        addr_draft.borrow().clone(),
        port_draft.borrow().clone(),
        mac_draft.borrow().clone(),
        *clip_draft.borrow(),
    );
    // A centred SHEET (scrim + card), not an in-grid tile: as a tile the editor inherited a
    // grid cell in the middle of the page, and on an ordinary window its lower half sat
    // below the fold with nothing hinting at it (live-diagnosed 2026-07-29: a control's
    // visible rect was a 9-px sliver). A sheet centres at its own height — and its content
    // sits in a scroll_view, so a short window scrolls the card instead of clipping it.
    // A tap on the scrim, or Escape, cancels (a tap INSIDE the card bubbles to the scrim —
    // the flag makes the scrim swallow exactly that one).
    let inside_tap = std::rc::Rc::new(std::cell::Cell::new(false));
    let cancel = {
        let se = set_edit.clone();
        move || se.call(None)
    };
    let modal = dialog_surface(scroll_view(
        vstack((
            text_block(format!("Edit \u{201c}{initial_name}\u{201d}"))
                .font_size(20.0)
                .bold(),
            field("Name", name0, "e.g. Living Room", name_draft),
            field("Address", addr0, "IP or hostname", addr_draft),
            field("Port", port0, "9777", port_draft),
            field(
                "MAC (Wake-on-LAN)",
                mac0,
                "auto-filled when known",
                mac_draft,
            ),
            vstack((
                preset_picker,
                text_block(
                    "The settings a plain click on this host uses. \u{201c}Connect with\u{201d} \
                     in the tile\u{2019}s menu overrides it for one session without changing it.",
                )
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .horizontal_alignment(HorizontalAlignment::Left),
            ))
            .spacing(4.0),
            vstack((
                ToggleSwitch::new(clip0)
                    .header("Share clipboard with this host")
                    .on_content("On")
                    .off_content("Off")
                    .on_toggled(move |v: bool| clip_draft.set(v)),
                text_block(
                    "Copy on one machine, paste on the other. Off for every host until you \
                     turn it on here; the host must allow it too.",
                )
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .horizontal_alignment(HorizontalAlignment::Left),
            ))
            .spacing(4.0),
            hstack((
                button("Save")
                    .accent()
                    .icon(lucide::icon("check"))
                    .on_click(commit),
                button("Cancel")
                    .subtle()
                    .on_click(move || set_edit.call(None)),
            ))
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(10.0),
    ))
    .on_tapped({
        let inside_tap = inside_tap.clone();
        move || inside_tap.set(true)
    })
    .max_width(460.0)
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

/// A saved host's plain dial: its fingerprint is already pinned, so this is the silent connect
/// a tile's click makes. `preset: None` honours the host's own binding.
///
/// Free rather than inline in the tile loop so the shell's start screen can build one before
/// any tile exists.
pub(crate) fn saved_target(k: &pf_client_core::trust::KnownHost) -> Target {
    Target {
        name: k.name.clone(),
        addr: k.addr.clone(),
        port: k.port,
        fp_hex: Some(k.fp_hex.clone()),
        pair_optional: false,
        mac: k.mac.clone(),
        mgmt_port: k.mgmt_port,
        preset: None,
        launch: None,
    }
}

pub(crate) fn hosts_page(props: &HostsProps, cx: &mut RenderCx) -> Element {
    let status = props.status.as_str();
    let (manual, set_manual) = cx.use_state(String::new());
    // The Add-host field's live value, read by Connect at click time. This page's `use_state` is
    // unreliable as the click's source of truth: while the modal is open the page usually has no
    // reason to re-render (you open it precisely because the host ISN'T being discovered, so no
    // discovery tick fires), and the top-down reconcile skips this unchanged-props subtree — so a
    // sync `set_manual` write never re-renders the Connect button to re-capture the address, and it
    // would connect to the empty mount-time value. Mirror every keystroke into this stable ref (the
    // pair-screen PIN pattern). `manual` still drives the text box's displayed value.
    let manual_live = cx.use_ref(String::new());
    let drafts = edit_drafts(cx, props.rename.as_ref());
    let hover = Hover {
        current: props.hover.clone(),
        set: props.set_hover.clone(),
    };
    let known = KnownHosts::load();

    // Responsive column count from the live window width (re-renders on resize): as many
    // TILE_MIN_WIDTH columns as fit the page's content width, at least one.
    let window = cx.use_inner_size();
    let content_w = (window.width - 64.0).clamp(TILE_MIN_WIDTH, 1120.0);
    let cols = (((content_w + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP)).floor() as usize).max(1);
    let mut body: Vec<Element> = Vec::new();

    body.push(header(&props.svc, &props.set_show_add));

    if !status.is_empty() {
        body.push(
            InfoBar::new("Couldn't connect")
                .message(status.to_string())
                .error()
                .is_closable(false)
                .into(),
        );
    }

    // Saved (trusted/paired) hosts — reachable even when mDNS isn't. A saved host that answers
    // the probe shows as Online (and any advert for it is deduped out of the discovery section).
    if !known.hosts.is_empty() {
        body.push(section("SAVED HOSTS"));
        let mut tiles: Vec<Element> = Vec::new();
        // One catalog read per render, shared by every tile's menu and chip.
        let presets: Vec<(String, String, Option<String>)> =
            pf_client_core::presets::PresetsFile::load()
                .presets
                .into_iter()
                .map(|p| (p.id, p.name, p.accent))
                .collect();
        // …and one settings read, for the checkmark on whichever tile the pointer names.
        let settings_default = Settings::load().default_host;
        for k in &known.hosts {
            tiles.extend(saved_tiles(props, k, &hover, &presets, &settings_default));
        }
        body.push(tile_grid(tiles, cols, TILE_GAP));
    }

    body.push(section("ON THIS NETWORK"));
    body.push(discovered_tiles(props, &known, &hover, cols));
    let forget_confirm = forget_dialog(props);
    let page = page_wide(body);
    let add_slot = add_host_slot(props, manual, set_manual, manual_live);
    // The host editor sheet, in its own stable slot (see the add modal's note).
    let edit_slot: Element = if let Some(who) = &props.rename {
        edit_editor(who, &who.name, drafts, props.set_rename.clone())
    } else {
        border(vstack(Vec::<Element>::new())).into()
    };
    grid(vec![page, add_slot, edit_slot, forget_confirm]).into()
}

/// The page header: the title block and the page actions — ONE labelled primary (Add host,
/// in accent), the rest icon-only with tooltips.
fn header(svc: &Svc, set_show_add: &AsyncSetState<bool>) -> Element {
    let (ctx, set_screen, set_status) = (&svc.ctx, &svc.set_screen, &svc.set_status);
    let icon_btn = |label: &str, mark: &str| {
        button("")
            .icon(lucide::icon(mark))
            .tooltip(label)
            .automation_name(label)
    };
    grid((
        vstack((
            text_block("Punktfunk").font_size(30.0).bold(),
            text_block("Stream from a host on your network.")
                .wrap()
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(2.0)
        .grid_column(0)
        .vertical_alignment(VerticalAlignment::Center),
        hstack({
            let mut actions: Vec<Element> = vec![button("Add host")
                .icon(lucide::icon("plus"))
                .accent()
                .on_click({
                    let sa = set_show_add.clone();
                    move || sa.call(true)
                })
                .into()];
            // Re-query mDNS. The browse runs for the app's lifetime, and `mdns-sd` backs its
            // re-query interval off to as much as an hour — so a host that appeared since
            // startup, or whose announcement was lost to multicast, may need an actual ask.
            actions.push(
                icon_btn("Scan the network for hosts again", "refresh-cw")
                    .on_click({
                        let (c, st) = (ctx.clone(), set_status.clone());
                        move || {
                            if let Some(r) = c.shared.rescan.lock().unwrap().as_ref() {
                                r.request();
                            }
                            st.call("Scanning the network\u{2026}".to_string());
                        }
                    })
                    .into(),
            );
            // The couch UI's front door, beside the other page actions. Absent on ARM64,
            // where the session binary ships without its Skia console.
            if CONSOLE_UI_AVAILABLE {
                actions.push(
                    icon_btn(
                        "Console UI \u{2014} the controller-driven couch interface",
                        "gamepad-2",
                    )
                    .on_click({
                        let (c, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
                        move || open_console(&c, &ss, &st)
                    })
                    .into(),
                );
            }
            actions.push(
                icon_btn("Keyboard shortcuts", "keyboard")
                    .on_click({
                        let ss = set_screen.clone();
                        move || ss.call(Screen::Help)
                    })
                    .into(),
            );
            actions.push(
                icon_btn("Settings", "settings")
                    .on_click({
                        let (c, ss) = (ctx.clone(), set_screen.clone());
                        move || {
                            // Re-base the settings snapshot on the file before the page
                            // renders — this process is not its only writer (see
                            // settings::refresh_snapshot).
                            super::settings::refresh_snapshot(&c);
                            ss.call(Screen::Settings)
                        }
                    })
                    .into(),
            );
            actions
        })
        .spacing(8.0)
        .grid_column(1)
        .vertical_alignment(VerticalAlignment::Center),
    ))
    .columns([GridLength::Star(1.0), GridLength::Auto])
    .margin(edges(0.0, 0.0, 0.0, 10.0))
    .into()
}

/// A saved host's primary tile, then its pinned host+preset tiles in pin order.
fn saved_tiles(
    props: &HostsProps,
    k: &pf_client_core::trust::KnownHost,
    hover: &Hover,
    presets: &[(String, String, Option<String>)],
    settings_default: &Option<String>,
) -> Vec<Element> {
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let hosts = props.hosts.as_slice();
    let mut tiles: Vec<Element> = Vec::new();
    let target = saved_target(k);
    // Online = the last probe sweep reached it, and nothing else. An advert is NOT
    // presence: it is a cache entry with a 75-minute TTL that a suspending host sends no
    // goodbye for, so counting it kept a sleeping machine's pip green — and every wake
    // gate below reads `!online`, which is how Wake-on-LAN stayed silent for exactly the
    // host it was meant to wake.
    let online = props.probed.get(&k.card_key()).copied().unwrap_or(false);
    // Everything the advert teaches: wake MAC(s), OS chain (so the mark survives going
    // offline), management port, and its address as a place the probe sweep asks —
    // the card moves there only once its pin answers. No disk write when unchanged.
    if let Some(a) = hosts
        .iter()
        .find(|h| pf_client_core::discovery::same_host(k, h))
    {
        crate::trust::learn_from_advert(
            &k.fp_hex,
            &k.addr,
            k.port,
            &a.addr,
            &a.mac,
            &a.os,
            a.mgmt_port,
        );
    }
    let can_wake = !online && !k.mac.is_empty();
    // What this host last said it lets this device do to it. Kept warm here — on the
    // list's own refresh, gated by the cache's TTL — so the menu is BUILT from a
    // settled answer: rows that appeared while a menu was open would land under a
    // cursor already moving, and two of these rows shut a machine down.
    if k.paired && online {
        pf_client_core::host_actions::refresh(
            &k.addr,
            target
                .mgmt_port
                .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
            &k.fp_hex,
        );
    }
    let menu = saved_menu(
        props,
        k,
        &target,
        online,
        can_wake,
        presets,
        settings_default,
    );
    let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let pinned_base = target.clone();
    tiles.push(host_tile(
        &k.fp_hex,
        hover,
        &k.name,
        &k.os,
        &format!("{}:{}", k.addr, k.port),
        status_row_with(
            Some(online),
            // Paired is the resting state — no chip; TOFU-only trust is worth one.
            (!k.paired).then_some(("Trusted", Pill::Info)),
            // The dot carries the preset's own colour where it has one —
            // that is what makes two bound hosts tell apart at a glance.
            k.preset_id
                .as_ref()
                .and_then(|id| presets.iter().find(|(pid, _, _)| pid == id))
                .map(|(_, name, accent)| (name.as_str(), accent.clone())),
        ),
        Some(menu),
        Some(Box::new(move || {
            // Saved host with a known MAC the probe did not reach: fire a wake packet
            // and DIAL IMMEDIATELY — looking unreachable ≠ unreachable (a routed/Tailscale
            // host answers a dial it never advertised for); only a failed dial falls into
            // the "Waking…" wait. A reachable host dials straight away.
            if can_wake {
                initiate_waking(&ctx2, target.clone(), &ss, &st);
            } else {
                initiate(&ctx2, target.clone(), &ss, &st);
            }
        })),
    ));

    // …then this host's pinned host+preset tiles, in pin order (design §5.2a). They read
    // the host's live record, and a pin whose preset is gone does not render. A pinned
    // tile is a shortcut: its menu starts it (the library opens with its preset), copies
    // its link or unpins it. Everything that configures the host stays on the primary.
    for id in &k.pinned_presets {
        let Some(preset) = presets.iter().find(|(pid, ..)| pid == id) else {
            continue;
        };
        tiles.push(pinned_tile(
            props,
            k,
            hover,
            online,
            can_wake,
            &pinned_base,
            preset,
        ));
    }
    tiles
}

/// A saved host's "…" menu: connect, the library surfaces, the host's own actions, links,
/// pins, the default-host pointer, edit and forget.
fn saved_menu(
    props: &HostsProps,
    k: &pf_client_core::trust::KnownHost,
    target: &Target,
    online: bool,
    can_wake: bool,
    presets: &[(String, String, Option<String>)],
    settings_default: &Option<String>,
) -> Button {
    let host_actions = pf_client_core::host_actions::cached(&k.fp_hex);
    let (svc, target) = (props.svc.clone(), target.clone());
    let click_actions = host_actions.clone();
    let (sf, sr) = (props.set_forget.clone(), props.set_rename.clone());
    let who = HostRef::of(k);
    let menu_presets = presets.to_vec();
    let pinned_now = k.pinned_presets.clone();
    let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
    let (link_host, link_preset) = (k.clone(), None::<String>);
    let shortcut_host = k.clone();
    let record_id = k.id.clone();
    let is_default = record_id.is_some() && *settings_default == record_id;
    button("")
        .icon(lucide::icon("ellipsis"))
        .subtle()
        .tooltip("More options")
        .automation_name("More options")
        .menu_flyout({
            // Short, in sections: the per-preset families nest in SUBMENUS — one "Connect
            // with" and one "Pin tiles" — so the top level stays a fixed handful whatever the
            // catalog grows to.
            let mut items = vec![menu_item(MENU_CONNECT)];
            // One-off connects: "Connect with" NEVER rebinds the host. Submenu
            // leaves report their own text, so the leaf names stay bare.
            if !presets.is_empty() {
                let mut leaves: Vec<MenuItemDef> = presets
                    .iter()
                    .map(|(_, name, _)| menu_item(name.clone()))
                    .collect();
                leaves.push(menu_item(SUB_WITH_DEFAULT));
                items.push(menu_sub_item(SUB_WITH, leaves));
            }

            items.push(menu_separator());
            // The library surfaces — mouse/KB page and the gamepad console UI — for
            // paired hosts only, because the mgmt API needs the paired identity.
            if k.paired {
                items.push(menu_item(MENU_LIBRARY));
            }
            items.push(menu_item(MENU_SPEED));
            // See [`MENU_SEND_LOGS`] for the gate.
            if k.paired && online {
                items.push(menu_item(MENU_SEND_LOGS));
            }
            // An explicit wake only when the host is offline and we have a MAC.
            if can_wake {
                items.push(menu_item(MENU_WAKE));
            }
            // …and the other half of that round trip, from the shared cache the
            // host list keeps warm. Empty unless the host answered AND this
            // device's access carries the grant, so no row here can be refused
            // for permission.
            for a in &host_actions {
                items.push(menu_item(host_action_label(a)));
            }

            items.push(menu_separator());
            items.push(menu_item(MENU_COPY_LINK));
            items.push(menu_item(MENU_SHORTCUT));
            // Pin/unpin a preset's one-click tile, beside the other tile-shaped
            // shortcuts. The verb prefixes stay on the leaves: "Connect with"'s
            // leaves are bare names, and the shared click callback only gets the
            // leaf text — the prefix is what keeps the two families apart.
            if !presets.is_empty() {
                let leaves: Vec<MenuItemDef> = presets
                    .iter()
                    .map(|(id, name, _)| {
                        let pinned = pinned_now.iter().any(|x| x == id);
                        menu_item(format!(
                            "{}{name}",
                            if pinned { MENU_UNPIN } else { MENU_PIN }
                        ))
                    })
                    .collect();
                items.push(menu_sub_item(SUB_PIN, leaves));
            }

            items.push(menu_separator());
            // Which host the app opens on. Needs a pairing to point at — the start
            // screen skips an unpaired host, so writing one would set a pointer
            // that never resolves. Unchecked is not "not the default": a lone
            // paired host is the default with nothing written.
            if k.paired {
                items.push(menu_item(if is_default {
                    MENU_DEFAULT_SET
                } else {
                    MENU_DEFAULT
                }));
            }
            items.push(menu_item(MENU_EDIT));
            items.push(menu_item(MENU_FORGET));
            items
        })
        .on_item_clicked(move |item: String| match item.as_str() {
            // The host's own actions are dynamic too, and matched by prefix ahead
            // of the fixed entries. The label is recovered back to an id through
            // the SAME list the rows were built from, so a menu whose rows outlived
            // their handlers can never run a different verb than the one clicked —
            // which matters here more than anywhere else in this menu.
            _ if item.starts_with(MENU_HOST_ACTION) => {
                if let Some(a) = click_actions.iter().find(|a| host_action_label(a) == item) {
                    run_host_action(&svc, &target, a);
                }
            }
            // The preset items are dynamic, so they are matched by prefix before
            // the fixed ones.
            _ if item.starts_with(MENU_PIN) || item.starts_with(MENU_UNPIN) => {
                let (on, name) = if let Some(n) = item.strip_prefix(MENU_PIN) {
                    (true, n)
                } else {
                    (false, item.trim_start_matches(MENU_UNPIN))
                };
                let Some((id, ..)) = menu_presets.iter().find(|(_, n, _)| n == name) else {
                    return;
                };
                tracing::info!(pin = %id, host = %who.name, on, "pin toggle");
                let mut known = KnownHosts::load();
                let target = who.index(&known);
                if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                    h.pinned_presets.retain(|x| x != id);
                    if on {
                        h.pinned_presets.push(id.clone());
                    }
                    if let Err(e) = known.save() {
                        tracing::warn!(error = %format!("{e:#}"), "saving a pin");
                    }
                }
                // The store changed behind the tiles and nothing the page reads
                // as state did — the bump is what makes the pinned tile appear
                // (or vanish) NOW, not on the next discovery tick.
                set_hosts_rev.call(hosts_rev + 1);
            }
            MENU_SHORTCUT => {
                let url = pf_client_core::deeplink::DeepLink::for_host(&shortcut_host, None, None)
                    .to_url();
                match crate::deeplink::write_shortcut(&shortcut_host.name, &url) {
                    Ok(p) => tracing::info!(path = %p.display(), "shortcut written"),
                    Err(e) => tracing::warn!(error = %e, "writing the shortcut"),
                }
            }
            MENU_COPY_LINK => {
                let url = pf_client_core::deeplink::DeepLink::for_host(
                    &link_host,
                    None,
                    link_preset.as_deref(),
                )
                .to_url();
                pf_client_core::clipboard::set_text(&url);
            }
            MENU_CONNECT => initiate(&svc.ctx, target.clone(), &svc.set_screen, &svc.set_status),
            MENU_LIBRARY => {
                *svc.ctx.shared.target.lock().unwrap() = target.clone();
                super::library::start_fetch(&svc.ctx, &svc.set_library);
                svc.set_screen.call(Screen::Library);
            }
            MENU_WAKE => crate::wol::wake(&target.mac, target.addr.parse().ok()),
            MENU_SEND_LOGS => send_logs(&svc, &target),
            MENU_SPEED => {
                *svc.ctx.shared.target.lock().unwrap() = target.clone();
                // New run: invalidate any still-in-flight probe, reset the screen.
                svc.ctx
                    .shared
                    .speed_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                svc.set_speed.call(SpeedState::Running);
                svc.set_screen.call(Screen::SpeedTest);
            }
            MENU_EDIT => sr.call(Some(who.clone())),
            MENU_FORGET => sf.call(Some(who.clone())),
            // Whole-file writer: rebase on the store before mutating, or a setting
            // another surface just wrote is reverted.
            MENU_DEFAULT | MENU_DEFAULT_SET => {
                let mut settings = Settings::load();
                let on = settings.default_host != record_id;
                settings.default_host = on.then(|| record_id.clone()).flatten();
                settings.save();
                svc.set_status.call(String::new());
                set_hosts_rev.call(hosts_rev + 1);
            }
            // "Connect with"'s submenu leaves: a bare preset name, or
            // SUB_WITH_DEFAULT. `Some("")` — not `None` — so Default settings
            // really does override a bound host for this one connect.
            other => {
                let preset_id = if other == SUB_WITH_DEFAULT {
                    Some(String::new())
                } else {
                    menu_presets
                        .iter()
                        .find(|(_, n, _)| n == other)
                        .map(|(id, _, _)| id.clone())
                };
                if let Some(id) = preset_id {
                    let mut target = target.clone();
                    target.preset = Some(id);
                    initiate(&svc.ctx, target, &svc.set_screen, &svc.set_status)
                }
            }
        })
}

/// Runs one of the host's own actions on a worker thread, the outcome on the status line.
fn run_host_action(svc: &Svc, target: &Target, a: &pf_client_core::host_actions::ActionInfo) {
    let set_status = svc.set_status.clone();
    let (action_id, label) = (a.id.clone(), a.label().to_string());
    if !a.available {
        // The host already said it cannot do this right now.
        set_status.call(
            a.unavailable_reason
                .clone()
                .unwrap_or_else(|| format!("{label} isn't available")),
        );
        return;
    }
    let identity = svc.ctx.identity.clone();
    let target = target.clone();
    set_status.call(format!("{label} — asking {}…", target.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-hostaction".into())
        .spawn(move || {
            set_status.call(pf_client_core::host_actions::run(
                &target.name,
                &target.addr,
                target
                    .mgmt_port
                    .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                &identity,
                target.fp_hex.as_deref().unwrap_or_default(),
                &action_id,
                &label,
            ));
        });
}

/// Uploads this device's log ring to the host. Blocking network (the library agent's 5 s
/// connect / 10 s global budgets), so on a worker thread, the outcome on the status line.
fn send_logs(svc: &Svc, target: &Target) {
    let identity = svc.ctx.identity.clone();
    let target = target.clone();
    let set_status = svc.set_status.clone();
    set_status.call(format!("Sending logs to {}…", target.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-sendlogs".into())
        .spawn(move || {
            set_status.call(pf_client_core::logring::send_bundle(
                "punktfunk-client",
                &target.name,
                &target.addr,
                target
                    .mgmt_port
                    .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                &identity,
                target.fp_hex.as_deref().unwrap_or_default(),
            ));
        });
}

/// One pinned host+preset tile (design §5.2a). A pinned tile is a shortcut: its menu starts
/// it (the library opens with its preset), copies its link or unpins it. Everything that
/// configures the host stays on the primary tile.
fn pinned_tile(
    props: &HostsProps,
    k: &pf_client_core::trust::KnownHost,
    hover: &Hover,
    online: bool,
    can_wake: bool,
    base: &Target,
    (id, name, accent): &(String, String, Option<String>),
) -> Element {
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let (ctx3, ss3, st3) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let mut pinned_target = base.clone();
    pinned_target.preset = Some(id.clone());
    let pinned_menu = {
        let (svc, target) = (props.svc.clone(), pinned_target.clone());
        let (unpin_who, pin_id) = (HostRef::of(k), id.clone());
        let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
        let link_host = k.clone();
        let link_preset = id.clone();
        let unpin_label = format!("{MENU_UNPIN}{name}");
        let unpin_item = unpin_label.clone();
        button("")
            .icon(lucide::icon("ellipsis"))
            .subtle()
            .tooltip("More options")
            .automation_name("More options")
            .menu_flyout({
                let mut items = Vec::new();
                // Same gate as the primary tile's: the mgmt API needs the paired
                // identity, so an unpaired host has nothing to show.
                if k.paired {
                    items.push(menu_item(MENU_LIBRARY));
                }
                items.push(menu_item(MENU_COPY_LINK));
                items.push(menu_separator());
                items.push(menu_item(unpin_label));
                items
            })
            .on_item_clicked(move |item: String| match item.as_str() {
                MENU_LIBRARY => {
                    // The shared target IS what the library page launches through, so
                    // parking THIS tile's target here is what makes its grid launch
                    // with the pinned preset.
                    *svc.ctx.shared.target.lock().unwrap() = target.clone();
                    super::library::start_fetch(&svc.ctx, &svc.set_library);
                    svc.set_screen.call(Screen::Library);
                }
                MENU_COPY_LINK => {
                    let url = pf_client_core::deeplink::DeepLink::for_host(
                        &link_host,
                        None,
                        Some(link_preset.as_str()),
                    )
                    .to_url();
                    pf_client_core::clipboard::set_text(&url);
                }
                other if other == unpin_item => {
                    tracing::info!(pin = %pin_id, host = %unpin_who.name, on = false, "pin toggle");
                    let mut known = KnownHosts::load();
                    let target = unpin_who.index(&known);
                    if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                        h.pinned_presets.retain(|x| x != &pin_id);
                        if let Err(e) = known.save() {
                            tracing::warn!(
                                error = %format!("{e:#}"), "saving a pin"
                            );
                        }
                    }
                    // Same reason as the primary tile's toggle: nothing the page reads
                    // as state changed, so the bump is what makes this tile vanish NOW.
                    set_hosts_rev.call(hosts_rev + 1);
                }
                _ => {}
            })
    };
    host_tile(
        // Its own hover key: two tiles for one host must not light up together.
        &format!("{}#{id}", k.fp_hex),
        hover,
        &k.name,
        &k.os,
        &format!("{}:{}", k.addr, k.port),
        status_row_with(
            Some(online),
            (!k.paired).then_some(("Trusted", Pill::Info)),
            Some((name.as_str(), accent.clone())),
        ),
        Some(pinned_menu),
        Some(Box::new(move || {
            if can_wake {
                initiate_waking(&ctx3, pinned_target.clone(), &ss3, &st3);
            } else {
                initiate(&ctx3, pinned_target.clone(), &ss3, &st3);
            }
        })),
    )
}

/// Discovered hosts not already saved, or a searching card while there are none.
fn discovered_tiles(props: &HostsProps, known: &KnownHosts, hover: &Hover, cols: usize) -> Element {
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let hosts = props.hosts.as_slice();
    let discovered: Vec<&DiscoveredHost> = hosts
        .iter()
        .filter(|h| {
            !known
                .hosts
                .iter()
                .any(|k| pf_client_core::discovery::same_host(k, h))
        })
        .collect();
    if discovered.is_empty() {
        return card(
            hstack((
                ProgressRing::indeterminate().width(18.0).height(18.0),
                text_block("Searching the LAN\u{2026}").foreground(ThemeRef::SecondaryText),
            ))
            .spacing(12.0),
        )
        .into();
    }
    let mut tiles: Vec<Element> = Vec::new();
    for h in discovered {
        let target = Target {
            name: h.name.clone(),
            addr: h.addr.clone(),
            port: h.port,
            fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
            pair_optional: h.pair == "optional",
            mac: h.mac.clone(),
            mgmt_port: h.mgmt_port,
            preset: None,
            launch: None,
        };
        let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
        let (badge, kind) = if h.pair == "required" {
            ("PIN", Pill::Info)
        } else {
            ("Open", Pill::Neutral)
        };
        tiles.push(host_tile(
            &format!("{}:{}", h.addr, h.port),
            hover,
            &h.name,
            &h.os,
            &format!("{}:{}", h.addr, h.port),
            status_row(None, Some((badge, kind))),
            None,
            Some(Box::new(move || initiate(&ctx2, target.clone(), &ss, &st))),
        ));
    }
    tile_grid(tiles, cols, TILE_GAP)
}

/// Forget confirmation: ALWAYS MOUNTED (`is_open` arms it) in a stable trailing layer, not
/// in `body`, whose children shift with discovery. Unmounting or re-pairing a ContentDialog
/// trips the reactor's phantom-child bookkeeping into an E_BOUNDS panic (as the delete-preset
/// dialog in settings.rs shows). Confirmed first: undoing a forget needs a fresh pairing.
fn forget_dialog(props: &HostsProps) -> Element {
    let (sf, st) = (props.set_forget.clone(), props.svc.set_status.clone());
    let pending = props.forget.clone();
    let content = pending
        .as_ref()
        .map(|who| {
            let name = &who.name;
            format!(
                "Forget \u{201C}{name}\u{201D}? You'll need to pair (or trust) it again to \
                 reconnect."
            )
        })
        .unwrap_or_default();
    ContentDialog::new("Remove saved host?")
        .content(content)
        .primary_button_text("Remove")
        .close_button_text("Cancel")
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            if r == ContentDialogResult::Primary
                && let Some(who) = &pending
            {
                let mut known = KnownHosts::load();
                if let Some(i) = who.index(&known)
                    && let Err(e) = pf_client_core::orchestrate::forget_host(&mut known, i)
                {
                    st.call(format!("Couldn't save — {e:#}"));
                }
            }
            sf.call(None); // re-renders the page; the row is gone on the next load
        })
        .into()
}

/// "Add host" modal: a scrim + centered card. It's an in-tree overlay, not a WinUI
/// ContentDialog, because ContentDialog is text-only in windows-reactor (no room for a text
/// field). The scrim border fills the cell and is hit-testable, so it blocks the page behind;
/// it closes only via Cancel/Connect (a scrim tap would bubble `Tapped` up from the card too).
fn add_host_slot(
    props: &HostsProps,
    manual: String,
    set_manual: SetState<String>,
    manual_live: HookRef<String>,
) -> Element {
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let set_show_add = &props.set_show_add;
    let connect_manual = {
        let (ctx2, ss, st, live, sa) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            manual_live.clone(),
            set_show_add.clone(),
        );
        move || {
            let text = live.borrow();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            // Shared parser: a pasted `::1` is one address, not host `:` port `1`, and this
            // value is persisted and compared against saved records.
            let (addr, port) = pf_client_core::deeplink::parse_addr_port(text)
                .unwrap_or_else(|| (text.to_string(), pf_client_core::deeplink::DEFAULT_PORT));
            sa.call(false);
            initiate(
                &ctx2,
                Target {
                    name: addr.clone(),
                    addr,
                    port,
                    fp_hex: None,
                    pair_optional: false,
                    mac: Vec::new(),
                    // Added by hand, so nothing has told us where its mgmt API is: fall back to
                    // 47990 (exactly today's behaviour) until an advert teaches us otherwise.
                    // A host that moved its mgmt port AND is never visible on mDNS still needs the
                    // host to announce the port in-band — see the note in `Target::mgmt_port`.
                    mgmt_port: None,
                    preset: None,
                    launch: None,
                },
                &ss,
                &st,
            );
        }
    };
    let modal = dialog_surface(
        vstack((
            text_block("Add a host").font_size(20.0).bold(),
            text_block(
                "Enter the host's IP address or name. Append :port only for a non-standard port \
                 (the default is 9777).",
            )
            .font_size(13.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText),
            text_box(manual)
                .header("Address")
                .placeholder_text("192.168.1.20  or  my-pc.local")
                .on_text_changed({
                    let live = manual_live.clone();
                    move |s: String| {
                        live.set(s.clone());
                        set_manual.call(s);
                    }
                })
                .margin(edges(0.0, 6.0, 0.0, 0.0)),
            hstack((
                button("Connect")
                    .accent()
                    .icon(lucide::icon("arrow-right"))
                    .on_click(connect_manual),
                button("Cancel").on_click({
                    let sa = set_show_add.clone();
                    move || sa.call(false)
                }),
            ))
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right)
            .margin(edges(0.0, 6.0, 0.0, 0.0)),
        ))
        .spacing(12.0),
    )
    .max_width(460.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    // Entrance: fade + slide up, driven by the root tween (`add_anim` 0 → 1). The card starts
    // a bit low and rises to centre — for a centred element, extra top margin shifts it down by
    // half the difference, so the offset is doubled.
    .opacity(props.add_anim)
    .margin(edges(
        24.0,
        24.0 + (1.0 - props.add_anim) * 56.0,
        24.0,
        24.0,
    ));

    // The scrim fades in with the same tween. Its layer slot is STABLE (a same-kind,
    // background-less Border when closed — invisible and not hit-testable) so the layer
    // list never changes shape around the always-mounted dialog after it. A tap on the
    // scrim, or Escape, cancels — with the same bubble-swallow flag the sheets use.
    if props.show_add {
        let inside_tap = std::rc::Rc::new(std::cell::Cell::new(false));
        let cancel = {
            let sa = set_show_add.clone();
            move || sa.call(false)
        };
        let scrim_cancel = cancel.clone();
        Element::from(
            border(Element::from(modal).on_tapped({
                let inside_tap = inside_tap.clone();
                move || inside_tap.set(true)
            }))
            .background(Color {
                a: (140.0 * props.add_anim) as u8,
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
    } else {
        border(vstack(Vec::<Element>::new())).into()
    }
}

/// The host editor's live drafts, read at Save time (see `edit_editor`).
struct EditDrafts {
    name: HookRef<String>,
    addr: HookRef<String>,
    port: HookRef<String>,
    mac: HookRef<String>,
    clip: HookRef<bool>,
}

/// The editor's drafts, re-seeded from the STORED host whenever the edit target changes (open,
/// cancel, or switching to another host). The seed key is one string per record — the minted
/// id, or addr:port for a record older than ids — so it re-runs only for a DIFFERENT host.
fn edit_drafts(cx: &mut RenderCx, rename: Option<&HostRef>) -> EditDrafts {
    let drafts = EditDrafts {
        name: cx.use_ref(String::new()),
        addr: cx.use_ref(String::new()),
        port: cx.use_ref(String::new()),
        mac: cx.use_ref(String::new()),
        clip: cx.use_ref(false),
    };
    let edit_seed = cx.use_ref(Option::<String>::None);
    let active = rename.map(|who| {
        who.id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", who.addr, who.port))
    });
    if *edit_seed.borrow() != active {
        let stored = rename.and_then(|who| {
            let known = KnownHosts::load();
            who.index(&known).map(|i| known.hosts[i].clone())
        });
        drafts
            .name
            .set(stored.as_ref().map(|h| h.name.clone()).unwrap_or_default());
        drafts
            .addr
            .set(stored.as_ref().map(|h| h.addr.clone()).unwrap_or_default());
        drafts.port.set(
            stored
                .as_ref()
                .map(|h| h.port.to_string())
                .unwrap_or_default(),
        );
        drafts.mac.set(
            stored
                .as_ref()
                .map(|h| h.mac.join(", "))
                .unwrap_or_default(),
        );
        drafts
            .clip
            .set(stored.as_ref().is_some_and(|h| h.clipboard_sync));
        edit_seed.set(active);
    }
    drafts
}
