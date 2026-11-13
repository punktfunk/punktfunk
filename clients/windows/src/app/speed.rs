//! Per-host network speed test (the GTK/Swift clients' "Test Network Speed…"): connect over the
//! real data plane, have the host burst probe filler for 2 s up to its 3 Gbps ceiling, and
//! report goodput · loss · a recommended bitrate (≈70 % of measured), applied in one tap.

use super::style::*;
use super::{saved, Screen, Svc};
use crate::trust::KnownHosts;
use pf_client_core::presets::PresetsFile;
use pf_fluent::Token;
use pf_l10n::{client, common, desktop};
use windows_reactor::*;

/// Speed-test lifecycle, root state: the probe worker completes it via `Svc::set_speed`, and
/// the hosts page resets it to `Running` before navigating here.
#[derive(Clone, PartialEq)]
pub(crate) enum SpeedState {
    Running,
    Failed(String),
    Done {
        /// What the link carries.
        mbps: f64,
        /// The clean round under it: `(rate Mbit/s, loss %, jitter ms)`. `None` toward a
        /// host without a ramp, which gets no loss line.
        clean: Option<(f64, f32, f64)>,
        recommended_kbps: u32,
        /// What the check found: `(id, figures)` per finding.
        findings: Vec<(u8, [u32; 3])>,
    },
}

/// Props for the speed page: the services plus the probe lifecycle that drives its re-render.
#[derive(Clone)]
pub(crate) struct SpeedProps {
    pub(crate) svc: Svc,
    pub(crate) state: SpeedState,
}

impl PartialEq for SpeedProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.state == other.state
    }
}

/// The speed test screen. One probe run per mount: navigating here again re-mounts and
/// re-runs it.
pub(crate) struct SpeedPage;

impl Component for SpeedPage {
    type Input = SpeedProps;
    type Message = ();

    fn create(props: &SpeedProps, _cx: &ComponentContext<Self>) -> Self {
        start_probe(&props.svc);
        SpeedPage
    }

    fn view(&self, props: &SpeedProps, _cx: &mut ViewContext<'_, Self>) -> View {
        speed_page(props)
    }
}

/// Run the probe against `Shared::target` on a worker thread, publishing into root state.
fn start_probe(svc: &Svc) {
    use std::sync::atomic::Ordering;
    let ctx = &svc.ctx;
    let target = ctx.shared.target.lock().unwrap().clone();
    let (set_speed, shared, identity) = (
        svc.set_speed.clone(),
        ctx.shared.clone(),
        ctx.identity.clone(),
    );
    // The generation the hosts page stamped for THIS run; a stale worker (user backed out and
    // started another test) must not publish over the newer run.
    let generation = shared.speed_gen.load(Ordering::SeqCst);
    std::thread::Builder::new()
        .name("pf-speedtest".into())
        .spawn(move || {
            let outcome = pf_client_core::speed::run_network_check_with(
                &target.host.addr,
                target.host.port,
                target.host.fp_hex.as_deref(),
                identity,
                |_| {},
            );
            if shared.speed_gen.load(Ordering::SeqCst) != generation {
                return; // superseded
            }
            set_speed.call(match outcome {
                Ok(r) => SpeedState::Done {
                    mbps: f64::from(r.speed.ceiling_kbps) / 1000.0,
                    clean: r.speed.clean.map(|c| {
                        (
                            f64::from(c.rate_kbps) / 1000.0,
                            c.loss_pct,
                            f64::from(c.jitter_us) / 1000.0,
                        )
                    }),
                    recommended_kbps: pf_client_core::speed::recommended_kbps(r.speed.ceiling_kbps),
                    findings: r.findings.iter().map(|f| (f.id as u8, f.numbers)).collect(),
                },
                Err(msg) => SpeedState::Failed(msg),
            });
        })
        .ok();
}

