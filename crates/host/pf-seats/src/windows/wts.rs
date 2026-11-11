//! WTS discovery and managed-session teardown.
//!
//! Enumeration copies session IDs and states before releasing WTS memory, then
//! queries each user and domain through `WTSQuerySessionInformationW`; a session that ends
//! between the two steps is left out. A usable seat session must be active, non-console,
//! local to this computer, and match the complete account name under Windows case rules.
//! Multiple matches are an error; how long to wait for one is the supervisor's. Disconnected
//! sessions remain visible to diagnostics but are never returned as display-capable.
//! Logoff looks the account's session up when it runs, never by an ID recorded earlier: Windows
//! reuses freed IDs.

use crate::windows::util::{backend_error, computer_name, io_error, WinResult};
use windows::core::PWSTR;
use windows::Wdk::System::SystemServices::RtlCompareUnicodeStrings;
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSLogoffSession, WTSQuerySessionInformationW, WTSUserName, WTS_CONNECTSTATE_CLASS,
    WTS_SESSION_INFOW,
};

#[derive(Clone, Debug)]
pub(super) struct Session {
    pub id: u32,
    pub user: String,
    pub domain: String,
    pub state: WTS_CONNECTSTATE_CLASS,
    pub console: bool,
}

pub(super) fn enumerate() -> WinResult<Vec<Session>> {
    let mut buffer: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: null server selects the local machine; both output pointers remain live.
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }
        .map_err(|error| io_error("wts_enumerate", "WTSEnumerateSessionsW failed", error))?;
    if buffer.is_null() && count != 0 {
        return Err(backend_error(
            "wts_enumerate",
            "WTSEnumerateSessionsW returned a null buffer",
        ));
    }
    let raw: Vec<(u32, WTS_CONNECTSTATE_CLASS)> = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: WTS returned `count` consecutive session records in the live allocation.
        unsafe { std::slice::from_raw_parts(buffer, count as usize) }
            .iter()
            .map(|session| (session.SessionId, session.State))
            .collect()
    };
    if !buffer.is_null() {
        // SAFETY: this is the allocation returned by WTSEnumerateSessionsW and is freed once.
        unsafe { WTSFreeMemory(buffer.cast()) };
    }

    // SAFETY: this API takes no pointers and returns the current console session ID by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    // A session that logged off since the listing has nothing to query: it is gone.
    Ok(raw
        .into_iter()
        .filter_map(|(id, state)| {
            let named = query_string(id, WTSUserName)
                .and_then(|user| Ok((user, query_string(id, WTSDomainName)?)));
            match named {
                Ok((user, domain)) => Some(Session {
                    id,
                    user,
                    domain,
                    state,
                    console: id == console,
                }),
                Err(error) => {
                    tracing::debug!(%error, session = id, "session ended during enumeration");
                    None
                }
            }
        })
        .collect())
}

/// Whether this computer's `account` has any session right now: console or not, active or
/// disconnected.
pub(super) fn has_session(account: &str) -> WinResult<bool> {
    let machine = computer_name()?;
    Ok(enumerate()?
        .iter()
        .any(|session| belongs_to(session, account, &machine)))
}

/// Whether `session` is this computer's `account`. Names compare as Windows compares account
/// names: case folded one UTF-16 unit at a time, so `ß` never matches `SS`.
fn belongs_to(session: &Session, account: &str, machine: &str) -> bool {
    let same = |a: &str, b: &str| {
        let (a, b): (Vec<u16>, Vec<u16>) = (a.encode_utf16().collect(), b.encode_utf16().collect());
        // SAFETY: both slices are live for the call; ntdll only reads them.
        unsafe { RtlCompareUnicodeStrings(&a, &b, true) == 0 }
    };
    same(&session.user, account) && (session.domain.is_empty() || same(&session.domain, machine))
}

pub(super) fn active_for_account(account: &str) -> WinResult<Option<Session>> {
    one_account_session(account, true)
}

/// The account's session whatever its state. A logoff removes it; a dropped connection leaves it
/// `Disconnected`, which is the difference between a seat that is gone and one that can be
/// reattached.
pub(super) fn any_for_account(account: &str) -> WinResult<Option<Session>> {
    one_account_session(account, false)
}

/// Logs off every session of `account`, whatever its state, and counts them. None is success.
pub(super) fn logoff_account_if_present(account: &str) -> WinResult<usize> {
    let sessions = sessions_of(account, false)?;
    for session in &sessions {
        logoff(session.id)?;
    }
    Ok(sessions.len())
}

