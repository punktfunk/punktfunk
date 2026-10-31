//! The SPAKE2 PIN pairing screen: the host is armed and displays a 4-digit PIN; proving
//! knowledge of it pins the host's certificate (and registers ours) with no offline-guessable
//! transcript. Also offers the no-PIN "request access" (delegated-approval) alternative.

use super::connect::{connect, request_access};
use super::style::*;
use super::{Screen, Svc};
use crate::trust;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use windows_reactor::*;

/// The pairing screen. Its state is the PIN as typed, which Pair reads at click time; the box
/// is never re-declared, so redraws leave the caret alone.
pub(crate) struct PairPage {
    pin: Rc<RefCell<String>>,
}

impl Component for PairPage {
    type Input = Svc;
    type Message = ();

    fn create(_props: &Svc, _cx: &ComponentContext<Self>) -> Self {
        PairPage { pin: Rc::default() }
    }

    fn view(&self, props: &Svc, _cx: &mut ViewContext<'_, Self>) -> View {
        pair_page(props, &self.pin)
    }
}

fn pair_page(props: &Svc, live_pin: &Rc<RefCell<String>>) -> View {
    let (ctx, pal) = (&props.ctx, &props.pal);
    let set_screen = &props.set_screen;
    let set_status = &props.set_status;
    let target = ctx.shared.target.lock().unwrap().clone();

    let pair_btn = {
        let (ctx2, ss, st, svc, live, target2) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            props.clone(),
            live_pin.clone(),
            target.clone(),
        );
        icon_button("check", "Pair")
            .style(ButtonStyle::Accent)
            .on_click(move || {
                let pin = live.borrow().trim().to_string();
                let (ctx3, ss, st, svc, target3) = (
                    ctx2.clone(),
                    ss.clone(),
                    st.clone(),
                    svc.clone(),
                    target2.clone(),
                );
                let generation = ctx3.shared.pair_gen.fetch_add(1, Ordering::SeqCst) + 1;
                std::thread::spawn(move || {
                    let current = || ctx3.shared.pair_gen.load(Ordering::SeqCst) == generation;
                    match trust::pair_and_save(
                        &target3.host.addr,
                        target3.host.port,
                        &ctx3.identity,
                        &pin,
                        &trust::device_name(),
                        &target3.host.name,
                        &target3.host.mac,
                    ) {
                        Ok(paired) => {
                            if !current() {
                                return;
                            }
                            // Only a link's pairing streams; elsewhere the saved host's
                            // Connect dials it. `connect` clears the status line, so a save
                            // error goes after it.
                            if target3.from_link {
                                connect(&ctx3, &target3, Some(paired.fp), &ss, &st);
                                if let Err(e) = paired.saved {
                                    st.call(format!("Paired, but couldn't save — {e:#}"));
                                }
                                return;
                            }
                            svc.report(match paired.saved {
                                Ok(()) => Ok(format!("Paired with {}", target3.host.name)),
                                Err(e) => Err(format!("Paired, but couldn't save — {e:#}")),
                            });
                            ss.call(Screen::Hosts);
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
        icon_button("x", "Cancel").on_click(move || {
            ctx2.shared.pair_gen.fetch_add(1, Ordering::SeqCst);
            ss.call(Screen::Hosts);
        })
    };
    // The no-PIN alternative offered alongside the PIN ceremony: open an identified connect that
    // the host parks until the operator approves this device in its console (delegated approval).
    let request_btn = {
        let (svc, target2) = (props.clone(), target.clone());
        icon_button("send", "Request access without a PIN")
            .on_click(move || request_access(&svc, &target2))
            .horizontal_alignment(HorizontalAlignment::Stretch)
    };

    let live = live_pin.clone();
    let content = card(
        vstack((
            Grid::new()
                .children((
                    // `Target` holds no OS chain, so the pairing card keeps the monogram.
                    avatar(pal, &target.host.name, "")
                        .grid_column(0)
                        .vertical_alignment(VerticalAlignment::Center),
                    vstack((
                        text(format!("Pair with {}", target.host.name))
                            .font_size(20.0)
                            .font_weight(FontWeight::SEMI_BOLD),
                        muted(pal, format!("{}:{}", target.host.addr, target.host.port))
                            .font_size(12.0),
                    ))
                    .spacing(2.0)
                    .grid_column(1)
                    .vertical_alignment(VerticalAlignment::Center)
                    .margin(edges(12.0, 0.0, 0.0, 0.0)),
                ))
                .columns([GridLength::Auto, GridLength::STAR]),
            InfoBar::new()
                .title("Arm pairing on the host")
                .message(
                    "On the host's console or web console, start pairing — it shows a 4-digit \
                     PIN. Enter it below within 90 seconds.",
                )
                .severity(InfoBarSeverity::Informational)
                .is_open(true)
                .is_closable(false),
            TextBox::new()
                .placeholder_text("PIN")
                .on_text_changed(move |s: Rc<str>| *live.borrow_mut() = s.to_string()),
            hstack((pair_btn, cancel_btn)).spacing(8.0),
            muted(
                pal,
                "Don\u{2019}t have a PIN? Request access instead and approve this device on the \
                 host (its console or web UI) \u{2014} no PIN needed.",
            )
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap),
            request_btn,
        ))
        .spacing(16.0),
    )
    .max_width(480.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .margin(edges(0.0, 60.0, 0.0, 0.0));

    page(vec![content.into()])
}
