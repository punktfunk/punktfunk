//! What the shell asks of a host besides a stream: a speed test, its log bundle, its own
//! actions (sleep, restart, shut down).

use super::*;

impl AppModel {
    pub(super) fn send_logs(
        &self,
        req: ConnectRequest,
        mgmt_port: Option<u16>,
        sender: &ComponentSender<Self>,
    ) {
        // Blocking network (the library agent's 5 s connect / 10 s global budgets) —
        // a worker thread, with the outcome routed back as a Toast.
        let identity = self.identity.clone();
        let mgmt = mgmt_port.unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
        self.toast(&format!("Sending logs to {}…", req.name));
        let out = sender.input_sender().clone();
        std::thread::Builder::new()
            .name("punktfunk-sendlogs".into())
            .spawn(move || {
                let msg = pf_client_core::logring::send_bundle(
                    "punktfunk-client",
                    &req.name,
                    &req.addr,
                    mgmt,
                    &identity,
                    req.fp_hex.as_deref().unwrap_or_default(),
                );
                let _ = out.send(AppMsg::Toast(msg));
            })
            .ok();
    }

    pub(super) fn host_action(
        &self,
        req: ConnectRequest,
        mgmt: Option<u16>,
        action_id: String,
        label: String,
        danger: bool,
        sender: &ComponentSender<Self>,
    ) {
        let mgmt = mgmt.unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
        // Restart and shut down lose whatever is running on that machine, so they ask
        // first — the same treatment Forget gets. Sleep is reversible from the same
        // menu ("Wake host"), so it goes straight through.
        if danger {
            let dialog = adw::AlertDialog::new(
                Some(&format!("{label}?")),
                Some(&format!(
                    "This ends every stream from {} and anything running on it. \
                     You'll need to wake or start it again.",
                    req.name
                )),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("go", &label)]);
            dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let out = sender.input_sender().clone();
            let (req, action_id, label) = (req.clone(), action_id.clone(), label.clone());
            dialog.connect_response(Some("go"), move |_, _| {
                out.send(AppMsg::HostAction {
                    req: req.clone(),
                    mgmt: Some(mgmt),
                    action_id: action_id.clone(),
                    label: label.clone(),
                    // Asked and answered.
                    danger: false,
                })
                .ok();
            });
            dialog.present(Some(&self.window));
            return;
        }
        // Blocking network on a worker, outcome as a toast — the SendLogs recipe.
        let identity = self.identity.clone();
        self.toast(&format!("{label} — asking {}…", req.name));
        let out = sender.input_sender().clone();
        std::thread::Builder::new()
            .name("punktfunk-hostaction".into())
            .spawn(move || {
                let msg = pf_client_core::host_actions::run(
                    &req.name,
                    &req.addr,
                    mgmt,
                    &identity,
                    req.fp_hex.as_deref().unwrap_or_default(),
                    &action_id,
                    &label,
                );
                let _ = out.send(AppMsg::Toast(msg));
            })
            .ok();
    }

    /// Measure the path to a host over the real data plane: connect, burst probe filler
    /// for 2 s, report goodput · loss · a recommended bitrate, and apply it in one tap.
    pub(super) fn speed_test(&mut self, req: ConnectRequest, sender: &ComponentSender<AppModel>) {
        if std::mem::replace(&mut self.busy, true) {
            return;
        }
        let status = gtk::Label::new(Some("Connecting…"));
        let dialog = adw::AlertDialog::new(Some("Network Speed Test"), Some(&req.name));
        dialog.set_extra_child(Some(&status));
        // Where a measured bitrate belongs is "the layer this host actually resolves bitrate
        // from" (design/client-settings-profiles.md §5.3) — the long-standing wrong answer was
        // always the global, so measuring the slow retro box downstairs re-tuned the desktop
        // too. The target depends only on the host, so it is known before the result lands and
        // the button can say where it will write.
        let target = SpeedTestTarget::resolve(&req, &self.store);
        match &target {
            SpeedTestTarget::Global => {
                dialog.add_responses(&[("close", "Close"), ("apply", "Apply")]);
            }
            SpeedTestTarget::Preset(p) => {
                dialog.add_responses(&[
                    ("close", "Close"),
                    ("apply", &format!("Apply to “{}”", p.name)),
                ]);
            }
            // A bound host whose preset doesn't override bitrate could legitimately mean
            // either: the user gets both, rather than us guessing which layer they meant.
            SpeedTestTarget::Ask(p) => {
                dialog.add_responses(&[
                    ("close", "Close"),
                    ("apply-global", "Set as default"),
                    ("apply", &format!("Set in “{}”", p.name)),
                ]);
                dialog.set_response_enabled("apply-global", false);
            }
        }
        dialog.set_response_enabled("apply", false);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));

        let (tx, rx) =
            async_channel::bounded::<Result<punktfunk_core::client::ProbeOutcome, String>>(1);
        let identity = self.identity.clone();
        std::thread::spawn(move || {
            let result = pf_client_core::speed::run_speed_probe(
                &req.addr,
                req.port,
                req.fp_hex.as_deref(),
                identity,
            );
            let _ = tx.send_blocking(result);
        });

        let store = self.store.clone();
        let toasts = self.toasts.clone();
        let sender = sender.clone();
        glib::spawn_future_local(async move {
            let outcome = rx.recv().await;
            sender.input(AppMsg::SpeedTestDone);
            match outcome {
                Ok(Ok(r)) => {
                    let mbps = f64::from(r.throughput_kbps) / 1000.0;
                    let recommended_kbps =
                        pf_client_core::speed::recommended_kbps(r.throughput_kbps);
                    status.set_text(&format!(
                        "{mbps:.0} Mbit/s measured · {:.1} % loss\nRecommended bitrate: {:.0} Mbit/s",
                        r.loss_pct,
                        f64::from(recommended_kbps) / 1000.0,
                    ));
                    dialog.set_response_enabled("apply", true);
                    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
                    if matches!(target, SpeedTestTarget::Ask(_)) {
                        dialog.set_response_enabled("apply-global", true);
                    }
                    let mbit = f64::from(recommended_kbps) / 1000.0;
                    {
                        let (store, toasts) = (store.clone(), toasts.clone());
                        dialog.connect_response(Some("apply"), move |_, _| {
                            let where_to = match &target {
                                SpeedTestTarget::Global => {
                                    store.update_settings(|s| s.bitrate_kbps = recommended_kbps);
                                    "the default bitrate".to_string()
                                }
                                SpeedTestTarget::Preset(p) | SpeedTestTarget::Ask(p) => {
                                    write_preset_bitrate(&store, &p.id, recommended_kbps);
                                    format!("“{}”", p.name)
                                }
                            };
                            toasts.add_toast(adw::Toast::new(&format!(
                                "{mbit:.0} Mbit/s set in {where_to}"
                            )));
                        });
                    }
                    dialog.connect_response(Some("apply-global"), move |_, _| {
                        store.update_settings(|s| s.bitrate_kbps = recommended_kbps);
                        toasts.add_toast(adw::Toast::new(&format!(
                            "{mbit:.0} Mbit/s set in the default bitrate"
                        )));
                    });
                }
                Ok(Err(msg)) => status.set_text(&msg),
                Err(_) => {}
            }
        });
    }
}

