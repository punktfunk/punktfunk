//! This device: a saved or advertised host whose pinned certificate answers the identity probe
//! at [`LOOPBACK`] on its port. A host that is this device is played here and never streamed.
//!
//! The verdict is per process and never stored: no address match, nothing read from the host's
//! files, nothing written to the known-hosts store. True is kept for the life of the process,
//! so a host that stops is this device, offline. Every management call to this device dials
//! [`LOOPBACK`] (`library::HostApi`), so the host sees a loopback peer.

use punktfunk_core::client::NativeClient;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

/// Where this device's host answers.
pub const LOOPBACK: &str = "127.0.0.1";

/// A host on this machine answers in milliseconds; a miss costs the whole wait.
pub const LOOK_TIMEOUT: Duration = Duration::from_millis(250);

/// Each lowercase fingerprint asked at [`LOOPBACK`] in this process, and whether it answered.
static ASKED: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());

fn asked_map() -> std::sync::MutexGuard<'static, BTreeMap<String, bool>> {
    ASKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the host pinned as `fp_hex` answered at [`LOOPBACK`] as itself in this process.
/// An empty fingerprint is never this device.
pub fn is_this_device(fp_hex: &str) -> bool {
    asked_map().get(&fp_hex.to_ascii_lowercase()) == Some(&true)
}

/// Whether `fp_hex` was asked at [`LOOPBACK`] in this process, answered or not.
fn asked(fp_hex: &str) -> bool {
    asked_map().contains_key(&fp_hex.to_ascii_lowercase())
}

/// Probe each `(port, fingerprint)` at [`LOOPBACK`] in parallel, wall clock about one
/// `timeout`. A host already found and a record with no pin are not asked; a miss is asked
/// again, as the host may have started since.
pub fn look(hosts: &[(u16, String)], timeout: Duration) {
    look_with(hosts, |targets| {
        let asks: Vec<_> = (targets.into_iter())
            .map(|(_, port, fp)| ask(port, fp, timeout))
            .collect();
        asks.into_iter()
            .map(|a| a.recv().unwrap_or(false))
            .collect()
    });
}

/// Whether the host pinned as `fp_hex` answers at [`LOOPBACK`] on `port`, asked on a thread
/// of its own. The answer arrives when the handshake resolves; the thread then drains the
/// connection, whose close is already sent, and ends. A process that exits meanwhile only
/// cuts that drain short.
fn ask(port: u16, fp_hex: String, timeout: Duration) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if nobody_listens(port) {
            let _ = tx.send(false);
            return;
        }
        NativeClient::probe_identity_then(LOOPBACK, port, timeout, |id| {
            let _ = tx.send(super::probe::answered_as_self(&fp_hex, id));
        });
    });
    rx
}

/// How long the OS gets to say nothing listens. On Linux loopback the refusal is back
/// before the send returns.
const REFUSAL_WAIT: Duration = Duration::from_millis(5);

/// The error a connected UDP socket reports once the OS answered port unreachable.
const NOTHING_LISTENS: ErrorKind = if cfg!(windows) {
    ErrorKind::ConnectionReset
} else {
    ErrorKind::ConnectionRefused
};

/// Whether the OS says no socket listens on UDP `port` at [`LOOPBACK`]. It sends an empty
/// datagram, which a host's QUIC endpoint drops unread. Only the port-unreachable error is
/// a no; silence and every other answer are not, and the handshake decides.
fn nobody_listens(port: u16) -> bool {
    let Ok(socket) = UdpSocket::bind((LOOPBACK, 0)) else {
        return false;
    };
    let ready = socket.connect((LOOPBACK, port)).is_ok()
        && socket.set_read_timeout(Some(REFUSAL_WAIT)).is_ok();
    let refused = |r: std::io::Result<usize>| r.is_err_and(|e| e.kind() == NOTHING_LISTENS);
    ready && (refused(socket.send(&[])) || refused(socket.recv(&mut [0; 1])))
}

/// [`look`] for one host this process never asked, blocking for at most [`LOOK_TIMEOUT`]. A
/// host already asked costs nothing.
pub fn look_once(port: u16, fp_hex: &str) {
    if !asked(fp_hex) {
        look(&[(port, fp_hex.to_string())], LOOK_TIMEOUT);
    }
}

/// [`look`] over `probe`, which answers each `(addr, port, fingerprint)` in order.
fn look_with(hosts: &[(u16, String)], probe: impl FnOnce(Vec<(String, u16, String)>) -> Vec<bool>) {
    let mut ask: Vec<(String, u16, String)> = hosts
        .iter()
        .filter(|(_, fp)| !fp.is_empty() && !is_this_device(fp))
        .map(|(port, fp)| (LOOPBACK.to_string(), *port, fp.to_ascii_lowercase()))
        .collect();
    ask.sort();
    ask.dedup();
    if ask.is_empty() {
        return;
    }
    let found = probe(ask.clone());
    let mut asked = asked_map();
    for ((_, _, fp), hit) in ask.into_iter().zip(found) {
        *asked.entry(fp).or_default() |= hit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A port the OS says is closed is a no at once, and `look` keeps it as an asked miss; a
    /// bound socket, which never answers, is left to the handshake.
    #[test]
    fn a_closed_port_is_a_miss_at_once_and_a_bound_one_is_asked() {
        let gone = UdpSocket::bind((LOOPBACK, 0)).unwrap();
        let closed = gone.local_addr().unwrap().port();
        drop(gone);
        let t = Instant::now();
        assert!(nobody_listens(closed));
        look(&[(closed, "local-closed-port".into())], LOOK_TIMEOUT);
        assert!(
            t.elapsed() < Duration::from_millis(150),
            "{:?}",
            t.elapsed()
        );
        assert!(asked("local-closed-port") && !is_this_device("local-closed-port"));

        let bound = UdpSocket::bind((LOOPBACK, 0)).unwrap();
        assert!(!nobody_listens(bound.local_addr().unwrap().port()));
    }

    /// A host that answered once stays this device and is not asked again; a miss stays false
    /// but is known as asked, a host never asked is not, and a record with no pin is never
    /// asked.
    #[test]
    fn a_verdict_is_sticky_and_a_miss_stays_false() {
        let asked_now = std::cell::RefCell::new(Vec::new());
        let probe = |hit: &'static str| {
            let asked_now = &asked_now;
            move |targets: Vec<(String, u16, String)>| {
                asked_now.borrow_mut().extend(targets.iter().cloned());
                targets.iter().map(|t| t.2 == hit).collect()
            }
        };
        let hosts = [
            (9777, "Local-Sticky".to_string()),
            (9777, "local-miss".to_string()),
            (9777, String::new()),
        ];
        assert!(!asked("local-miss"));
        look_with(&hosts, probe("local-sticky"));
        assert!(is_this_device("local-sticky") && is_this_device("LOCAL-STICKY"));
        assert!(!is_this_device("local-miss"));
        assert!(asked("local-miss"), "asked and missed is known");
        assert!(!asked("local-never-asked"));
        assert!(!is_this_device("") && !asked(""));
        assert!(asked_now
            .borrow()
            .iter()
            .all(|(addr, _, fp)| addr == LOOPBACK && !fp.is_empty()));

        asked_now.borrow_mut().clear();
        look_with(&hosts, probe("none"));
        assert!(
            is_this_device("local-sticky"),
            "a stopped host is this device, offline"
        );
        assert_eq!(
            asked_now
                .borrow()
                .iter()
                .map(|t| t.2.as_str())
                .collect::<Vec<_>>(),
            ["local-miss"],
            "only the miss is asked again"
        );
    }
}