fn speed_page(props: &SpeedProps) -> View {
    let (ctx, pal) = (&props.svc.ctx, &props.svc.pal);
    let set_screen = &props.svc.set_screen;
    let target = ctx.shared.target.lock().unwrap().clone();
    let back_btn = {
        let ss = set_screen.clone();
        icon_button("arrow-left", common::common_close())
            .on_click(move || ss.call(Screen::Hosts))
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    let headline = if target.host.name.is_empty() {
        client::speedtest_title().to_string()
    } else {
        desktop::desktop_speedtest_title_host(&target.host.name)
    };

    match &props.state {
        SpeedState::Running => busy_page(
            pal,
            &headline,
            client::speedtest_measuring_body(),
            vec![back_btn.into()],
        ),
        SpeedState::Failed(msg) => {
            let content = vstack((
                text(headline)
                    .font_size(18.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .horizontal_alignment(HorizontalAlignment::Center),
                InfoBar::new()
                    .title(client::speedtest_failed_title(&target.host.name))
                    .is_open(true)
                    .message(msg.clone())
                    .severity(InfoBarSeverity::Error)
                    .is_closable(false),
                back_btn,
            ))
            .spacing(16.0)
            .max_width(480.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center);
            content.into()
        }
        SpeedState::Done {
            mbps,
            clean,
            recommended_kbps,
            findings,
        } => {
            let recommended_mbps = f64::from(*recommended_kbps) / 1000.0;
            // A measured bitrate belongs in the layer the TESTED host actually reads it from
            // (design/client-settings-profiles.md §5.3) — writing the global here is what made
            // measuring one host re-tune every other one. Resolved the way a connect resolves
            // it: the one-off this test was started with, else the host's binding.
            let target = ctx.shared.target.lock().unwrap().clone();
            let bound = KnownHosts::load()
                .resolve(
                    target.host.fp_hex.as_deref(),
                    &target.host.addr,
                    target.host.port,
                )
                .and_then(|h| h.preset_id.clone());
            let preset = match target.preset.as_deref() {
                Some("") => None,
                Some(id) => Some(id.to_string()),
                None => bound,
            }
            .and_then(|reference| PresetsFile::load().resolve(&reference).0.cloned());
            let kbps = *recommended_kbps;
            let write_global = {
                let (ctx, ss) = (ctx.clone(), set_screen.clone());
                move || {
                    // The file under its lock, as `commit()` edits it: another writer may have
                    // moved it under this snapshot.
                    let mut s = ctx.settings.lock().unwrap();
                    *s = crate::trust::Settings::update(|file| {
                        file.bitrate_kbps = kbps;
                        true
                    });
                    ss.call(Screen::Hosts);
                }
            };
            let write_preset = |id: String| {
                let (ss, st) = (set_screen.clone(), props.svc.set_status.clone());
                move || {
                    let r = super::settings::update_preset(&id, |p| {
                        p.overrides.bitrate_kbps = Some(kbps)
                    });
                    // The host list it returns to redraws anyway; no revision to bump.
                    saved(r, &st, None);
                    ss.call(Screen::Hosts);
                }
            };
            // Which button(s): no binding → the global; a binding that already overrides
            // bitrate → that override (it's what this host reads). Bound but INHERITING
            // bitrate could legitimately mean either layer — offer both rather than
            // guessing (the GTK client's Ask tier; this shell used to silently CREATE an
            // override on the preset).
            let mut buttons: Vec<View> = Vec::new();
            match &preset {
                None => buttons.push(
                    icon_button(
                        "check",
                        client::speedtest_use_rate(format!("{recommended_mbps:.0} Mbit/s")),
                    )
                    .style(ButtonStyle::Accent)
                    .on_click(write_global.clone())
                    .into(),
                ),
                Some(p) if p.overrides.bitrate_kbps.is_some() => buttons.push(
                    icon_button("check", client::speedtest_apply_to_preset(&p.name))
                        .style(ButtonStyle::Accent)
                        .on_click(write_preset(p.id.clone()))
                        .into(),
                ),
                Some(p) => {
                    buttons.push(
                        icon_button("check", client::speedtest_set_default())
                            .on_click(write_global.clone())
                            .into(),
                    );
                    buttons.push(
                        icon_button("check", client::speedtest_set_in_preset(&p.name))
                            .style(ButtonStyle::Accent)
                            .on_click(write_preset(p.id.clone()))
                            .into(),
                    );
                }
            }
            buttons.push({
                let ss = set_screen.clone();
                icon_button("x", common::common_close())
                    .on_click(move || ss.call(Screen::Hosts))
                    .into()
            });
            let finding_lines: Vec<View> = findings
                .iter()
                .map(|(id, numbers)| {
                    text(pf_client_core::findings::text(*id, *numbers))
                        .font_size(12.0)
                        .foreground(pal.get(Token::SecondaryText))
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .into()
                })
                .collect();
            let results = card(
                vstack((
                    text(format!("{mbps:.0} Mbit/s"))
                        .font_size(34.0)
                        .font_weight(FontWeight::BOLD)
                        .horizontal_alignment(HorizontalAlignment::Center),
                    text(match clean {
                        Some((rate, loss, jitter)) => client::speedtest_clean_round_body(
                            format!("{rate:.0} Mbit/s"),
                            format!("{loss:.1}"),
                            format!("{jitter:.1}"),
                        ),
                        None => desktop::desktop_speedtest_measured().to_string(),
                    })
                    .font_size(12.0)
                    .foreground(pal.get(Token::SecondaryText))
                    .horizontal_alignment(HorizontalAlignment::Center),
                    text(client::speedtest_recommended_body(format!(
                        "{recommended_mbps:.0} Mbit/s"
                    )))
                    .font_size(12.0)
                    .foreground(pal.get(Token::SecondaryText))
                    .horizontal_alignment(HorizontalAlignment::Center),
                    vstack(finding_lines).spacing(4.0),
                    hstack(buttons)
                        .spacing(8.0)
                        .horizontal_alignment(HorizontalAlignment::Center),
                ))
                .spacing(12.0),
            );
            vstack((
                text(headline)
                    .font_size(18.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .horizontal_alignment(HorizontalAlignment::Center),
                results,
            ))
            .spacing(16.0)
            .max_width(480.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
        }
    }
}
