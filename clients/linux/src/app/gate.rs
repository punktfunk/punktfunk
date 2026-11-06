//! The trust dialogs in front of a connect: TOFU, the SPAKE2 PIN ceremony, and
//! delegated (request-access) approval. The trust GATE itself is `orchestrate::trust_route`;
//! `AppModel::connect` opens these surfaces for it, each resolving into typed [`AppMsg`]s.

use crate::app::spawn::SpawnOpts;
use crate::app::{AppModel, AppMsg};
use crate::hosts::ConnectRequest;
use crate::trust;
use adw::prelude::*;
use gtk::glib;
use pf_client_core::orchestrate::{wake_parked_line, WakeOutcome, WakeWait};
use pf_l10n::{client, common, desktop};
use relm4::prelude::*;

/// The waiting dialog plus its Cancel handler. Both halves are needed to dismiss it: a bare
/// `close()` emits the close response, so the dialog must be disconnected first or the code
/// path that closes it fires the user's own Cancel.
pub type WaitingSlot =
    std::rc::Rc<std::cell::RefCell<Option<(adw::AlertDialog, glib::SignalHandlerId)>>>;

/// Wake-and-wait: the FALLBACK after a failed dial to a non-advertising saved host with a
/// known MAC (`AppMsg::WakeConnect` dials first — mDNS absence ≠ unreachable). A magic packet,
/// then mDNS polled until the host advertises — re-sending every few seconds up to a timeout —
/// and back into the trust gate, dialling the address it came back on. The "Waking…" dialog
/// offers Cancel; on a timeout it parks with Try Again, a fresh wake of the same request.
///
/// The cadence is [`WakeWait`] and the advert match `AdvertWatch`, both shared with the WinUI
/// shell (design/client-architecture-split.md §3).
pub fn wake_and_connect(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
) {
    let cancel = std::rc::Rc::new(std::cell::Cell::new(false));
    let waiting = adw::AlertDialog::new(
        Some(&client::dial_waking(&req.host.name)),
        Some(&desktop::desktop_dial_wake_sent(&req.host.name)),
    );
    waiting.add_responses(&[("cancel", common::common_cancel())]);
    waiting.set_close_response("cancel");
    {
        let cancel = cancel.clone();
        waiting.connect_response(Some("cancel"), move |_, _| cancel.set(true));
    }
    waiting.present(Some(window));

    let (window, sender) = (window.clone(), sender.clone());
    glib::spawn_future_local(async move {
        use std::time::Duration;
        let mut adverts = crate::discovery::AdvertWatch::start();
        let mut wait = WakeWait::new();
        loop {
            if cancel.get() {
                waiting.close();
                return;
            }
            // A match carries the address the host came back on.
            let seen = adverts.poll(req.host.fp_hex.as_deref(), &req.host.addr, req.host.port);
            let tick = wait.tick(seen.is_some());
            if tick.send_packet {
                crate::wol::wake(&req.host.mac, req.host.addr.parse().ok());
            }
            match tick.outcome {
                Some(WakeOutcome::Online) => {
                    waiting.close();
                    let mut req = req.clone();
                    // Dial a new DHCP lease. The saved card moves only when the probe
                    // sweep hears its pin there — an advert's address can be another machine's.
                    if let Some((addr, port)) =
                        seen.filter(|(a, p)| *a != req.host.addr || *p != req.host.port)
                    {
                        req.host.addr = addr;
                        req.host.port = port;
                    }
                    sender.input(AppMsg::Connect(req));
                    return;
                }
                Some(WakeOutcome::TimedOut) => {
                    // Park in the same dialog. Cancel stays its close response.
                    waiting.set_heading(Some(&wake_parked_line(&req.host.name)));
                    waiting.set_body(client::dial_wake_parked_hint());
                    waiting.add_response("retry", common::common_try_again());
                    waiting.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
                    waiting.set_default_response(Some("retry"));
                    waiting.connect_response(Some("retry"), move |_, _| {
                        wake_and_connect(&window, &sender, req.clone())
                    });
                    return;
                }
                None => {}
            }
            glib::timeout_future(Duration::from_secs(1)).await;
        }
    });
}

