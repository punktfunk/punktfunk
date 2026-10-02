//! The network speed test as a page (design §2.4): live goodput drawn while the host bursts,
//! then what was measured, what it recommends, and where the recommendation goes. A measured
//! bitrate lands in the layer this host reads bitrate from (client-settings-profiles.md §5.3).

use super::ConnectRequest;
use crate::store::Store;
use adw::prelude::*;
use gtk::glib;
use pf_client_core::presets::StreamPreset;
use pf_client_core::speed::{recommended_kbps, CleanRound, HealthReport, SpeedReport};
use std::cell::RefCell;
use std::rc::Rc;

/// Open the page and start the test. `done` runs once the test ends, whether or not the page
/// is still open.
pub fn push(
    nav: &adw::NavigationView,
    store: Rc<Store>,
    identity: (String, String),
    req: ConnectRequest,
    toasts: &adw::ToastOverlay,
    done: impl FnOnce() + 'static,
) {
    let target = Target::resolve(&req, &store);
    let chart = Rc::new(RefCell::new(Chart {
        current: target.current(&store),
        ..Chart::default()
    }));
    let area = gtk::DrawingArea::builder()
        .content_height(180)
        .hexpand(true)
        .build();
    {
        let chart = chart.clone();
        area.set_draw_func(move |area, cr, w, h| draw(area, cr, w, h, &chart.borrow()));
    }
    let headline = gtk::Label::builder()
        .label("Measuring\u{2026}")
        .css_classes(["title-1"])
        .build();
    let caption = gtk::Label::builder()
        .label(format!("The host sends a short burst to {}", req.name))
        .css_classes(["dim-label"])
        .wrap(true)
        .build();
    let results = adw::PreferencesGroup::new();
    results.set_visible(false);
    let row = |title: &str| {
        let value = gtk::Label::builder().css_classes(["dim-label"]).build();
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(&value);
        results.add(&row);
        value
    };
    let (loss, received, recommended, current) = (
        row("Loss"),
        row("Received"),
        row("Recommended bitrate"),
        row("Current bitrate"),
    );
    current.set_label(&bitrate_label(chart.borrow().current));
    // What the check found, one row each; hidden until there is something to say.
    let findings = adw::PreferencesGroup::builder()
        .title("What the check found")
        .build();
    findings.set_visible(false);
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(24);
    body.set_margin_bottom(24);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&headline);
    body.append(&caption);
    body.append(&area);
    body.append(&legend());
    body.append(&results);
    body.append(&findings);
    body.append(&buttons);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&adw::Clamp::builder().maximum_size(640).child(&body).build())
            .build(),
    ));
    nav.push(&adw::NavigationPage::new(&toolbar, "Network Speed"));

    enum Probe {
        Sample(u32),
        // Boxed: the report is 300 bytes against a sample's four.
        Done(Result<Box<HealthReport>, String>),
    }
    let (tx, rx) = async_channel::unbounded::<Probe>();
    // The host's record and name, for the offer: `req` moves into the worker below.
    let (offer_fp, host_name) = (req.fp_hex.clone(), req.name.clone());
    std::thread::Builder::new()
        .name("punktfunk-speed".into())
        .spawn(move || {
            let progress = |kbps| {
                let _ = tx.send_blocking(Probe::Sample(kbps));
            };
            let result = if crate::shots::shot_scene().is_some() {
                canned(progress)
            } else {
                let fp = req.fp_hex.as_deref();
                pf_client_core::speed::run_network_check_with(
                    &req.addr, req.port, fp, identity, progress,
                )
            };
            let _ = tx.send_blocking(Probe::Done(result.map(Box::new)));
        })
        .expect("spawn speed thread");
    let toasts = toasts.clone();
    glib::spawn_future_local(async move {
        let mut done = Some(done);
        while let Ok(msg) = rx.recv().await {
            match msg {
                Probe::Sample(kbps) => {
                    chart.borrow_mut().samples.push(kbps);
                    headline.set_label(&mbit(kbps));
                    area.queue_draw();
                }
                Probe::Done(Err(msg)) => {
                    if let Some(done) = done.take() {
                        done();
                    }
                    headline.set_label("Couldn't measure");
                    caption.set_label(&msg);
                }
                Probe::Done(Ok(report)) => {
                    if let Some(done) = done.take() {
                        done();
                    }
                    let r = report.speed;
                    let rec = recommended_kbps(r.ceiling_kbps);
                    {
                        let mut c = chart.borrow_mut();
                        c.measured = Some(r.ceiling_kbps);
                        c.recommended = Some(rec);
                    }
                    area.queue_draw();
                    headline.set_label(&mbit(r.ceiling_kbps));
                    for f in &report.findings {
                        let row = adw::ActionRow::builder()
                            .title(pf_client_core::findings::text(f.id as u8, f.numbers))
                            .title_lines(0)
                            .build();
                        findings.add(&row);
                    }
                    findings.set_visible(!report.findings.is_empty());
                    // The profile a finding offered goes on this host's record; the next
                    // connect asks for it. Only a saved host has a record.
                    let offered = report.findings.iter().find_map(|f| f.profile);
                    if let (Some(profile), Some(fp)) = (offered, offer_fp.clone()) {
                        let b = gtk::Button::builder()
                            .label(format!(
                                "Use paced delivery ({})",
                                pf_client_core::findings::profile_name(profile)
                            ))
                            .css_classes(["pill"])
                            .build();
                        let (store, toasts, name) =
                            (store.clone(), toasts.clone(), host_name.clone());
                        b.connect_clicked(move |_| {
                            let written = store.update_hosts(|k| {
                                k.hosts.iter_mut().find(|h| h.fp_hex == fp).map(|h| {
                                    h.delivery = Some(profile);
                                })
                            });
                            let text = match written {
                                Ok(Some(())) => format!(
                                    "Paced delivery set for {name} \u{2014} it applies from the \
                                     next connect"
                                ),
                                _ => "Couldn't save the host's record".to_string(),
                            };
                            toasts.add_toast(adw::Toast::new(&text));
                        });
                        buttons.append(&b);
                    }
                    // The loss figure is the clean round's, at a rate the link holds. A host
                    // without a ramp only ever measured the blast, which says nothing.
                    match r.clean {
                        Some(c) => {
                            caption.set_label(&format!(
                                "At {}: {:.1} % loss, {:.1} ms jitter",
                                mbit(c.rate_kbps),
                                c.loss_pct,
                                f64::from(c.jitter_us) / 1000.0
                            ));
                            loss.set_label(&format!("{:.1} %", c.loss_pct));
                            received.set_label(&format!(
                                "{} of {} packets",
                                c.outcome.recv_packets, c.outcome.wire_packets_sent
                            ));
                        }
                        None => {
                            caption.set_label("Measured over the real data plane");
                            loss.set_label("\u{2014}");
                            let b = r.blast.unwrap_or_default();
                            received.set_label(&format!(
                                "{} of {} packets",
                                b.recv_packets, b.wire_packets_sent
                            ));
                        }
                    }
                    recommended.set_label(&mbit(rec));
                    results.set_visible(true);
                    let applied = {
                        let (chart, area, current) = (chart.clone(), area.clone(), current.clone());
                        move |kbps: u32| {
                            chart.borrow_mut().current = kbps;
                            current.set_label(&bitrate_label(kbps));
                            area.queue_draw();
                        }
                    };
                    for (label, layer) in target.choices() {
                        let b = gtk::Button::builder()
                            .label(label)
                            .css_classes(["pill"])
                            .build();
                        let (store, toasts, applied) =
                            (store.clone(), toasts.clone(), applied.clone());
                        b.connect_clicked(move |_| {
                            let r = layer.write(&store, rec);
                            toasts.add_toast(adw::Toast::new(&r));
                            applied(rec);
                        });
                        buttons.append(&b);
                    }
                    if let Some(last) = buttons.last_child() {
                        last.add_css_class("suggested-action");
                    }
                }
            }
        }
        // The worker never reported: still release the caller.
        if let Some(done) = done.take() {
            done();
        }
    });
}

