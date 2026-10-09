//! One AppContainer per plugin, `punktfunk.plugin.<id>`. The package SID is a function of the
//! name, so the host (SYSTEM) derives it for a pipe's DACL while the runner's account creates
//! the profile and spawns inside it; a token's package SID says who dialed a pipe.
//!
//! The spawned process inherits this one's stdio and dies with it: the job is kill-on-close
//! and this process holds the only handle, so a runner that kills the helper kills the plugin.

use anyhow::{Context, Result};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows::core::{Owned, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    SetHandleInformation, ERROR_ALREADY_EXISTS, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL,
};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    FreeSid, GetTokenInformation, TokenAppContainerSid, PSID, SECURITY_CAPABILITIES,
    SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, OpenProcessToken, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// `internetClient`: the LAN and beyond. `privateNetworkClientServer` alone reaches nothing.
const CAP_INTERNET_CLIENT: &str = "S-1-15-3-1";
/// `privateNetworkClientServer`, granted beside the one above for a plugin with `network`.
const CAP_PRIVATE_NETWORK: &str = "S-1-15-3-3";

/// `SE_GROUP_ENABLED` on a capability SID.
const CAPABILITY_ENABLED: u32 = 0x4;

/// The profile a plugin runs under.
pub(crate) fn profile_name(id: &str) -> String {
    format!("punktfunk.plugin.{id}")
}

/// The capability SIDs a manifest earns.
pub(crate) fn capabilities(network: bool) -> Vec<&'static str> {
    if network {
        vec![CAP_INTERNET_CLIENT, CAP_PRIVATE_NETWORK]
    } else {
        Vec::new()
    }
}

/// The package SID for `id`, as `S-1-15-2-…`: derived from the name, with no profile needed.
pub(crate) fn package_sid(id: &str) -> Result<String> {
    let name = wide(&profile_name(id));
    // SAFETY: `name` is NUL-terminated; the SID returned is `FreeSid`'d by `sid_string`.
    let sid = unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())) }
        .with_context(|| format!("derive the package SID of {id}"))?;
    sid_string(sid)
}

/// Run `program args…` inside the plugin's container and return its exit code. The profile is
/// created for this account on first use. stdio is inherited.
pub(crate) fn spawn(id: &str, network: bool, program: &Path, args: &[String]) -> Result<u32> {
    let name = wide(&profile_name(id));
    // SAFETY: `name` is NUL-terminated; a SID that comes back is `FreeSid`'d below.
    let created = unsafe {
        CreateAppContainerProfile(
            PCWSTR(name.as_ptr()),
            PCWSTR(name.as_ptr()),
            PCWSTR(name.as_ptr()),
            None,
        )
    };
    let package = match created {
        Ok(sid) => sid,
        Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {
            // SAFETY: as above.
            unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())) }
                .with_context(|| format!("derive the package SID of {id}"))?
        }
        Err(e) => {
            return Err(e).with_context(|| format!("create the AppContainer profile of {id}"))
        }
    };
    let _package = FreeOnDrop(package);
    let mut caps: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    let mut cap_sids: Vec<Owned<HLOCAL>> = Vec::new();
    for cap in capabilities(network) {
        let text = wide(cap);
        let mut sid = PSID::default();
        // SAFETY: `text` is NUL-terminated; `sid` is a LocalAlloc'd SID `Owned` frees after the
        // process creation that reads it.
        unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid) }
            .with_context(|| format!("parse capability {cap}"))?;
        // SAFETY: `sid` is the LocalAlloc'd SID the call just returned; `Owned` frees it once.
        let owned = unsafe { Owned::new(HLOCAL(sid.0)) };
        cap_sids.push(owned);
        caps.push(SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: CAPABILITY_ENABLED,
        });
    }
    // A zero-length array must be a null pointer: a dangling one is ERROR_INVALID_PARAMETER.
    let mut sc = SECURITY_CAPABILITIES {
        AppContainerSid: package,
        Capabilities: if caps.is_empty() {
            std::ptr::null_mut()
        } else {
            caps.as_mut_ptr()
        },
        CapabilityCount: caps.len() as u32,
        Reserved: 0,
    };

    // In the job from creation, so this helper dying at any point (a runner restart's Ctrl+C)
    // takes the plugin with it: an orphan would hold the runner's log open.
    let job = kill_on_close_job()?;
    let jobs = [HANDLE(job.as_raw_handle())];
    let mut bytes = 0usize;
    // SAFETY: the sizing call; it reports the bytes needed through `bytes` and fails by design.
    let _ = unsafe { InitializeProcThreadAttributeList(None, 2, None, &mut bytes) };
    let mut list_buf = vec![0u8; bytes];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buf.as_mut_ptr().cast());
    // SAFETY: `list_buf` is `bytes` long, as the sizing call asked, and outlives the list.
    unsafe { InitializeProcThreadAttributeList(Some(list), 2, None, &mut bytes) }
        .context("InitializeProcThreadAttributeList")?;
    // SAFETY: `list` is initialised; `sc`, the SIDs it points at and `jobs` outlive the
    // process creation.
    let listed = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some((&raw mut sc).cast()),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )
        .and_then(|()| {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                std::mem::size_of_val(&jobs),
                None,
                None,
            )
        })
    };

    // Our own stdio, marked inheritable: what the runner gave this helper, the plugin gets.
    let stdio = [
        HANDLE(std::io::stdin().as_raw_handle()),
        HANDLE(std::io::stdout().as_raw_handle()),
        HANDLE(std::io::stderr().as_raw_handle()),
    ];
    for h in stdio {
        // SAFETY: a live handle of this process; a closed or invalid one fails and is skipped.
        let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) };
    }
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdio[0];
    si.StartupInfo.hStdOutput = stdio[1];
    si.StartupInfo.hStdError = stdio[2];
    si.lpAttributeList = list;
    let mut cmd = wide(&command_line(program, args));
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `cmd` is a NUL-terminated local that outlives the call; `si` is a whole
    // STARTUPINFOEXW whose list is initialised above; `pi` is a live out-param.
    let created = listed.and_then(|()| unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR::null(),
            &si.StartupInfo,
            &mut pi,
        )
    });
    // SAFETY: `list` was initialised above and is not used after this.
    unsafe { DeleteProcThreadAttributeList(list) };
    created.with_context(|| format!("start {} in the container of {id}", program.display()))?;
    // SAFETY: the launch succeeded, so both handles in `pi` are this frame's alone.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(pi.hProcess.0),
            OwnedHandle::from_raw_handle(pi.hThread.0),
        )
    };
    // SAFETY: a live process handle; INFINITE waits for its exit.
    unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), INFINITE) };
    let mut code = 0u32;
    // SAFETY: a live process handle and a live out-param.
    unsafe { GetExitCodeProcess(HANDLE(process.as_raw_handle()), &mut code) }
        .context("GetExitCodeProcess")?;
    Ok(code)
}

