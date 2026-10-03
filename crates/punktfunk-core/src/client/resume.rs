//! `punktfunk/2` sessions this process lost to the network, by the host they were dialed at.
//!
//! The next dial to that host presents the id as `resume`, and the host retires exactly that
//! session instead of waiting out its release. Only a lost session is kept: one still live, or
//! ended on purpose, must never be retired by a second dial such as a speed test.

use std::sync::Mutex;

/// Hosts remembered at once; a dial takes its entry, so this only bounds a long session list.
const KEPT: usize = 16;

static LOST: Mutex<Vec<(String, u16, [u8; 16])>> = Mutex::new(Vec::new());

/// A session to `host:port` ended as [`super::PunktfunkEndReason::Lost`].
pub(crate) fn note_lost(host: &str, port: u16, session_id: [u8; 16]) {
    let mut lost = LOST.lock().unwrap_or_else(|e| e.into_inner());
    lost.retain(|(h, p, _)| !(h == host && *p == port));
    if lost.len() == KEPT {
        lost.remove(0);
    }
    lost.push((host.to_string(), port, session_id));
}

/// The lost session a dial to `host:port` resumes, once.
pub(crate) fn take(host: &str, port: u16) -> Option<[u8; 16]> {
    let mut lost = LOST.lock().unwrap_or_else(|e| e.into_inner());
    let i = lost.iter().position(|(h, p, _)| h == host && *p == port)?;
    Some(lost.remove(i).2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_session_is_resumed_once_by_its_host() {
        note_lost("resume-test.local", 9777, [1; 16]);
        note_lost("resume-test.local", 9777, [2; 16]);
        assert_eq!(
            take("resume-test.local", 9778),
            None,
            "another port is another host"
        );
        assert_eq!(
            take("resume-test.local", 9777),
            Some([2; 16]),
            "the latest loss wins"
        );
        assert_eq!(take("resume-test.local", 9777), None, "taken once");
    }
}