/// The screenshot scenes' measurement: a link whose ramp proved ~940 Mbit/s, then a clean
/// round at half of it, and nothing to fix.
fn canned(mut progress: impl FnMut(u32)) -> Result<HealthReport, String> {
    for kbps in [40_000, 180_000, 310_000, 410_000, 462_000, 470_000, 468_000] {
        progress(kbps);
    }
    let outcome = punktfunk_core::client::ProbeOutcome {
        done: true,
        throughput_kbps: 468_000,
        loss_pct: 0.0,
        recv_packets: 97_900,
        wire_packets_sent: 97_900,
        gap_p50_us: 100,
        gap_p99_us: 400,
        ..Default::default()
    };
    Ok(HealthReport {
        speed: SpeedReport {
            ceiling_kbps: 940_000,
            wall: true,
            clean: Some(CleanRound {
                rate_kbps: 470_000,
                loss_pct: 0.0,
                jitter_us: 300,
                reorders: 0,
                outcome,
            }),
            blast: None,
        },
        ..Default::default()
    })
}

fn mbit(kbps: u32) -> String {
    format!("{:.0} Mbit/s", f64::from(kbps) / 1000.0)
}

fn bitrate_label(kbps: u32) -> String {
    match kbps {
        0 => "Host default".into(),
        k => mbit(k),
    }
}

