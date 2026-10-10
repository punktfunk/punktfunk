//! Windows SCM service: a session-0 LocalSystem supervisor that launches two children.
//!
//! The streaming host must run **as SYSTEM in the interactive console session** (session 1+).
//! Capture of the secure (Winlogon/UAC/lock) desktop and `SendInput` both need SYSTEM; capture
//! and injection both need that session, which a plain session-0 service is not in. This process
//! never captures: it duplicates its LocalSystem token, retargets it to the active console
//! session, and `CreateProcessAsUserW`s the host there. The host captures the virtual display
//! in-process via IDD direct-push.
//!
//! The second child is the web management console (bun/Nitro on :47992), spawned plainly into
//! session 0 so a session switch does not tear it down. The seat supervisor ([`seats`]) runs
//! in this process and starts seat hosts in jobs of its own.
//!
//! Subcommands: `run` (SCM binPath), `install`/`uninstall`, `start`/`stop`/`restart`/`status`.
//! Config: `%ProgramData%\punktfunk\host.env`. Logs: `%ProgramData%\punktfunk\logs\`.
//! [`runtime`] supervises both children, [`web`] is the console's slot, [`setup`] installs,
//! [`host_env`] and [`firewall`] are what install writes, and [`rollback`] undoes a bad update.

use crate::install::run_quiet;
use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsString};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    SECURITY_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_DATA, OPEN_ALWAYS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, ResetEvent, ResumeThread, SetEvent, TerminateProcess, WaitForMultipleObjects,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// SCM key under `HKLM\SYSTEM\CurrentControlSet\Services`. Do not rename.
const SERVICE_NAME: &str = "PunktfunkHost";
const SERVICE_DISPLAY: &str = "Punktfunk Host";
const SERVICE_DESCRIPTION: &str =
    "Low-latency desktop/game streaming host. Launches the ordinary host into the active console session.";

/// `PUNKTFUNK_HOST_CMD` when host.env names none. Only a host.env from before the setting has no
/// line; `service install` moves its GameStream into the settings store and writes `serve`.
const DEFAULT_HOST_CMD: &str = "serve --gamestream";

mod firewall;
mod host_env;
mod rollback;
mod runtime;
mod seats;
mod setup;
mod web;

use self::{firewall::*, host_env::*, rollback::*, runtime::*, seats::*, setup::*, web::*};
pub(crate) use firewall::{
    allow_public_network, firewall_profile_arg, fw_add_rule_args, run_netsh,
};
pub use runtime::init_file_logging;

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("run") => run(),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("start") => sc(&["start", SERVICE_NAME]),
        Some("stop") => sc(&["stop", SERVICE_NAME]),
        Some("restart") => restart(),
        Some("status") => sc(&["query", SERVICE_NAME]),
        _ => {
            eprintln!(
                "punktfunk-host service — Windows service control\n\n\
                 USAGE:\n\
                 \x20   punktfunk-host service install [--gamestream=on|off] [--web-bind=ADDR]\n\
                 \x20                                      register the auto-start service + firewall rules\n\
                 \x20                                      (--gamestream sets the console's GameStream setting;\n\
                 \x20                                       --web-bind sets PUNKTFUNK_UI_BIND, the console's\n\
                 \x20                                       address: 127.0.0.1, 0.0.0.0, or one of yours)\n\
                 \x20   punktfunk-host service uninstall   stop + remove the service + firewall rules\n\
                 \x20   punktfunk-host service start       start the service now\n\
                 \x20   punktfunk-host service stop        stop the service\n\
                 \x20   punktfunk-host service restart     stop, wait for exit, start again\n\
                 \x20   punktfunk-host service status      query the service\n\n\
                 Config: %ProgramData%\\punktfunk\\host.env   Logs: %ProgramData%\\punktfunk\\logs\\"
            );
            Ok(())
        }
    }
}
