//! Production Windows implementation of the seat platform boundary.
//!
//! Account, credential, WTS, process/job, RDP and supervisor concerns are split
//! into narrow modules. `WindowsBackend` runs inside the punktfunk service and
//! owns only per-instance state. It opens one hardened secret root, takes the
//! host executable from its caller, and delegates each portable command. The
//! `HKLM\SOFTWARE\Punktfunk\Seats` key is the switch: without it no seat
//! starts. Doctor reports prerequisite and ownership evidence but never infers
//! HDR or IDD health from static configuration.

mod accounts;
mod credentials;
pub mod keeper;
pub mod pipe;
mod process;
mod rdp;
mod supervisor;
mod util;
mod wts;

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{Diagnostic, DiagnosticLevel};
use crate::model::{
    Ledger, RuntimeState, RuntimeStatus, Seat, DEFAULT_MGMT_PORTS, DEFAULT_NATIVE_PORTS,
};
use crate::persistence::SecretRoot;
use accounts::AccountManager;
use credentials::CredentialStore;
use std::path::{Path, PathBuf};
use supervisor::Supervisor;
use util::{backend_error, io_error, require_local_system, WinResult};
use util::{port_open, udp_port_owners};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, OSVERSIONINFOW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

/// Whose existence turns seats on and reserves display slots 12 through 15.
const SEATS_KEY: &str = r"SOFTWARE\Punktfunk\Seats";

#[derive(Clone)]
pub struct WindowsBackend {
    root: SecretRoot,
    host_path: PathBuf,
    accounts: AccountManager,
    supervisor: Supervisor,
}

/// Whether the seats key exists, read through the 64-bit registry view.
pub fn seats_enabled() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_QUERY_VALUE | KEY_WOW64_64KEY)
        .is_ok()
}

fn termservice_running() -> WinResult<bool> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| io_error("termservice", "open Service Control Manager", error))?;
    let service = manager
        .open_service("TermService", ServiceAccess::QUERY_STATUS)
        .map_err(|error| io_error("termservice", "open TermService", error))?;
    let status = service
        .query_status()
        .map_err(|error| io_error("termservice", "query TermService", error))?;
    Ok(status.current_state == ServiceState::Running)
}

impl WindowsBackend {
    /// `host_path` is the absolute `punktfunk-host.exe` each seat runs, the service's own.
    pub fn open(root: impl AsRef<Path>, host_path: PathBuf) -> Result<Self, BackendError> {
        if !host_path.is_absolute() {
            return Err(backend_error(
                "host_path_invalid",
                "host executable path is not absolute",
            ));
        }
        let root = SecretRoot::open(root)
            .map_err(|error| io_error("seat_root", "open hardened seat root", error))?;
        let credentials = CredentialStore::new(root.clone());
        let accounts = AccountManager::new(credentials);
        let supervisor = Supervisor::new(root.clone(), accounts.clone(), host_path.clone());
        Ok(Self {
            root,
            host_path,
            accounts,
            supervisor,
        })
    }

    pub fn stop_all(&self) {
        self.supervisor.stop_all();
    }

    fn ensure_runtime_prerequisites(&self) -> WinResult<()> {
        require_local_system()?;
        if !seats_enabled() {
            return Err(backend_error(
                "seats_off",
                r"HKLM\SOFTWARE\Punktfunk\Seats is missing; seats are off on this host",
            ));
        }
        if !termservice_running()? {
            return Err(backend_error(
                "termservice_stopped",
                "TermService is not running",
            ));
        }
        Ok(())
    }
}

impl PlatformBackend for WindowsBackend {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.ensure_runtime_prerequisites()?;
        self.accounts.provision(seat)
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.ensure_runtime_prerequisites()?;
        self.supervisor.start(seat)
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.supervisor.stop(seat)
    }

    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        let _ = self.supervisor.stop(seat)?;
        self.accounts.delete(seat)
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(self.supervisor.status(seat).status)
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(self.doctor_report(ledger))
    }
}