/// What the chart shows, in kbps.
#[derive(Default)]
struct Chart {
    /// Live goodput, one per poll.
    samples: Vec<u32>,
    measured: Option<u32>,
    recommended: Option<u32>,
    /// The bitrate this host streams at now; 0 = the host's default, not drawn.
    current: u32,
}

fn draw(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32, c: &Chart) {
    let fg = area.color();
    let accent = adw::StyleManager::default().accent_color_rgba();
    let (w, h) = (f64::from(w), f64::from(h));
    let top = c
        .samples
        .iter()
        .copied()
        .chain(c.measured)
        .chain(c.recommended)
        .chain([c.current])
        .max()
        .unwrap_or(0)
        .max(1000) as f64
        * 1.15;
    let y = |kbps: u32| h - 4.0 - f64::from(kbps) / top * (h - 8.0);
    let ink = |rgba: &gtk::gdk::RGBA, alpha: f64| {
        cr.set_source_rgba(
            f64::from(rgba.red()),
            f64::from(rgba.green()),
            f64::from(rgba.blue()),
            alpha,
        )
    };
    let across = |kbps: u32, dashed: bool| {
        cr.set_dash(if dashed { &[6.0, 4.0] } else { &[] }, 0.0);
        cr.move_to(0.0, y(kbps));
        cr.line_to(w, y(kbps));
        let _ = cr.stroke();
    };
    cr.set_line_width(1.0);
    ink(&fg, 0.15);
    across(0, false);
    if c.current > 0 {
        ink(&fg, 0.45);
        across(c.current, true);
    }
    if let Some(r) = c.recommended {
        ink(&accent, 0.9);
        across(r, true);
    }
    if let Some(m) = c.measured {
        ink(&fg, 0.6);
        across(m, false);
    }
    let n = c.samples.len();
    if n > 0 {
        cr.set_dash(&[], 0.0);
        cr.set_line_width(2.5);
        ink(&accent, 1.0);
        for (i, &k) in c.samples.iter().enumerate() {
            let x = if n == 1 {
                w / 2.0
            } else {
                i as f64 / (n - 1) as f64 * w
            };
            if i == 0 {
                cr.move_to(x, y(k));
            } else {
                cr.line_to(x, y(k));
            }
        }
        let _ = cr.stroke();
    }
}