/// Which layer a measured bitrate should land in for the host that was tested
/// (design/client-settings-profiles.md §5.3).
enum SpeedTestTarget {
    /// No preset bound — the global default, i.e. what has always happened.
    Global,
    /// The bound preset already overrides bitrate, so that override is what this host reads.
    Preset(pf_client_core::presets::StreamPreset),
    /// Bound, but the preset inherits bitrate: writing either layer is defensible, so ask.
    Ask(pf_client_core::presets::StreamPreset),
}

impl SpeedTestTarget {
    fn resolve(req: &crate::hosts::ConnectRequest, store: &Store) -> SpeedTestTarget {
        // Resolved exactly the way a connect resolves it: the one-off pick this test was
        // started with (a pinned card carries one), else the host's binding.
        let bound = store
            .hosts()
            .resolve(req.fp_hex.as_deref(), &req.addr, req.port)
            .and_then(|h| h.preset_id.clone());
        let reference = match req.preset.as_deref() {
            Some("") => return SpeedTestTarget::Global,
            Some(id) => Some(id.to_string()),
            None => bound,
        };
        let Some(reference) = reference else {
            return SpeedTestTarget::Global;
        };
        let catalog = store.presets();
        match catalog.resolve(&reference).0 {
            Some(p) if p.overrides.bitrate_kbps.is_some() => SpeedTestTarget::Preset(p.clone()),
            Some(p) => SpeedTestTarget::Ask(p.clone()),
            // A dangling binding resolves as no preset everywhere else; here too.
            None => SpeedTestTarget::Global,
        }
    }
}

/// Write a measured bitrate into one preset's overlay, leaving everything else alone. A
/// preset deleted while the test ran is left deleted; the toast still reports the test.
fn write_preset_bitrate(store: &Store, id: &str, kbps: u32) {
    let saved = store.update_presets(|catalog| {
        if let Some(p) = catalog.presets.iter_mut().find(|p| p.id == id) {
            p.overrides.bitrate_kbps = Some(kbps);
        }
    });
    if let Err(e) = saved {
        tracing::warn!(error = %format!("{e:#}"), "measured bitrate not saved");
    }
}
