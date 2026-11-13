//! A Windows seat's person pipe, `\\.\pipe\punktfunk-person-<session>`. The seat host is SYSTEM
//! in the seat's session and keeps its person token in memory, so no file is the person's alone.
//! Instead a caller writes [`REQUEST`] here, and one that [`crate::person_token::admits`] reads
//! back a ticket for the seat's own profile, signed with that token. Anyone else is hung up on.
//!
//! The DACL admits SYSTEM and the seat's account, and a medium label keeps a low-integrity
//! process out. The pipe is owned by SYSTEM: a name a user took first is not, and the client
//! ([`ticket`], which `ctl console-url` runs) refuses it.

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, RevertToSelf,
    TokenIntegrityLevel, TokenIsAppContainer, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

/// What a caller writes before it is asked who it is: a pipe server can't read its client's
/// token before it has read from it.
const REQUEST: &[u8] = b"ticket\n";
/// Callers answered at once. A process that connects and never writes holds one.
const MAX_CALLERS: usize = 4;
/// Room for [`REQUEST`] one way and a ticket the other.
const BUFFER_BYTES: u32 = 512;
const LOCAL_SYSTEM: &str = "S-1-5-18";

/// The person pipe of WTS session `session`.
pub(crate) fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\punktfunk-person-{session}")
}

/// Serves this host's session's pipe for its lifetime, signing with `token`.
pub(crate) fn serve(token: String) {
    let spawned = std::thread::Builder::new()
        .name("person-pipe".into())
        .spawn(move || run(&token));
    if let Err(error) = spawned {
        tracing::warn!(%error, "person pipe did not start");
    }
}

fn run(token: &str) {
    let session = match super::interactive::current_process_session_id() {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "person pipe closed: no session");
            return;
        }
    };
    let name = pipe_name(session);
    let mut next = instance_when_ready(&name, true);
    tracing::info!(pipe = %name, "person pipe listening");
    let active = std::sync::Arc::new(AtomicUsize::new(0));
    loop {
        let current = next;
        // SAFETY: `current` is a live pipe handle this loop owns; the call is synchronous.
        let connected = unsafe { ConnectNamedPipe(HANDLE(current.as_raw_handle()), None) };
        // The next instance exists before this one is handed off, so the name is never free.
        next = instance_when_ready(&name, false);
        match connected {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(_) => continue,
        }
        let taken = active.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_CALLERS).then_some(n + 1)
        });
        if taken.is_err() {
            continue;
        }
        let (token, done) = (token.to_string(), active.clone());
        let spawned = std::thread::Builder::new()
            .name("person-pipe-caller".into())
            .spawn(move || {
                answer(std::fs::File::from(current), &token, session);
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("person pipe caller thread did not start");
        }
    }
}

