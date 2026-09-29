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

    /// The speed-test page. The test is a connect, so it holds `busy` until it ends.
    pub(super) fn speed_test(&mut self, req: ConnectRequest, sender: &ComponentSender<AppModel>) {
        if std::mem::replace(&mut self.busy, true) {
            return;
        }
        let sender = sender.clone();
        crate::hosts::speed::push(
            &self.nav,
            self.store.clone(),
            self.identity.clone(),
            req,
            &self.toasts,
            move || sender.input(AppMsg::SpeedTestDone),
        );
    }
}