/// The package SID of the process at the other end of a connected pipe instance, or `None`
/// for a client that runs in no container, or that cannot be asked.
pub(crate) fn pipe_client_package_sid(pipe: HANDLE) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: a live, connected pipe handle and a live out-param.
    unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.ok()?;
    // SAFETY: the pid the pipe reported; the handle is owned and closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: the open succeeded, so this frame alone owns the handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let mut token = HANDLE::default();
    // SAFETY: a live process handle and a live out-param.
    unsafe { OpenProcessToken(HANDLE(process.as_raw_handle()), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: the open succeeded, so this frame alone owns the token.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    // The class fills the struct and lays the SID it points at right behind it, in this buffer.
    let mut buf = vec![
        0u8;
        std::mem::size_of::<TOKEN_APPCONTAINER_INFORMATION>()
            + SECURITY_MAX_SID_SIZE as usize
    ];
    let mut len = 0u32;
    // SAFETY: `buf` is as long as the call is told; the struct and its SID stay in it below.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenAppContainerSid,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: the call filled the struct at the buffer's start; its SID pointer points into
    // `buf`, which outlives the read below.
    let info = unsafe {
        buf.as_ptr()
            .cast::<TOKEN_APPCONTAINER_INFORMATION>()
            .read_unaligned()
    };
    if info.TokenAppContainer.is_invalid() {
        return None;
    }
    sid_string_borrowed(info.TokenAppContainer).ok()
}

/// A job whose close kills every process in it; this process holds the one handle.
fn kill_on_close_job() -> Result<OwnedHandle> {
    // SAFETY: unnamed, default security; the handle is owned on the next line.
    let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObjectW")?;
    // SAFETY: the handle just created, owned nowhere else.
    let job = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: a live job; `info` is the type the class expects and the size is its own.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("SetInformationJobObject")?;
    Ok(job)
}

/// A SID this module must free, read as `S-1-…`.
fn sid_string(sid: PSID) -> Result<String> {
    let _free = FreeOnDrop(sid);
    sid_string_borrowed(sid)
}

/// A SID someone else owns, read as `S-1-…`.
fn sid_string_borrowed(sid: PSID) -> Result<String> {
    let mut text = PWSTR::null();
    // SAFETY: `sid` is a valid SID for the call; `text` receives a LocalAlloc'd string that
    // `Owned` frees once copied out.
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.context("ConvertSidToStringSidW")?;
    // SAFETY: `text` is the LocalAlloc'd string the call returned; `Owned` frees it once read.
    let _text = unsafe { Owned::new(HLOCAL(text.0.cast())) };
    // SAFETY: `text` is NUL-terminated and still allocated.
    let read = unsafe { text.to_string() };
    read.context("read a SID string")
}

/// A SID from the AppContainer APIs, which `FreeSid` releases.
struct FreeOnDrop(PSID);

impl Drop for FreeOnDrop {
    fn drop(&mut self) {
        // SAFETY: a SID the AppContainer profile APIs allocated; the matching free, once.
        unsafe {
            let _ = FreeSid(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A command line `CommandLineToArgvW` reads back as `program` then `args`.
fn command_line(program: &Path, args: &[String]) -> String {
    std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| quote_arg(&a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One argument in Windows quoting: a run of backslashes before a quote is doubled, the quote
/// escaped; a trailing run is doubled so the closing quote survives.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn arguments_round_trip_the_windows_rules() {
        assert_eq!(super::quote_arg("plain"), "plain");
        assert_eq!(super::quote_arg(""), "\"\"");
        assert_eq!(super::quote_arg("a b"), "\"a b\"");
        assert_eq!(super::quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(
            super::quote_arg(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_eq!(super::quote_arg(r#"back\"slash"#), r#""back\\\"slash""#);
    }
}