/// An instance of `name` for the account signed in to this session, once one is. `first` claims
/// the name: while another process holds it, this waits.
fn instance_when_ready(name: &str, first: bool) -> OwnedHandle {
    let mut told = false;
    loop {
        let made = super::theme::session_sid()
            .ok_or_else(|| "nobody is signed in to this session".to_string())
            .and_then(|sid| create(name, &sddl("SY", &sid), first));
        match made {
            Ok(instance) => return instance,
            Err(why) if !told => {
                tracing::warn!(pipe = name, %why, "person pipe waits");
                told = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The pipe's descriptor: owned by `owner` (SYSTEM, `SY`, but in tests), SYSTEM may do anything,
/// the seat's account `seat_sid` may read and write, nothing is inherited, and nothing below
/// medium integrity reads or writes.
fn sddl(owner: &str, seat_sid: &str) -> String {
    format!("O:{owner}D:P(A;;GA;;;SY)(A;;FRFW;;;{seat_sid})S:(ML;;NWNR;;;ME)")
}

/// One instance of `name` under `descriptor` (SDDL).
fn create(name: &str, descriptor: &str, first: bool) -> Result<OwnedHandle, String> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let attributes =
        pf_paths_win::SecurityAttributes::new(descriptor).map_err(|e| e.to_string())?;
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: `wide` is NUL-terminated, and `attributes` and the descriptor it points at outlive
    // the call, which copies the descriptor into the pipe.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            0,
            Some(attributes.as_ptr()),
        )
    };
    if handle.is_invalid() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: a valid handle fresh from CreateNamedPipeW, owned here once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// One caller: its request, who it is, and a ticket when the seat admits it.
fn answer(mut pipe: std::fs::File, token: &str, session: u32) {
    let mut request = [0u8; REQUEST.len()];
    if pipe.read_exact(&mut request).is_err() || request != REQUEST {
        return;
    }
    let Some(caller) = caller_of(&pipe) else {
        tracing::info!("person pipe: a caller's token didn't read");
        return;
    };
    let Some(seat_sid) = super::theme::session_sid() else {
        return;
    };
    if !crate::person_token::admits(&caller, &seat_sid, session) {
        tracing::info!(
            sid = %caller.sid,
            session = caller.session,
            integrity = caller.integrity,
            app_container = caller.app_container,
            "person pipe refused a caller"
        );
        return;
    }
    let Some(profile) = crate::library::View::here().layer().map(str::to_string) else {
        tracing::info!("person pipe: this seat names no profile of its own");
        return;
    };
    let ticket = crate::person_token::sign(
        token,
        crate::clock::unix_secs_u64(),
        &crate::mgmt_token::random_token()[..32],
        &profile,
    );
    if pipe.write_all(format!("{ticket}\n").as_bytes()).is_ok() {
        // SAFETY: the handle is live inside `pipe`; the flush returns once the caller has read.
        let _ = unsafe { FlushFileBuffers(HANDLE(pipe.as_raw_handle())) };
    }
}

/// The caller at the other end of `pipe`, from the token it connected with.
fn caller_of(pipe: &std::fs::File) -> Option<crate::person_token::PipeCaller> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut session = 0u32;
    // SAFETY: a live, connected pipe handle and a live out-param.
    unsafe { GetNamedPipeClientSessionId(handle, &mut session) }.ok()?;
    // SAFETY: a live, connected pipe this thread has read from.
    unsafe { ImpersonateNamedPipeClient(handle) }.ok()?;
    let mut token = HANDLE::default();
    // `true`: the token is opened as this process, since an identification-level token can't
    // open anything as the caller.
    // SAFETY: the current-thread pseudo-handle and a live out-param.
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) };
    // SAFETY: ends the impersonation begun above on this thread.
    if unsafe { RevertToSelf() }.is_err() {
        // This thread stays the caller; it ends here, and only `answer`'s return follows.
        return None;
    }
    opened.ok()?;
    // SAFETY: the open succeeded, so `token` is a handle this frame alone owns.
    let token = unsafe { Owned::new(token) };
    Some(crate::person_token::PipeCaller {
        sid: super::theme::token_sid(*token)?,
        session,
        integrity: integrity_of(*token)?,
        app_container: dword(*token, TokenIsAppContainer)? != 0,
    })
}

/// `token`'s mandatory label RID.
fn integrity_of(token: HANDLE) -> Option<u32> {
    let mut len = 0u32;
    // SAFETY: a null buffer with length zero is the documented size probe.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len) };
    // u64 words: the label's SID lies in this buffer and is read through u32 pointers.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` writable bytes, which is what the probe asked for.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: the call wrote a TOKEN_MANDATORY_LABEL at the 8-aligned start of `buf`.
    let sid: PSID = unsafe { (*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    // SAFETY: `sid` points into `buf`, which lives past both reads; the count is read before
    // the index it bounds.
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        let last = u32::from(count.checked_sub(1)?);
        Some(*GetSidSubAuthority(sid, last))
    }
}

/// A DWORD-sized token class of `token`.
fn dword(token: HANDLE, class: windows::Win32::Security::TOKEN_INFORMATION_CLASS) -> Option<u32> {
    let mut value = 0u32;
    let mut len = 0u32;
    // SAFETY: `value` is a live DWORD and the call is told its size.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(std::ptr::addr_of_mut!(value).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut len,
        )
    }
    .ok()?;
    Some(value)
}

/// A ticket from the seat host of this session, for `ctl console-url`.
pub(crate) fn ticket() -> Result<String, String> {
    let session = super::interactive::current_process_session_id()
        .map_err(|e| format!("read this desktop's session: {e:#}"))?;
    let mut pipe = open(&pipe_name(session))?;
    if !owned_by_system(&pipe) {
        return Err("the pipe there is not the seat host's".into());
    }
    pipe.write_all(REQUEST)
        .map_err(|e| format!("ask the seat host: {e}"))?;
    let mut answer = String::new();
    pipe.take(u64::from(BUFFER_BYTES))
        .read_to_string(&mut answer)
        .map_err(|e| format!("read the seat host's answer: {e}"))?;
    let ticket = answer.trim();
    if crate::person_token::parse(ticket).is_none() {
        return Err("the seat host gave no ticket to this account".into());
    }
    Ok(ticket.to_string())
}