/// The non-console local sessions of `account`, and only the active ones when `active_only`.
fn sessions_of(account: &str, active_only: bool) -> WinResult<Vec<Session>> {
    let machine = computer_name()?;
    Ok(of_account(enumerate()?, account, &machine, active_only))
}

fn of_account(
    sessions: Vec<Session>,
    account: &str,
    machine: &str,
    active_only: bool,
) -> Vec<Session> {
    sessions
        .into_iter()
        .filter(|session| {
            (!active_only || session.state == WTSActive)
                && !session.console
                && belongs_to(session, account, machine)
        })
        .collect()
}

fn one_account_session(account: &str, active_only: bool) -> WinResult<Option<Session>> {
    let mut matches = sessions_of(account, active_only)?;
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        count => Err(backend_error(
            "wts_ambiguous",
            format!("{count} non-console local sessions match account '{account}'"),
        )),
    }
}

fn logoff(session_id: u32) -> WinResult<()> {
    // SAFETY: the ID comes from an enumeration made moments ago; no memory is shared.
    unsafe { WTSLogoffSession(None, session_id, true) }
        .map_err(|error| io_error("wts_logoff", "WTSLogoffSession failed", error))
}

fn query_string(
    session_id: u32,
    class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
) -> WinResult<String> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: both outputs remain live and WTS owns any returned allocation until the free below.
    unsafe { WTSQuerySessionInformationW(None, session_id, class, &mut buffer, &mut bytes) }
        .map_err(|error| io_error("wts_query", "WTSQuerySessionInformationW failed", error))?;
    if buffer.is_null() {
        return Ok(String::new());
    }
    let units = bytes as usize / 2;
    // SAFETY: WTS reports `bytes` readable bytes at `buffer`; the allocation is still live.
    let raw = unsafe { std::slice::from_raw_parts(buffer.0, units) };
    let end = raw.iter().position(|unit| *unit == 0).unwrap_or(raw.len());
    let value = String::from_utf16(&raw[..end])
        .map_err(|error| io_error("wts_query", "WTS string is not valid UTF-16", error));
    // SAFETY: this is the allocation returned by WTSQuerySessionInformationW and is freed once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::RemoteDesktop::WTSDisconnected;

    fn session(id: u32, user: &str, domain: &str, state: WTS_CONNECTSTATE_CLASS) -> Session {
        Session {
            id,
            user: user.into(),
            domain: domain.into(),
            state,
            console: false,
        }
    }

    /// A logoff by account name reaches this computer's sessions of that account, active or
    /// not, and never the console, another account, or a domain account of the same name.
    #[test]
    fn an_account_s_sessions_exclude_the_console_and_strangers() {
        let console = Session {
            console: true,
            ..session(1, "pf-seat-1", "BOX", WTSActive)
        };
        let all = vec![
            console,
            session(2, "pf-seat-1", "BOX", WTSDisconnected),
            session(3, "PF-SEAT-1", "box", WTSActive),
            session(4, "pf-seat-2", "BOX", WTSDisconnected),
            session(5, "pf-seat-1", "CORP", WTSDisconnected),
        ];
        let ids = |active_only| -> Vec<u32> {
            of_account(all.clone(), "pf-seat-1", "BOX", active_only)
                .iter()
                .map(|session| session.id)
                .collect()
        };
        assert_eq!(ids(false), [2, 3]);
        assert_eq!(ids(true), [3]);
    }

    /// A name matches as Windows folds it, one UTF-16 unit at a time, past ASCII too.
    #[test]
    fn an_account_name_folds_case_as_windows_does() {
        for (user, domain, account, machine, same) in [
            ("PF-SEAT-1", "box", "pf-seat-1", "BOX", true),
            ("Ärger", "BOX", "ärger", "BOX", true),
            ("ÄRGER", "", "ärger", "BOX", true),
            ("ärger", "BÜRO", "Ärger", "büro", true),
            ("STRAßE", "BOX", "straße", "BOX", true),
            ("Arger", "BOX", "ärger", "BOX", false),
            ("STRASSE", "BOX", "straße", "BOX", false),
            ("pf-seat-10", "BOX", "pf-seat-1", "BOX", false),
            ("ärger", "CORP", "ärger", "BOX", false),
        ] {
            let found = of_account(
                vec![session(1, user, domain, WTSActive)],
                account,
                machine,
                true,
            );
            assert_eq!(
                found.len() == 1,
                same,
                "{user}@{domain} as {account}@{machine}"
            );
        }
    }
}
