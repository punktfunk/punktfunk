//! Per-host network speed test (the GTK/Swift clients' "Test Network Speed…"): connect over the
//! real data plane, have the host burst probe filler for 2 s up to its 3 Gbps ceiling, and
//! report goodput · loss · a recommended bitrate (≈70 % of measured), applied in one tap.

use super::lucide;
use super::style::*;
use super::{saved, Screen, Svc};
use crate::trust::KnownHosts;
use pf_client_core::presets::PresetsFile;
use windows_reactor::*;

/// Speed-test lifecycle. Held as ROOT state (the probe worker completes it via
/// `Svc::set_speed`, and thread-driven updates only re-render through a prop change — see the
/// app module docs). The hosts page resets it to `Running` before navigating here.
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

pub(crate) fn speed_page(props: &SpeedProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let set_screen = &props.svc.set_screen;
    let target = ctx.shared.target.lock().unwrap().clone();

    // One probe run per mount (navigating here again re-mounts and re-runs).
    cx.use_effect((), {
        let set_speed = props.svc.set_speed.clone();
        let shared = ctx.shared.clone();
        let identity = ctx.identity.clone();
        let target = target.clone();
        move || {
            use std::sync::atomic::Ordering;
            // The generation the hosts page stamped for THIS run; a stale worker (user backed
            // out and started another test) must not publish over the newer run.
            let generation = shared.speed_gen.load(Ordering::SeqCst);
            std::thread::Builder::new()
                .name("pf-speedtest".into())
                .spawn(move || {
                    let outcome = pf_client_core::speed::run_network_check_with(
                        &target.addr,
                        target.port,
                        target.fp_hex.as_deref(),
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
                            recommended_kbps: pf_client_core::speed::recommended_kbps(
                                r.speed.ceiling_kbps,
                            ),
                            findings: r.findings.iter().map(|f| (f.id as u8, f.numbers)).collect(),
                        },
                        Err(msg) => SpeedState::Failed(msg),
                    });
                })
                .ok();
        }
    });

    let back_btn = {
        let ss = set_screen.clone();
        button("Close")
            .icon(lucide::icon("arrow-left"))
            .on_click(move || ss.call(Screen::Hosts))
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    let headline = if target.name.is_empty() {
        "Network speed test".to_string()
    } else {
        format!("Network speed test \u{00B7} {}", target.name)
    };

    match &props.state {
        SpeedState::Running => busy_page(
            &headline,
            "Measuring the path over the real data plane \u{2014} a 2 s probe burst\u{2026}",
            vec![back_btn.into()],
        ),
        SpeedState::Failed(msg) => {
            let content = vstack((
                text_block(headline)
                    .font_size(18.0)
                    .semibold()
                    .horizontal_alignment(HorizontalAlignment::Center),
                InfoBar::new("Speed test failed")
                    .message(msg.clone())
                    .error()
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
                .resolve(target.fp_hex.as_deref(), &target.addr, target.port)
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
                    // Rebase on the file before the whole-struct save — same discipline as
                    // `commit()`; another writer may have moved it under this snapshot.
                    let mut s = ctx.settings.lock().unwrap();
                    *s = crate::trust::Settings::load();
                    s.bitrate_kbps = kbps;
                    s.save();
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
            let mut buttons: Vec<Element> = Vec::new();
            match &preset {
                None => buttons.push(
                    button(format!("Use {recommended_mbps:.0} Mb/s"))
                        .accent()
                        .icon(lucide::icon("check"))
                        .on_click(write_global.clone())
                        .into(),
                ),
                Some(p) if p.overrides.bitrate_kbps.is_some() => buttons.push(
                    button(format!(
                        "Set {recommended_mbps:.0} Mb/s in \u{201c}{}\u{201d}",
                        p.name
                    ))
                    .accent()
                    .icon(lucide::icon("check"))
                    .on_click(write_preset(p.id.clone()))
                    .into(),
                ),
                Some(p) => {
                    buttons.push(
                        button("Set as default")
                            .icon(lucide::icon("check"))
                            .on_click(write_global.clone())
                            .into(),
                    );
                    buttons.push(
                        button(format!("Set in \u{201c}{}\u{201d}", p.name))
                            .accent()
                            .icon(lucide::icon("check"))
                            .on_click(write_preset(p.id.clone()))
                            .into(),
                    );
                }
            }
            buttons.push({
                let ss = set_screen.clone();
                button("Close")
                    .icon(lucide::icon("x"))
                    .on_click(move || ss.call(Screen::Hosts))
                    .into()
            });
            let finding_lines: Vec<Element> = findings
                .iter()
                .map(|(id, numbers)| {
                    text_block(pf_client_core::findings::text(*id, *numbers))
                        .font_size(12.0)
                        .foreground(ThemeRef::SecondaryText)
                        .horizontal_alignment(HorizontalAlignment::Center)
                        .into()
                })
                .collect();
            let results = card(
                vstack((
                    text_block(format!("{mbps:.0} Mbit/s"))
                        .font_size(34.0)
                        .bold()
                        .horizontal_alignment(HorizontalAlignment::Center),
                    text_block(match clean {
                        Some((rate, loss, jitter)) => format!(
                            "at {rate:.0} Mbit/s \u{00B7} {loss:.1} % loss \u{00B7} {jitter:.1} ms jitter"
                        ),
                        None => "measured".to_string(),
                    })
                        .font_size(12.0)
                        .foreground(ThemeRef::SecondaryText)
                        .horizontal_alignment(HorizontalAlignment::Center),
                    text_block(format!(
                    "Recommended bitrate: {recommended_mbps:.0} Mb/s (\u{2248}70 % of measured, \
                     leaving headroom for FEC and loss)"
                ))
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText)
                    .horizontal_alignment(HorizontalAlignment::Center),
                    vstack(finding_lines).spacing(4.0),
                    hstack(buttons)
                        .spacing(8.0)
                        .horizontal_alignment(HorizontalAlignment::Center),
                ))
                .spacing(12.0),
            );
            vstack((
                text_block(headline)
                    .font_size(18.0)
                    .semibold()
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
