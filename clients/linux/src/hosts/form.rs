//! The Add and Edit forms' connection rows: address, port and Wake-on-LAN MACs.

use super::*;
use crate::trust::HostField;

/// Address, port and Wake-on-LAN MAC rows, checked as they are typed. A row the store would
/// refuse wears the `error` style, and `problem` says why under the list.
pub(super) struct ConnectionRows {
    addr: adw::EntryRow,
    port: adw::EntryRow,
    macs: adw::EntryRow,
    problem: gtk::Label,
}

impl ConnectionRows {
    pub(super) fn new(addr: &str, port: u16, macs: &[String]) -> Self {
        let port_row = adw::EntryRow::builder()
            .title("Port")
            .text(port.to_string())
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let macs_row = adw::EntryRow::builder()
            .title("Wake-on-LAN MAC addresses")
            .text(macs.join(", "))
            .build();
        macs_row.set_tooltip_text(Some(
            "Wakes this host from sleep. Found on the network automatically when it's on; \
             separate several with commas.",
        ));
        let problem = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .margin_top(6)
            .css_classes(["caption", "error"])
            .build();
        ConnectionRows {
            addr: adw::EntryRow::builder().title("Address").text(addr).build(),
            port: port_row,
            macs: macs_row,
            problem,
        }
    }

    pub(super) fn append_to(&self, list: &gtk::ListBox) {
        list.append(&self.addr);
        list.append(&self.port);
        list.append(&self.macs);
    }

    /// `list` with the problem line under it — what a dialog takes as its extra child.
    pub(super) fn framed(&self, list: &gtk::ListBox) -> gtk::Box {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.append(list);
        frame.append(&self.problem);
        frame
    }

    pub(super) fn edit(&self) -> Option<HostEdit> {
        HostEdit::parse(&self.addr.text(), &self.port.text(), &self.macs.text()).ok()
    }

    /// Re-check on every keystroke, enabling `response` only while the rows parse. The
    /// handlers hold the widgets weakly: the rows live inside the dialog they re-check.
    pub(super) fn enable_when_valid(&self, dialog: &adw::AlertDialog, response: &'static str) {
        let rows = [
            (HostField::Addr, self.addr.downgrade()),
            (HostField::Port, self.port.downgrade()),
            (HostField::Macs, self.macs.downgrade()),
        ];
        let (problem, dialog) = (self.problem.downgrade(), dialog.downgrade());
        let check = Rc::new(move || {
            let text = |i: usize| rows[i].1.upgrade().map(|r| r.text()).unwrap_or_default();
            let result = HostEdit::parse(&text(0), &text(1), &text(2));
            for (field, row) in &rows {
                if let Some(row) = row.upgrade() {
                    if matches!(&result, Err(e) if e.field == *field) {
                        row.add_css_class("error");
                    } else {
                        row.remove_css_class("error");
                    }
                }
            }
            if let Some(problem) = problem.upgrade() {
                problem.set_label(result.as_ref().err().map_or("", |e| e.message.as_str()));
                problem.set_visible(result.is_err());
            }
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled(response, result.is_ok());
            }
        });
        for row in [&self.addr, &self.port, &self.macs] {
            let check = check.clone();
            row.connect_changed(move |_| check());
        }
        check();
    }
}