fn legend() -> gtk::Box {
    let legend = gtk::Box::builder()
        .spacing(18)
        .halign(gtk::Align::Center)
        .build();
    for (text, class) in [
        ("\u{2501} Live goodput", "accent"),
        ("\u{2501} Measured", "dim-label"),
        ("\u{2504} Recommended", "accent"),
        ("\u{2504} Current bitrate", "dim-label"),
    ] {
        legend.append(
            &gtk::Label::builder()
                .label(text)
                .css_classes(["caption", class])
                .build(),
        );
    }
    legend
}

/// Where a measured bitrate goes for the host that was tested.
enum Target {
    /// No preset bound: the default bitrate.
    Global,
    /// The bound preset overrides bitrate, so that override is what this host reads.
    Preset(StreamPreset),
    /// Bound, but the preset inherits bitrate: either layer is defensible, so both are offered.
    Ask(StreamPreset),
}

/// One place a recommendation can be written.
#[derive(Clone)]
enum Layer {
    Global,
    Preset { id: String, name: String },
}

impl Layer {
    /// Write `kbps` and say where it went.
    fn write(&self, store: &Store, kbps: u32) -> String {
        match self {
            Layer::Global => {
                store.update_settings(|s| s.bitrate_kbps = kbps);
                format!("{} set as the default bitrate", mbit(kbps))
            }
            Layer::Preset { id, name } => {
                let saved = store.update_presets(|catalog| {
                    if let Some(slot) = catalog.presets.iter_mut().find(|x| &x.id == id) {
                        slot.overrides.bitrate_kbps = Some(kbps);
                    }
                });
                match saved {
                    Ok(()) => format!("{} set in \u{201c}{name}\u{201d}", mbit(kbps)),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "measured bitrate not saved");
                        format!("Couldn't save the bitrate \u{2014} {e:#}")
                    }
                }
            }
        }
    }
}

impl Target {
    /// Resolved as a connect resolves it: the one-off this test was started with (a pinned
    /// card carries one), else the host's binding.
    fn resolve(req: &ConnectRequest, store: &Store) -> Target {
        let bound = store
            .hosts()
            .resolve(req.fp_hex.as_deref(), &req.addr, req.port)
            .and_then(|h| h.preset_id.clone());
        let reference = match req.preset.as_deref() {
            Some("") => return Target::Global,
            Some(id) => Some(id.to_string()),
            None => bound,
        };
        let Some(reference) = reference else {
            return Target::Global;
        };
        let catalog = store.presets();
        match catalog.resolve(&reference).0 {
            Some(p) if p.overrides.bitrate_kbps.is_some() => Target::Preset(p.clone()),
            Some(p) => Target::Ask(p.clone()),
            // A dangling binding resolves as no preset everywhere else; here too.
            None => Target::Global,
        }
    }

    /// The bitrate this host streams at today.
    fn current(&self, store: &Store) -> u32 {
        match self {
            Target::Preset(p) => p.overrides.bitrate_kbps.unwrap_or_default(),
            Target::Global | Target::Ask(_) => store.settings().bitrate_kbps,
        }
    }

    /// The buttons, the suggested one last.
    fn choices(&self) -> Vec<(String, Layer)> {
        let preset = |p: &StreamPreset| Layer::Preset {
            id: p.id.clone(),
            name: p.name.clone(),
        };
        match self {
            Target::Global => vec![("Apply".into(), Layer::Global)],
            Target::Preset(p) => vec![(format!("Apply to \u{201c}{}\u{201d}", p.name), preset(p))],
            Target::Ask(p) => vec![
                ("Set as default".into(), Layer::Global),
                (format!("Set in \u{201c}{}\u{201d}", p.name), preset(p)),
            ],
        }
    }
}
