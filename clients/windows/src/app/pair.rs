//! The SPAKE2 PIN pairing screen: the host is armed and displays a 4-digit PIN; proving
//! knowledge of it pins the host's certificate (and registers ours) with no offline-guessable
//! transcript. Also offers the no-PIN "request access" (delegated-approval) alternative.

use super::connect::{connect, request_access};
use super::lucide;
use super::style::*;
use super::{Screen, Svc};
use crate::trust;
use std::sync::atomic::Ordering;
use windows_reactor::*;

pub(crate) fn pair_page(props: &Svc, cx: &mut RenderCx) -> Element {
    let ctx = &props.ctx;
    let set_screen = &props.set_screen;
    let set_status = &props.set_status;
    let (code, set_code) = cx.use_state(String::new());
    // The PIN's live value, read directly by the click handler. This page's props (`Svc`) never
    // change, and root wraps every screen in an animated `border` that compares equal once the
    // entrance tween settles — so the top-down reconcile `can_skip_update`s this subtree and never
    // re-renders the pair component off its *local* `use_state`. A button rebuilt only at mount
    // would forever capture the empty mount-time PIN (pairing then fails as a "wrong PIN"). Mirror
    // every keystroke into this stable ref instead, so the click reads exactly what was typed.
    let live_pin = cx.use_ref(String::new());
    let target = ctx.shared.target.lock().unwrap().clone();

    let pair_btn = {
        let (ctx2, ss, st, live, target2) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            live_pin.clone(),
            target.clone(),
        );
        button("Pair & Connect")
            .accent()
            .icon(lucide::icon("check"))
            .on_click(move || {
                let pin = live.borrow().trim().to_string();
                let (ctx3, ss, st, target3) =
                    (ctx2.clone(), ss.clone(), st.clone(), target2.clone());
                let generation = ctx3.shared.pair_gen.fetch_add(1, Ordering::SeqCst) + 1;
                std::thread::spawn(move || {
                    let current = || ctx3.shared.pair_gen.load(Ordering::SeqCst) == generation;
                    match trust::pair_with_host(
                        &target3.host.addr,
                        target3.host.port,
                        &ctx3.identity,
                        &pin,
                        &trust::device_name(),
                    ) {
                        Ok(fp) => {
                            let saved = trust::persist_host(
                                &target3.host.name,
                                &target3.host.addr,
                                target3.host.port,
                                &trust::hex(&fp),
                                true,
                                &target3.host.mac,
                            );
                            if !current() {
                                return;
                            }
                            connect(&ctx3, &target3, Some(fp), &ss, &st);
                            // After `connect`, which clears the status line. The stream runs
                            // on the pin in memory; the next launch asks for a PIN again.
                            if let Err(e) = saved {
                                st.call(format!("Paired, but couldn't save — {e:#}"));
                            }
                        }
                        Err(e) => {
                            if !current() {
                                return;
                            }
                            // Cause-specific: wrong PIN vs pairing-not-armed vs unreachable —
                            // never blame the PIN for a dead network path (shared wording).
                            st.call(trust::pair_error_message(&e));
                            ss.call(Screen::Hosts);
                        }
                    }
                });
            })
    };
    let cancel_btn = {
        let (ss, ctx2) = (set_screen.clone(), ctx.clone());
        button("Cancel").icon(lucide::icon("x")).on_click(move || {
            ctx2.shared.pair_gen.fetch_add(1, Ordering::SeqCst);
            ss.call(Screen::Hosts);
        })
    };
    // The no-PIN alternative offered alongside the PIN ceremony: open an identified connect that
    // the host parks until the operator approves this device in its console (delegated approval).
    let request_btn = {
        let (svc, target2) = (props.clone(), target.clone());
        button("Request access without a PIN")
            .icon(lucide::icon("send"))
            .on_click(move || request_access(&svc, &target2))
            .horizontal_alignment(HorizontalAlignment::Stretch)
    };

    let content = card(vstack((
        grid((
            // `Target` holds no OS chain, so the pairing card keeps the monogram.
            avatar(&target.host.name, "")
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack((
                text_block(format!("Pair with {}", target.host.name))
                    .font_size(20.0)
                    .semibold(),
                text_block(format!("{}:{}", target.host.addr, target.host.port))
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .margin(edges(12.0, 0.0, 0.0, 0.0)),
        ))
        .columns([GridLength::Auto, GridLength::Star(1.0)]),
        InfoBar::new("Arm pairing on the host")
            .message(
                "On the host's console or web console, start pairing — it shows a 4-digit PIN. \
                 Enter it below within 90 seconds.",
            )
            .informational()
            .is_closable(false),
        text_box(code)
            .placeholder_text("PIN")
            .font_size(28.0)
            .on_text_changed({
                let live = live_pin.clone();
                move |s: String| {
                    // Record the live value for the click handler (the source of truth for the
                    // PIN), and mirror it into `code` so the field stays correct if anything ever
                    // does re-render this page (theme/DPI change).
                    live.set(s.clone());
                    set_code.call(s);
                }
            }),
        hstack((pair_btn, cancel_btn)).spacing(8.0),
        text_block(
            "Don\u{2019}t have a PIN? Request access instead and approve this device on the host \
             (its console or web UI) \u{2014} no PIN needed.",
        )
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText),
        request_btn,
    ))
    .spacing(16.0))
    .max_width(480.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .margin(edges(0.0, 60.0, 0.0, 0.0));

    page(vec![content.into()])
}