/// The certificate fingerprint as grouped monospaced hex — 4-char groups over 2 lines,
/// far easier to compare against the host's log than one 64-char run.
fn grouped_fingerprint(fp: &str) -> String {
    let groups: Vec<&str> = fp
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    groups
        .chunks(8)
        .map(|line| line.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// First contact with a discovered host that opted into TOFU: show the advertised
/// fingerprint and let the user trust it, run the PIN ceremony instead, or walk away.
/// Trusting starts a session pinned to the advertised fp; it persists once the child
/// proves the host holds that identity (`AppMsg::SessionReady` with `tofu`).
pub fn tofu_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
) {
    let fp = req.host.fp_hex.clone().unwrap_or_default();
    let dialog = adw::AlertDialog::new(
        Some(client::trust_title()),
        Some(&format!(
            "{}\n\n{}",
            client::trust_first_connection(format!("{}:{}", req.host.addr, req.host.port)),
            client::trust_explain()
        )),
    );
    let fp_label = gtk::Label::new(Some(&grouped_fingerprint(&fp)));
    fp_label.add_css_class("monospace");
    fp_label.set_selectable(true);
    fp_label.set_justify(gtk::Justification::Center);
    fp_label.set_halign(gtk::Align::Center);
    dialog.set_extra_child(Some(&fp_label));
    dialog.add_responses(&[
        ("cancel", common::common_cancel()),
        ("pair", client::pair_use_pin()),
        ("trust", client::trust_accept()),
    ]);
    dialog.set_response_appearance("trust", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("trust"));
    dialog.set_close_response("cancel");
    let sender = sender.clone();
    dialog.connect_response(None, move |_, response| match response {
        "trust" => sender.input(AppMsg::StartSession {
            req: req.clone(),
            fp_hex: fp.clone(),
            tofu: true,
            opts: SpawnOpts::default(),
        }),
        "pair" => sender.input(AppMsg::Pair(req.clone())),
        _ => {}
    });
    dialog.present(Some(window));
}

/// The SPAKE2 ceremony: the host is armed and displays a 4-digit PIN; proving knowledge
/// of it pins the host's certificate (and registers ours) with no offline-guessable
/// transcript. Success persists the host as paired, and streams only when `then_connect`
/// (a link that asked to connect), as the profile the link's `as=` named.
pub fn pin_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    identity: (String, String),
    req: ConnectRequest,
    then_connect: bool,
) {
    let entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .placeholder_text(client::pair_pin_placeholder())
        .activates_default(true)
        .build();
    // The label the HOST stores this client under — prefilled with the hostname.
    let name_entry = gtk::Entry::builder()
        .text(glib::host_name().as_str())
        .activates_default(true)
        .build();
    let name_caption = gtk::Label::new(Some(client::pair_device_name()));
    name_caption.add_css_class("caption");
    name_caption.add_css_class("dim-label");
    name_caption.set_halign(gtk::Align::Start);
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
    fields.append(&name_caption);
    fields.append(&name_entry);
    let pin_caption = gtk::Label::new(Some(client::pair_pin()));
    pin_caption.add_css_class("caption");
    pin_caption.add_css_class("dim-label");
    pin_caption.set_halign(gtk::Align::Start);
    fields.append(&pin_caption);
    fields.append(&entry);
    let dialog = adw::AlertDialog::new(
        Some(&client::pair_title(&req.host.name)),
        Some(client::pair_intro_pin_only()),
    );
    dialog.set_extra_child(Some(&fields));
    dialog.add_responses(&[
        ("cancel", common::common_cancel()),
        ("pair", client::pair_action()),
    ]);
    dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("pair"));
    dialog.set_close_response("cancel");
    let sender = sender.clone();
    dialog.connect_response(Some("pair"), move |_, _| {
        let pin = entry.text().to_string();
        let req = req.clone();
        let identity = identity.clone();
        let sender = sender.clone();
        let (tx, rx) = async_channel::bounded::<Result<[u8; 32], String>>(1);
        let device = name_entry.text().trim().to_string();
        let name = if device.is_empty() {
            glib::host_name().to_string()
        } else {
            device
        };
        let (host, port) = (req.host.addr.clone(), req.host.port);
        std::thread::spawn(move || {
            // Cause-specific wording (wrong PIN vs not-armed vs unreachable vs a typed host
            // rejection) — never blame the PIN for a dead network path.
            let result = trust::pair_with_host(&host, port, &identity, &pin, &name)
                .map_err(|e| trust::pair_error_message(&e));
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(fp)) => {
                    let paired =
                        trust::save_paired(&req.host.name, &req.host.addr, req.host.port, fp, &[]);
                    sender.input(AppMsg::Toast(match &paired.saved {
                        Ok(()) if then_connect => desktop::desktop_pair_paired_connecting().into(),
                        Ok(()) => client::pair_paired(&req.host.name),
                        Err(e) => client::pair_saved_failed(format!("{e:#}")),
                    }));
                    if then_connect {
                        // The link's `as=`, as a direct connect dials it.
                        let profile = req.profile.clone().map(Some);
                        sender.input(AppMsg::StartSession {
                            req,
                            fp_hex: paired.fp_hex,
                            tofu: false,
                            opts: SpawnOpts {
                                profile,
                                ..SpawnOpts::default()
                            },
                        });
                    }
                }
                Ok(Err(msg)) => sender.input(AppMsg::Toast(msg)),
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(window));
}

/// A fresh host that requires pairing: "Request access" (wait for the operator to click
/// Approve in the host's console — delegated approval) or the PIN ceremony.
pub fn approval_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    waiting_slot: WaitingSlot,
    identity: (String, String),
    req: ConnectRequest,
) {
    let dialog = adw::AlertDialog::new(
        Some(client::pair_required_title()),
        Some(&format!(
            "{}\n\n{}",
            client::pair_required_body(&req.host.name),
            client::pair_intro()
        )),
    );
    dialog.add_responses(&[
        ("cancel", common::common_cancel()),
        ("pin", client::pair_use_pin()),
        ("request", client::pair_request_access()),
    ]);
    dialog.set_response_appearance("request", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("request"));
    dialog.set_close_response("cancel");
    let parent = window.clone();
    let window = window.clone();
    let sender = sender.clone();
    dialog.connect_response(None, move |_, response| match response {
        "request" => request_access(
            &window,
            &sender,
            waiting_slot.clone(),
            identity.clone(),
            req.clone(),
        ),
        "pin" => sender.input(AppMsg::Pair(req.clone())),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The no-PIN "request access" flow: ask the host, which parks the request until the
/// operator approves it in the console, under a cancelable "waiting" dialog. Approval
/// saves the host as paired and never starts a stream.
///
/// The "Waiting for Approval" dialog over `window`, filed in `waiting_slot`; its Cancel runs
/// `on_cancel`. `close()` emits the close response, so whoever closes it disconnects the
/// handler first ([`super::connect::Flight::close_waiting`]).
pub(super) fn present_waiting(
    window: &adw::ApplicationWindow,
    waiting_slot: &WaitingSlot,
    host_name: &str,
    on_cancel: impl Fn() + 'static,
) -> adw::AlertDialog {
    let body = client::dial_approve_no_pin(host_name);
    present_wait(
        window,
        waiting_slot,
        client::pair_waiting_for_approval(),
        Some(&body),
        on_cancel,
    )
}

/// A wait dialog with `heading`, `body` and Cancel, filed in `waiting_slot` as
/// [`present_waiting`] files its own.
pub(super) fn present_wait(
    window: &adw::ApplicationWindow,
    waiting_slot: &WaitingSlot,
    heading: &str,
    body: Option<&str>,
    on_cancel: impl Fn() + 'static,
) -> adw::AlertDialog {
    let waiting = adw::AlertDialog::new(Some(heading), body);
    waiting.add_responses(&[("cancel", common::common_cancel())]);
    waiting.set_close_response("cancel");
    let handler = waiting.connect_response(Some("cancel"), move |_, _| on_cancel());
    waiting.present(Some(window));
    *waiting_slot.borrow_mut() = Some((waiting.clone(), handler));
    waiting
}

/// The pin is the advertised fingerprint (defence against an impostor while we wait), so a
/// manually-typed host, which has none, pairs with a PIN instead.
fn request_access(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    waiting_slot: WaitingSlot,
    identity: (String, String),
    req: ConnectRequest,
) {
    let Some(pin) = req.host.pin().and_then(trust::parse_hex32) else {
        sender.input(AppMsg::Toast(desktop::desktop_pair_no_advert().into()));
        sender.input(AppMsg::Pair(req));
        return;
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiting = {
        let cancel = cancel.clone();
        present_waiting(window, &waiting_slot, &req.host.name, move || {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let (tx, rx) = async_channel::bounded(1);
    let (host, port) = (req.host.addr.clone(), req.host.port);
    let asked = cancel.clone();
    std::thread::spawn(move || {
        let r = trust::request_access_to_host(
            &host,
            port,
            &identity,
            Some(pin),
            &trust::device_name(),
            Some(asked),
        );
        let _ = tx.send_blocking(r);
    });
    let sender = sender.clone();
    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else {
            return;
        };
        // A later request may own the slot by now; only close this one's dialog.
        let mine = waiting_slot.borrow_mut().take_if(|(w, _)| *w == waiting);
        if let Some((w, handler)) = mine {
            w.disconnect(handler);
            w.close();
        }
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // `persist_on_ready` keeps what the advert taught, the wake MACs among it.
        let msg = match result {
            Ok(fp) => match pf_client_core::orchestrate::persist_on_ready(
                &req.host,
                &trust::hex(&fp),
                true,
            ) {
                Ok(()) => client::pair_paired(&req.host.name),
                Err(e) => client::pair_saved_failed(format!("{e:#}")),
            },
            Err(e) => trust::access_error_message(&e),
        };
        sender.input(AppMsg::Toast(msg));
    });
}
