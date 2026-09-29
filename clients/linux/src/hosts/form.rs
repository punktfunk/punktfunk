//! The Add and Edit forms' connection rows: address, port and Wake-on-LAN MACs.

use super::*;

/// Which connection row a typed value failed in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Addr,
    Port,
    Macs,
}

/// A typed value the store would refuse, and the sentence that says so.
#[derive(Debug, PartialEq)]
struct FieldError {
    field: Field,
    message: String,
}

/// The address row: trimmed, a pasted `host:port` split. `Err` is the sentence to show.
pub(super) fn parse_address(text: &str) -> Result<(String, Option<u16>), String> {
    let addr = text.trim();
    if addr.is_empty() {
        return Err("Enter the host's address.".into());
    }
    pf_client_core::deeplink::split_host_port(addr).ok_or_else(|| {
        format!("\u{201c}{addr}\u{201d} isn't an address. Use a name or an IP, like 192.168.1.20.")
    })
}

/// The port row; blank is the default 9777.
pub(super) fn parse_port(text: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(9777);
    }
    text.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| {
        format!("\u{201c}{text}\u{201d} isn't a port. Use a number from 1 to 65535.")
    })
}

/// The Wake-on-LAN row: a list, or empty to clear it.
pub(super) fn parse_macs(text: &str) -> Result<Vec<String>, String> {
    pf_client_core::wol::parse_mac_list(text).map_err(|bad| {
        format!("\u{201c}{bad}\u{201d} isn't a MAC address. Use six pairs, like aa:bb:cc:dd:ee:ff.")
    })
}

/// The connection rows' text as a store edit. A pasted `host:port` address wins over the port
/// row, and a blank port is the default 9777.
fn parse_connection(addr: &str, port: &str, macs: &str) -> Result<HostEdit, FieldError> {
    let at = |field| move |message| FieldError { field, message };
    let (addr, spelled) = parse_address(addr).map_err(at(Field::Addr))?;
    let port = parse_port(port).map_err(at(Field::Port))?;
    let macs = parse_macs(macs).map_err(at(Field::Macs))?;
    Ok(HostEdit {
        name: None,
        addr: Some(addr),
        port: Some(spelled.unwrap_or(port)),
        macs: Some(macs),
    })
}

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
        parse_connection(&self.addr.text(), &self.port.text(), &self.macs.text()).ok()
    }

    /// Re-check on every keystroke, enabling `response` only while the rows parse. The
    /// handlers hold the widgets weakly: the rows live inside the dialog they re-check.
    pub(super) fn enable_when_valid(&self, dialog: &adw::AlertDialog, response: &'static str) {
        let rows = [
            (Field::Addr, self.addr.downgrade()),
            (Field::Port, self.port.downgrade()),
            (Field::Macs, self.macs.downgrade()),
        ];
        let (problem, dialog) = (self.problem.downgrade(), dialog.downgrade());
        let check = Rc::new(move || {
            let text = |i: usize| rows[i].1.upgrade().map(|r| r.text()).unwrap_or_default();
            let result = parse_connection(&text(0), &text(1), &text(2));
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

#[cfg(test)]
mod form_tests {
    use super::*;

    fn failed(addr: &str, port: &str, macs: &str) -> Option<Field> {
        parse_connection(addr, port, macs).err().map(|e| e.field)
    }

    #[test]
    fn typed_rows_become_one_store_edit() {
        let edit = parse_connection(" desk.lan ", "", "AA-BB-CC-DD-EE-FF").unwrap();
        assert_eq!(edit.addr.as_deref(), Some("desk.lan"));
        assert_eq!(edit.port, Some(9777));
        assert_eq!(edit.macs, Some(vec!["aa:bb:cc:dd:ee:ff".to_string()]));
        assert_eq!(edit.name, None);
        // A pasted host:port wins over the port row; an empty MAC row clears.
        let edit = parse_connection("192.168.1.20:9800", "9777", "").unwrap();
        assert_eq!(
            (edit.addr.as_deref(), edit.port),
            (Some("192.168.1.20"), Some(9800))
        );
        assert_eq!(edit.macs, Some(Vec::new()));
        // A bare IPv6 keeps its colons.
        let edit = parse_connection("::1", "9777", "").unwrap();
        assert_eq!((edit.addr.as_deref(), edit.port), (Some("::1"), Some(9777)));
    }

    #[test]
    fn a_refused_value_names_its_row() {
        assert_eq!(failed("  ", "9777", ""), Some(Field::Addr));
        assert_eq!(failed("desk", "0", ""), Some(Field::Port));
        assert_eq!(failed("desk", "70000", ""), Some(Field::Port));
        assert_eq!(failed("desk", "port", ""), Some(Field::Port));
        assert_eq!(failed("desk", "9777", "aa:bb"), Some(Field::Macs));
    }
}