impl WindowsBackend {
    fn doctor_report(&self, ledger: &Ledger) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        match windows_build() {
            Ok(build) if build >= 22621 => diagnostics.push(Diagnostic::info(
                "windows_build",
                format!("Windows build {build} meets the 22621 minimum"),
            )),
            Ok(build) => diagnostics.push(Diagnostic::error(
                "windows_build",
                format!("Windows build {build} is below the 22621 minimum"),
            )),
            Err(error) => diagnostics.push(Diagnostic::error("windows_build", error.to_string())),
        }
        match native_x64() {
            Ok(true) => diagnostics.push(Diagnostic::info("architecture", "native OS is x64")),
            Ok(false) => {
                diagnostics.push(Diagnostic::error("architecture", "native OS is not x64"))
            }
            Err(error) => diagnostics.push(Diagnostic::error("architecture", error.to_string())),
        }
        match util::is_local_system() {
            Ok(true) => diagnostics.push(Diagnostic::info(
                "identity",
                "service process is LocalSystem",
            )),
            Ok(false) => diagnostics.push(Diagnostic::error(
                "identity",
                "service process is not LocalSystem",
            )),
            Err(error) => diagnostics.push(Diagnostic::error("identity", error.to_string())),
        }
        if seats_enabled() {
            diagnostics.push(Diagnostic::info(
                "reservation",
                r"HKLM\SOFTWARE\Punktfunk\Seats reserves slots 12 through 15",
            ));
        } else {
            diagnostics.push(Diagnostic::error(
                "reservation",
                "Windows display-slot reservation marker is missing",
            ));
        }
        if self.host_path.is_file() {
            diagnostics.push(Diagnostic::info(
                "host_executable",
                format!("host executable exists at {}", self.host_path.display()),
            ));
        } else {
            diagnostics.push(Diagnostic::error(
                "host_executable",
                format!("host executable is missing at {}", self.host_path.display()),
            ));
        }
        match rdp::load_pin_optional(&self.root) {
            Ok(Some(pin)) => diagnostics.push(Diagnostic::info(
                "rdp_pin",
                format!("trusted RDP leaf SHA-256 is {}", hex::encode(pin)),
            )),
            Ok(None) => diagnostics.push(Diagnostic::error(
                "rdp_pin",
                "RDP certificate pin is missing",
            )),
            Err(error) => diagnostics.push(Diagnostic::error("rdp_pin", error.to_string())),
        }
        match termservice_running() {
            Ok(true) => diagnostics.push(Diagnostic::info("termservice", "TermService is running")),
            Ok(false) => {
                diagnostics.push(Diagnostic::error("termservice", "TermService is stopped"))
            }
            Err(error) => diagnostics.push(Diagnostic::error("termservice", error.to_string())),
        }
        diagnostics.push(Diagnostic::error(
            "session_provider",
            "no session provider is built yet, so this host cannot mint a seat session",
        ));
        for seat in &ledger.seats {
            self.doctor_seat(seat, &mut diagnostics);
        }
        diagnostics
    }

    fn doctor_seat(&self, seat: &Seat, diagnostics: &mut Vec<Diagnostic>) {
        let index = usize::from(seat.display_slot.saturating_sub(12));
        let resources_match = DEFAULT_NATIVE_PORTS.get(index) == Some(&seat.native_port)
            && DEFAULT_MGMT_PORTS.get(index) == Some(&seat.mgmt_port);
        diagnostics.push(seat_diagnostic(
            if resources_match {
                DiagnosticLevel::Info
            } else {
                DiagnosticLevel::Error
            },
            "seat_resources",
            format!(
                "slot {} uses native {} and management {}",
                seat.display_slot, seat.native_port, seat.mgmt_port
            ),
            seat,
        ));
        match self.accounts.inspect(seat) {
            Ok(inspection) => {
                diagnostics.push(seat_diagnostic(
                    if inspection.marker_matches {
                        DiagnosticLevel::Info
                    } else {
                        DiagnosticLevel::Error
                    },
                    "account_marker",
                    if inspection.marker_matches {
                        "account marker matches the exact seat ID".into()
                    } else {
                        "account is absent or its marker does not match".into()
                    },
                    seat,
                ));
                diagnostics.push(seat_diagnostic(
                    if inspection.credential_blob {
                        DiagnosticLevel::Info
                    } else {
                        DiagnosticLevel::Error
                    },
                    "credential_blob",
                    if inspection.credential_blob {
                        "DPAPI credential blob exists".into()
                    } else {
                        "DPAPI credential blob is missing".into()
                    },
                    seat,
                ));
                let policy_ok = inspection.rdp_member
                    && !inspection.administrator
                    && inspection.deny_console
                    && !inspection.deny_remote;
                diagnostics.push(seat_diagnostic(
                    if policy_ok {
                        DiagnosticLevel::Info
                    } else {
                        DiagnosticLevel::Error
                    },
                    "account_policy",
                    format!(
                        "rdp_member={} administrator={} deny_console={} deny_remote={}",
                        inspection.rdp_member,
                        inspection.administrator,
                        inspection.deny_console,
                        inspection.deny_remote
                    ),
                    seat,
                ));
            }
            Err(error) => diagnostics.push(seat_diagnostic(
                DiagnosticLevel::Error,
                "account_query",
                error.to_string(),
                seat,
            )),
        }
        let snapshot = self.supervisor.status(seat);
        diagnostics.push(seat_diagnostic(
            if snapshot.status.state == RuntimeState::Failed {
                DiagnosticLevel::Error
            } else {
                DiagnosticLevel::Info
            },
            "child_state",
            format!(
                "state={:?} session={:?} keeper_pid={:?} host_pid={:?}",
                snapshot.status.state, snapshot.session_id, snapshot.keeper_pid, snapshot.host_pid
            ),
            seat,
        ));
        for (kind, port) in [("native", seat.native_port), ("management", seat.mgmt_port)] {
            let open = if kind == "native" {
                !udp_port_owners(port).is_empty()
            } else {
                port_open(port)
            };
            let expected = snapshot.status.state == RuntimeState::Running;
            diagnostics.push(seat_diagnostic(
                if open == expected {
                    DiagnosticLevel::Info
                } else {
                    DiagnosticLevel::Warning
                },
                "seat_port",
                format!(
                    "{kind} port {port} is {}",
                    if open { "open" } else { "closed" }
                ),
                seat,
            ));
        }
    }
}

fn windows_build() -> WinResult<u32> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `version` is the initialized, correctly sized output structure required by ntdll.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.is_ok() {
        Ok(version.dwBuildNumber)
    } else {
        Err(io_error(
            "windows_build",
            "RtlGetVersion failed",
            std::io::Error::from_raw_os_error(status.0),
        ))
    }
}

fn native_x64() -> WinResult<bool> {
    let mut process_machine = IMAGE_FILE_MACHINE::default();
    let mut native_machine = IMAGE_FILE_MACHINE::default();
    // SAFETY: current-process pseudo-handle is valid and both machine outputs remain writable.
    unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            Some(&mut native_machine),
        )
    }
    .map_err(|error| io_error("architecture", "IsWow64Process2 failed", error))?;
    Ok(native_machine == IMAGE_FILE_MACHINE_AMD64)
}

fn seat_diagnostic(level: DiagnosticLevel, code: &str, message: String, seat: &Seat) -> Diagnostic {
    Diagnostic {
        level,
        code: code.into(),
        message,
        seat_id: Some(seat.id.clone()),
    }
}