/// The client end of `name`. Identification only: whoever serves it can't act as this account.
/// One instance waits at a time, so a busy pipe is tried again for two seconds.
fn open(name: &str) -> Result<std::fs::File, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(name);
        match opened {
            Ok(file) => return Ok(file),
            // ERROR_PIPE_BUSY while another caller holds the waiting instance.
            Err(e) if e.raw_os_error() == Some(231) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err("this desktop is not a seat's, or its host isn't running".into());
            }
            Err(e) => return Err(format!("open {name}: {e}")),
        }
    }
}

/// Whether SYSTEM owns the pipe `file` is connected to. Only SYSTEM can name SYSTEM the owner.
fn owned_by_system(file: &std::fs::File) -> bool {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: a live pipe handle opened with read access, which carries READ_CONTROL; both
    // out-params are live locals.
    let read = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    if read.is_err() {
        return false;
    }
    // SAFETY: the call LocalAlloc'd `descriptor`, which `Owned` frees once; `owner` points into it.
    let _free = unsafe { Owned::new(HLOCAL(descriptor.0)) };
    // SAFETY: `owner` points into `descriptor`, which lives until `_free` drops.
    unsafe { super::theme::sid_string(owner) }.as_deref() == Some(LOCAL_SYSTEM)
}

/// Opens `url` in this account's browser.
pub(crate) fn open_in_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("explorer.exe")
        .arg(url)
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SYSTEM owns it and may do anything; the seat's account reads and writes; nothing else,
    /// and nothing below medium integrity.
    #[test]
    fn the_descriptor_names_system_and_the_seat_alone() {
        assert_eq!(
            sddl("SY", "S-1-5-21-1-2-3-1001"),
            "O:SYD:P(A;;GA;;;SY)(A;;FRFW;;;S-1-5-21-1-2-3-1001)S:(ML;;NWNR;;;ME)"
        );
    }

    /// A real pipe in this test's own session, under this test's own account: the caller it
    /// admits gets a ticket its host redeems, and a caller in the wrong session is hung up on.
    /// The server here can't name SYSTEM its owner, so the client's owner check is skipped.
    #[test]
    fn a_ticket_crosses_the_pipe_to_the_admitted_caller_alone() {
        let session = super::super::interactive::current_process_session_id().unwrap();
        let me = own_sid();
        let ask = |expect_session: u32| {
            let name = format!(
                r"\\.\pipe\pf-person-test-{}-{expect_session}",
                std::process::id()
            );
            let server = create(&name, &sddl(&me, &me), true).unwrap();
            let me = me.clone();
            let serving = std::thread::spawn(move || {
                // SAFETY: a live pipe handle this thread owns.
                let _ = unsafe { ConnectNamedPipe(HANDLE(server.as_raw_handle()), None) };
                let mut pipe = std::fs::File::from(server);
                let mut request = [0u8; REQUEST.len()];
                pipe.read_exact(&mut request).unwrap();
                let caller = caller_of(&pipe).unwrap();
                if crate::person_token::admits(&caller, &me, expect_session) {
                    let t =
                        crate::person_token::sign(TOKEN, 1_000, &"0".repeat(32), "a1b2c3d4e5f6");
                    pipe.write_all(format!("{t}\n").as_bytes()).unwrap();
                }
            });
            let mut pipe = open(&name).unwrap();
            pipe.write_all(REQUEST).unwrap();
            let mut answer = String::new();
            let _ = pipe.read_to_string(&mut answer);
            serving.join().unwrap();
            answer.trim().to_string()
        };
        let got = ask(session);
        let parsed = crate::person_token::parse(&got).expect("a ticket");
        assert!(parsed.signed_by(TOKEN));
        assert_eq!(ask(session + 1), "", "another session's caller");
    }

    /// The pipe's DACL keeps out an account it doesn't name, an administrator included, and a
    /// pipe this account owns is not the seat host's.
    #[test]
    fn another_account_cannot_open_the_pipe() {
        let me = own_sid();
        let name = format!(r"\\.\pipe\pf-person-test-other-{}", std::process::id());
        let theirs = sddl(&me, "S-1-5-21-1-2-3-1001");
        let _server = create(&name, &theirs, true).unwrap();
        let err = open(&name).unwrap_err();
        assert!(err.contains("os error 5"), "{err}");

        let name = format!(r"\\.\pipe\pf-person-test-squat-{}", std::process::id());
        let _server = create(&name, &sddl(&me, &me), true).unwrap();
        assert!(!owned_by_system(&open(&name).unwrap()));
    }

    const TOKEN: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

    fn own_sid() -> String {
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = HANDLE::default();
        // SAFETY: the current-process pseudo-handle and a live out-param.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.unwrap();
        // SAFETY: the open succeeded, so `token` is a handle this frame alone owns.
        let token = unsafe { Owned::new(token) };
        super::super::theme::token_sid(*token).unwrap()
    }
}
