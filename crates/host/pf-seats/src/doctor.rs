//! The Windows seat doctor's rows, decided from facts the shell has already read.
//!
//! `WindowsBackend::doctor_report` probes the box (registry, services, files, ports) into
//! one [`BoxFacts`] and one [`SeatFacts`] per seat. [`report_rows`] and [`seat_rows`] turn
//! them into [`Diagnostic`]s: no I/O and no OS types, so the tables run on every target.
//! Every row is one plain sentence; a problem names the next move.

use crate::ipc::{Diagnostic, DiagnosticLevel};
use crate::model::{RuntimeState, RuntimeStatus, Seat, DEFAULT_MGMT_PORTS, DEFAULT_NATIVE_PORTS};
use std::path::{Path, PathBuf};

/// Where the seats display package stands against the one staged beside the host.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DriverState {
    Current,
    /// In the store, but not the staged version: an upgrade the store hasn't taken yet.
    Outdated,
    Missing,
    /// No package beside the host to compare with.
    Unstaged,
}

/// A seat's Windows account, as far as the doctor looks.
pub(crate) struct AccountInspection {
    /// The account is the row's own: a made row's carries its comment marker, an adopted row's
    /// name resolves to the SID the row stores.
    pub identity_matches: bool,
    pub credential_blob: bool,
    pub rdp_member: bool,
    pub administrator: bool,
    pub deny_console: bool,
    pub deny_remote: bool,
    pub seats_member: bool,
}

/// What the doctor read of the box.
pub(crate) struct BoxFacts {
    /// The Windows build number, or why it couldn't be read.
    pub build: Result<u32, String>,
    /// Whether the PC is x64, or why that couldn't be read.
    pub x64: Result<bool, String>,
    pub local_system: bool,
    pub seats_on: bool,
    /// `Some(true)` on Windows Server; `None` when Windows doesn't say.
    pub server: Option<bool>,
    /// A Remote Desktop wrapper is installed. Read only on a desktop edition.
    pub session_wrapper: bool,
    /// What turning seats on checks (role, GPU, ...).
    pub checks: Vec<Diagnostic>,
    /// The licensing row. Read only on Windows Server.
    pub licensing: Option<Diagnostic>,
    pub termservice: bool,
    /// The first of the host and keeper executables that isn't on disk.
    pub missing_file: Option<PathBuf>,
    pub driver: DriverState,
    pub inbox_driver_newer: Option<bool>,
    /// `None` while no certificate pin is recorded; otherwise whether it matches the live one.
    pub pin: Option<Result<(), String>>,
    pub steam_box: Option<PathBuf>,
    /// Where Steam registers itself. Read only when `steam_box` is `None`.
    pub steam_registered: Option<PathBuf>,
    pub seat_sound: bool,
    pub has_seats: bool,
}

/// What the doctor read of one seat.
pub(crate) struct SeatFacts {
    /// The inspection, or why the account couldn't be read.
    pub account: Result<AccountInspection, String>,
    /// The local group the account row names.
    pub group: &'static str,
    pub box_steam: bool,
    pub seat_steam: bool,
    pub status: RuntimeStatus,
    pub session_id: Option<u32>,
    pub native_open: bool,
    pub mgmt_open: bool,
}

/// The box's rows, in the order the console shows them.
pub(crate) fn report_rows(facts: BoxFacts) -> Vec<Diagnostic> {
    let mut rows = vec![build_row(&facts.build)];
    match &facts.x64 {
        Ok(true) => {}
        Ok(false) => rows.push(Diagnostic::error("architecture", "Seats need an x64 PC.")),
        Err(cause) => rows.push(Diagnostic::warning(
            "architecture",
            format!("Couldn't tell whether this PC is x64 — {cause}. Seats need an x64 PC."),
        )),
    }
    if !facts.local_system {
        rows.push(Diagnostic::error(
            "identity",
            "punktfunk's service isn't running as Local System. Reinstall punktfunk.",
        ));
    }
    rows.push(if facts.seats_on {
        Diagnostic::info("reservation", "Seats are on.")
    } else {
        Diagnostic::error(
            "reservation",
            "Seats are off. Turn them on under Profiles → Seats.",
        )
    });
    rows.push(session_row(facts.server, facts.session_wrapper));
    rows.extend(
        facts
            .checks
            .into_iter()
            .filter(|check| check.code != "windows_server"),
    );
    rows.extend(facts.licensing);
    rows.push(if facts.termservice {
        Diagnostic::info("termservice", "Remote Desktop Services is running.")
    } else {
        Diagnostic::error(
            "termservice",
            "Remote Desktop Services isn't running. Start it under Services, or restart the PC.",
        )
    });
    if let Some(missing) = &facts.missing_file {
        rows.push(Diagnostic::error(
            "host_executable",
            format!(
                "A punktfunk file is missing ({}). Reinstall punktfunk.",
                missing.display()
            ),
        ));
    }
    rows.push(driver_row(&facts.driver));
    if facts.inbox_driver_newer == Some(true) {
        rows.push(Diagnostic::error(
            "seat_display_rank",
            "Windows' own Remote Desktop display driver is newer than punktfunk's, so seats \
             start without a picture. Update punktfunk.",
        ));
    }
    rows.push(pin_row(facts.pin));
    rows.push(steam_row(
        facts.steam_box.as_deref(),
        facts.steam_registered.as_deref(),
    ));
    if facts.has_seats && !facts.seat_sound {
        rows.push(Diagnostic::warning(
            "seat_audio",
            "Seats stream without sound: Steam's streaming audio driver isn't on this PC. \
             Install Steam, then restart the seats.",
        ));
    }
    rows
}

fn build_row(build: &Result<u32, String>) -> Diagnostic {
    match build {
        Ok(build) if *build >= 22621 => Diagnostic::info(
            "windows_build",
            format!("This Windows is new enough for seats (build {build})."),
        ),
        Ok(build) => Diagnostic::error(
            "windows_build",
            format!(
                "This Windows is too old for seats (build {build}). Seats need Windows Server 2025 or newer."
            ),
        ),
        Err(message) => Diagnostic::error(
            "windows_build",
            format!("Couldn't read the Windows version — {message}."),
        ),
    }
}

fn session_row(server: Option<bool>, wrapper: bool) -> Diagnostic {
    match server {
        Some(true) => Diagnostic::info(
            "session_provider",
            "Windows Server gives each seat a desktop of its own.",
        ),
        Some(false) if wrapper => Diagnostic::warning(
            "session_provider",
            "A Remote Desktop wrapper gives each seat a desktop on this edition of Windows. \
             Only Windows Server is supported.",
        ),
        Some(false) => Diagnostic::error(
            "session_provider",
            "This edition of Windows runs one desktop at a time, so seats can't start. \
             Seats need Windows Server.",
        ),
        None => Diagnostic::error(
            "session_provider",
            "Couldn't tell which edition of Windows this is.",
        ),
    }
}

fn driver_row(driver: &DriverState) -> Diagnostic {
    match driver {
        DriverState::Current => {
            Diagnostic::info("seat_display_driver", "The seat display driver is installed.")
        }
        DriverState::Outdated => Diagnostic::warning(
            "seat_display_driver",
            "The seat display driver is older than this punktfunk. Seats update it when they next start.",
        ),
        DriverState::Missing => Diagnostic::error(
            "seat_display_driver",
            "The seat display driver isn't installed. Seats put it back when they next start; \
             if this stays, turn seats off and on under Profiles → Seats.",
        ),
        DriverState::Unstaged => Diagnostic::error(
            "seat_display_driver",
            "The seat display driver isn't on this PC. Reinstall punktfunk.",
        ),
    }
}

fn pin_row(pin: Option<Result<(), String>>) -> Diagnostic {
    match pin {
        Some(Ok(())) => Diagnostic::info("rdp_pin", "Remote Desktop's certificate is recorded."),
        Some(Err(message)) => Diagnostic::error("rdp_pin", message),
        None => Diagnostic::error(
            "rdp_pin",
            "Remote Desktop's certificate isn't recorded, so seats can't sign in. \
             Turn seats off and on under Profiles → Seats.",
        ),
    }
}

/// Where the box's Steam is, as seats see it. Seats copy only a Steam in its usual folder.
fn steam_row(installed: Option<&Path>, registered: Option<&Path>) -> Diagnostic {
    if let Some(dir) = installed {
        return Diagnostic::info(
            "steam",
            format!(
                "Steam is installed in {}. Each seat gets a copy of its own.",
                dir.display()
            ),
        );
    }
    match registered {
        Some(dir) => Diagnostic::warning(
            "steam",
            format!(
                "Steam is installed in {}, so seats get no Steam. Seats copy Steam only from \
                 C:\\Program Files (x86)\\Steam; reinstall it there.",
                dir.display()
            ),
        ),
        None => Diagnostic::info("steam", "Steam isn't installed, so seats have no Steam."),
    }
}

/// One seat's rows: its resources, account, Steam, state and ports.
pub(crate) fn seat_rows(seat: &Seat, facts: &SeatFacts) -> Vec<Diagnostic> {
    let mut rows = Vec::new();
    rows.extend(resources_row(seat));
    rows.push(match &facts.account {
        Ok(inspection) => account_row(seat, inspection, facts.group),
        Err(message) => seat_diagnostic(
            DiagnosticLevel::Error,
            "account",
            format!(
                "Couldn't read its Windows account {} — {}.",
                seat.account, message
            ),
            seat,
        ),
    });
    rows.push(steam_client_row(seat, facts.box_steam, facts.seat_steam));
    rows.extend(state_row(seat, &facts.status, facts.session_id));
    rows.extend(port_row(
        seat,
        &facts.status.state,
        facts.native_open,
        facts.mgmt_open,
    ));
    rows
}

/// The seat's display slot and ports are the ones its place in the pool gets.
fn resources_row(seat: &Seat) -> Option<Diagnostic> {
    let index = usize::from(seat.display_slot.saturating_sub(12));
    (DEFAULT_NATIVE_PORTS.get(index) != Some(&seat.native_port)
        || DEFAULT_MGMT_PORTS.get(index) != Some(&seat.mgmt_port))
    .then(|| {
        seat_diagnostic(
            DiagnosticLevel::Error,
            "seat_resources",
            format!(
                "Its display slot and ports don't match ({}, {}, {}). Remove the profile and add it again.",
                seat.display_slot, seat.native_port, seat.mgmt_port
            ),
            seat,
        )
    })
}

/// The seat's Windows account as one row: fine, or the first thing wrong with it. An adopted
/// account is a person's own: a lost password is typed again on the card, and the fine row says
/// nobody signs in to it at the PC meanwhile.
fn account_row(seat: &Seat, inspection: &AccountInspection, group: &str) -> Diagnostic {
    let account = &seat.account;
    let adopted = seat.adopted_sid.is_some();
    let problem = if !inspection.identity_matches {
        Some(if adopted {
            format!(
                "Its Windows account {account} is gone, or was made again. Remove the profile and add it again."
            )
        } else {
            format!(
                "Its Windows account {account} is missing or isn't punktfunk's. Remove the profile and add it again."
            )
        })
    } else if !inspection.credential_blob {
        Some(if adopted {
            format!(
                "punktfunk has lost {account}'s password. Enter it again on the profile's card."
            )
        } else {
            format!(
                "punktfunk has lost the password of its Windows account {account}. Remove the profile and add it again."
            )
        })
    } else if inspection.administrator {
        Some(format!(
            "Its Windows account {account} is an administrator, which a seat must not be. \
             Remove it from Administrators."
        ))
    } else if !inspection.rdp_member || !inspection.deny_console || inspection.deny_remote {
        Some(format!(
            "Its Windows account {account} lost a right it needs to sign in to its seat. \
             Remove the profile and add it again."
        ))
    } else {
        None
    };
    match problem {
        Some(problem) => seat_diagnostic(DiagnosticLevel::Error, "account", problem, seat),
        None if !inspection.seats_member => seat_diagnostic(
            DiagnosticLevel::Warning,
            "account",
            format!(
                "Its Windows account {account} left the {group} group. It's put back when the seat next starts."
            ),
            seat,
        ),
        None if adopted => seat_diagnostic(
            DiagnosticLevel::Info,
            "account",
            format!(
                "Its Windows account {account} is set up. While it's a seat, nobody signs in to it at this PC."
            ),
            seat,
        ),
        None => seat_diagnostic(
            DiagnosticLevel::Info,
            "account",
            format!("Its Windows account {account} is set up."),
            seat,
        ),
    }
}

fn steam_client_row(seat: &Seat, box_steam: bool, seat_steam: bool) -> Diagnostic {
    let (level, message) = match (box_steam, seat_steam) {
        (false, _) => (DiagnosticLevel::Info, "No Steam: the PC has none to copy."),
        (true, true) => (
            DiagnosticLevel::Info,
            "Has its own Steam. Steam opened on this seat starts that copy.",
        ),
        (true, false) => (
            DiagnosticLevel::Warning,
            "Its own Steam isn't set up yet. It's copied the first time the seat starts; \
             Steam games start on the seat once that's done.",
        ),
    };
    seat_diagnostic(level, "steam_client", message.into(), seat)
}

fn state_row(seat: &Seat, status: &RuntimeStatus, session_id: Option<u32>) -> Option<Diagnostic> {
    let message = match status.state {
        RuntimeState::Running => match session_id {
            Some(id) => format!("Running, in Windows session {id}."),
            None => "Running.".into(),
        },
        RuntimeState::Starting => status.detail.clone().unwrap_or_else(|| "Starting…".into()),
        RuntimeState::Stopped | RuntimeState::Unknown => {
            "Stopped. It starts when its player connects.".into()
        }
        RuntimeState::Stopping => "Stopping…".into(),
        // The service's own `runtime_failed` row says why.
        RuntimeState::Failed => return None,
    };
    Some(seat_diagnostic(
        DiagnosticLevel::Info,
        "child_state",
        message,
        seat,
    ))
}

/// A running seat with a port closed, or a port held while the seat is idle. A seat that is
/// starting or stopping holds its ports on the way.
fn port_row(
    seat: &Seat,
    state: &RuntimeState,
    native_open: bool,
    mgmt_open: bool,
) -> Option<Diagnostic> {
    let running = *state == RuntimeState::Running;
    let idle = matches!(
        state,
        RuntimeState::Stopped | RuntimeState::Unknown | RuntimeState::Failed
    );
    let message = if running && !(native_open && mgmt_open) {
        format!(
            "It's running, but its ports {} and {} aren't both open. Stop and start the seat.",
            seat.native_port, seat.mgmt_port
        )
    } else if idle && (native_open || mgmt_open) {
        let port = if native_open {
            seat.native_port
        } else {
            seat.mgmt_port
        };
        format!("Another program holds its port {port}, so the seat can't start. Restart the PC.")
    } else {
        return None;
    };
    Some(seat_diagnostic(
        DiagnosticLevel::Warning,
        "seat_port",
        message,
        seat,
    ))
}

fn seat_diagnostic(level: DiagnosticLevel, code: &str, message: String, seat: &Seat) -> Diagnostic {
    Diagnostic {
        level,
        code: code.into(),
        message,
        seat_id: Some(seat.id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SeatId;
    use DiagnosticLevel::{Error, Info, Warning};

    fn healthy() -> BoxFacts {
        BoxFacts {
            build: Ok(26100),
            x64: Ok(true),
            local_system: true,
            seats_on: true,
            server: Some(true),
            session_wrapper: false,
            checks: Vec::new(),
            licensing: None,
            termservice: true,
            missing_file: None,
            driver: DriverState::Current,
            inbox_driver_newer: Some(false),
            pin: Some(Ok(())),
            steam_box: Some(PathBuf::from(r"C:\Program Files (x86)\Steam")),
            steam_registered: None,
            seat_sound: true,
            has_seats: true,
        }
    }

    /// The one box row with `code`.
    fn pick(rows: &[Diagnostic], code: &str) -> (DiagnosticLevel, String) {
        let mut found = rows.iter().filter(|row| row.code == code);
        let row = found
            .next()
            .unwrap_or_else(|| panic!("no {code} row in {rows:?}"));
        assert!(found.next().is_none(), "two {code} rows");
        assert_eq!(row.seat_id, None);
        (row.level, row.message.clone())
    }

    fn codes(rows: &[Diagnostic]) -> Vec<&str> {
        rows.iter().map(|row| row.code.as_str()).collect()
    }

    fn shown(rows: &[Diagnostic]) -> Vec<(DiagnosticLevel, &str, &str)> {
        rows.iter()
            .map(|row| (row.level, row.code.as_str(), row.message.as_str()))
            .collect()
    }

    #[test]
    fn a_healthy_box_reads_info_throughout() {
        let rows = report_rows(healthy());
        assert_eq!(
            shown(&rows),
            [
                (
                    Info,
                    "windows_build",
                    "This Windows is new enough for seats (build 26100)."
                ),
                (Info, "reservation", "Seats are on."),
                (
                    Info,
                    "session_provider",
                    "Windows Server gives each seat a desktop of its own."
                ),
                (Info, "termservice", "Remote Desktop Services is running."),
                (
                    Info,
                    "seat_display_driver",
                    "The seat display driver is installed."
                ),
                (Info, "rdp_pin", "Remote Desktop's certificate is recorded."),
                (
                    Info,
                    "steam",
                    r"Steam is installed in C:\Program Files (x86)\Steam. Each seat gets a copy of its own."
                ),
            ]
        );
    }

    #[test]
    fn the_build_row_follows_the_windows_build() {
        let row = |build| {
            pick(
                &report_rows(BoxFacts { build, ..healthy() }),
                "windows_build",
            )
        };
        assert_eq!(
            row(Ok(22621)),
            (
                Info,
                "This Windows is new enough for seats (build 22621).".into()
            )
        );
        assert_eq!(
            row(Ok(22620)),
            (
                Error,
                "This Windows is too old for seats (build 22620). Seats need Windows Server 2025 or newer."
                    .into()
            )
        );
        assert_eq!(
            row(Err("RtlGetVersion failed".into())),
            (
                Error,
                "Couldn't read the Windows version — RtlGetVersion failed.".into()
            )
        );
    }

    #[test]
    fn a_wrong_machine_or_identity_adds_its_row() {
        let rows = report_rows(BoxFacts {
            x64: Ok(false),
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "architecture"),
            (Error, "Seats need an x64 PC.".into())
        );
        // A probe that failed is not a verdict on the PC.
        let rows = report_rows(BoxFacts {
            x64: Err("IsWow64Process2 failed: access denied".into()),
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "architecture"),
            (
                Warning,
                "Couldn't tell whether this PC is x64 — IsWow64Process2 failed: access denied. \
                 Seats need an x64 PC."
                    .into()
            )
        );
        let rows = report_rows(BoxFacts {
            local_system: false,
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "identity"),
            (
                Error,
                "punktfunk's service isn't running as Local System. Reinstall punktfunk.".into()
            )
        );
        let rows = report_rows(healthy());
        assert!(!codes(&rows).contains(&"architecture"));
        assert!(!codes(&rows).contains(&"identity"));
    }

    #[test]
    fn the_reservation_row_follows_the_switch() {
        let rows = report_rows(BoxFacts {
            seats_on: false,
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "reservation"),
            (
                Error,
                "Seats are off. Turn them on under Profiles → Seats.".into()
            )
        );
    }

    #[test]
    fn the_session_row_follows_the_edition() {
        let row = |server, session_wrapper| {
            let facts = BoxFacts {
                server,
                session_wrapper,
                ..healthy()
            };
            pick(&report_rows(facts), "session_provider")
        };
        assert_eq!(
            row(Some(true), false),
            (
                Info,
                "Windows Server gives each seat a desktop of its own.".into()
            )
        );
        assert_eq!(
            row(Some(false), true),
            (
                Warning,
                "A Remote Desktop wrapper gives each seat a desktop on this edition of Windows. \
                 Only Windows Server is supported."
                    .into()
            )
        );
        assert_eq!(
            row(Some(false), false),
            (
                Error,
                "This edition of Windows runs one desktop at a time, so seats can't start. \
                 Seats need Windows Server."
                    .into()
            )
        );
        assert_eq!(
            row(None, false),
            (
                Error,
                "Couldn't tell which edition of Windows this is.".into()
            )
        );
    }

    /// The turn-on checks and the licensing row sit between the session and TermService rows,
    /// and the `windows_server` check is the session row's twin.
    #[test]
    fn the_checks_and_licensing_follow_the_session_row() {
        let facts = BoxFacts {
            checks: vec![
                Diagnostic::info("rds_role", "role"),
                Diagnostic::error("windows_server", "twin"),
                Diagnostic::warning("gpu", "gpu"),
            ],
            licensing: Some(Diagnostic::warning("rds_licensing", "licensing")),
            ..healthy()
        };
        assert_eq!(
            codes(&report_rows(facts)),
            [
                "windows_build",
                "reservation",
                "session_provider",
                "rds_role",
                "gpu",
                "rds_licensing",
                "termservice",
                "seat_display_driver",
                "rdp_pin",
                "steam"
            ]
        );
    }

    #[test]
    fn a_stopped_termservice_is_an_error() {
        let rows = report_rows(BoxFacts {
            termservice: false,
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "termservice"),
            (
                Error,
                "Remote Desktop Services isn't running. Start it under Services, or restart the PC."
                    .into()
            )
        );
    }

    #[test]
    fn a_missing_file_is_named() {
        let rows = report_rows(BoxFacts {
            missing_file: Some(PathBuf::from("pf-keeper.exe")),
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "host_executable"),
            (
                Error,
                "A punktfunk file is missing (pf-keeper.exe). Reinstall punktfunk.".into()
            )
        );
    }

    #[test]
    fn the_driver_row_follows_the_driver_state() {
        let row = |driver| {
            pick(
                &report_rows(BoxFacts {
                    driver,
                    ..healthy()
                }),
                "seat_display_driver",
            )
        };
        assert_eq!(
            row(DriverState::Current),
            (Info, "The seat display driver is installed.".into())
        );
        assert_eq!(
            row(DriverState::Outdated),
            (
                Warning,
                "The seat display driver is older than this punktfunk. Seats update it when they next start."
                    .into()
            )
        );
        assert_eq!(
            row(DriverState::Missing),
            (
                Error,
                "The seat display driver isn't installed. Seats put it back when they next start; \
                 if this stays, turn seats off and on under Profiles → Seats."
                    .into()
            )
        );
        assert_eq!(
            row(DriverState::Unstaged),
            (
                Error,
                "The seat display driver isn't on this PC. Reinstall punktfunk.".into()
            )
        );
    }

    /// Only a newer inbox driver adds the rank row; an unreadable date adds nothing.
    #[test]
    fn a_newer_inbox_driver_adds_the_rank_row() {
        for (newer, added) in [(Some(true), true), (Some(false), false), (None, false)] {
            let rows = report_rows(BoxFacts {
                inbox_driver_newer: newer,
                ..healthy()
            });
            assert_eq!(
                codes(&rows).contains(&"seat_display_rank"),
                added,
                "{newer:?}"
            );
        }
        let rows = report_rows(BoxFacts {
            inbox_driver_newer: Some(true),
            ..healthy()
        });
        assert_eq!(
            pick(&rows, "seat_display_rank"),
            (
                Error,
                "Windows' own Remote Desktop display driver is newer than punktfunk's, so seats \
                 start without a picture. Update punktfunk."
                    .into()
            )
        );
    }

    #[test]
    fn the_pin_row_follows_the_recorded_certificate() {
        let row = |pin| pick(&report_rows(BoxFacts { pin, ..healthy() }), "rdp_pin");
        assert_eq!(
            row(Some(Ok(()))),
            (Info, "Remote Desktop's certificate is recorded.".into())
        );
        assert_eq!(
            row(Some(Err("the certificate changed".into()))),
            (Error, "the certificate changed".into())
        );
        assert_eq!(
            row(None),
            (
                Error,
                "Remote Desktop's certificate isn't recorded, so seats can't sign in. \
                 Turn seats off and on under Profiles → Seats."
                    .into()
            )
        );
    }

    #[test]
    fn the_steam_row_follows_where_steam_is() {
        let row = |steam_box: Option<&str>, steam_registered: Option<&str>| {
            let facts = BoxFacts {
                steam_box: steam_box.map(PathBuf::from),
                steam_registered: steam_registered.map(PathBuf::from),
                ..healthy()
            };
            pick(&report_rows(facts), "steam")
        };
        assert_eq!(
            row(Some(r"C:\Steam"), Some(r"D:\Steam")),
            (
                Info,
                r"Steam is installed in C:\Steam. Each seat gets a copy of its own.".into()
            )
        );
        assert_eq!(
            row(None, Some(r"D:\Steam")),
            (
                Warning,
                "Steam is installed in D:\\Steam, so seats get no Steam. Seats copy Steam only from \
                 C:\\Program Files (x86)\\Steam; reinstall it there."
                    .into()
            )
        );
        assert_eq!(
            row(None, None),
            (
                Info,
                "Steam isn't installed, so seats have no Steam.".into()
            )
        );
    }

    /// Sound is owed only to a box with seats.
    #[test]
    fn missing_steam_audio_warns_only_with_seats() {
        let row = |has_seats, seat_sound| {
            let rows = report_rows(BoxFacts {
                has_seats,
                seat_sound,
                ..healthy()
            });
            rows.into_iter().find(|row| row.code == "seat_audio")
        };
        assert_eq!(row(true, true), None);
        assert_eq!(row(false, false), None);
        assert_eq!(
            row(true, false),
            Some(Diagnostic::warning(
                "seat_audio",
                "Seats stream without sound: Steam's streaming audio driver isn't on this PC. \
                 Install Steam, then restart the seats."
            ))
        );
    }

    fn seat(slot: u8) -> Seat {
        let at = usize::from(slot - 12);
        Seat {
            id: SeatId::random(),
            name: "one".into(),
            account: "pfs-one".into(),
            display_slot: slot,
            native_port: DEFAULT_NATIVE_PORTS[at],
            mgmt_port: DEFAULT_MGMT_PORTS[at],
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            owner: false,
            fingerprint: None,
            adopted_sid: None,
            adopted_uid: None,
            rdp_granted: false,
        }
    }

    fn inspection() -> AccountInspection {
        AccountInspection {
            identity_matches: true,
            credential_blob: true,
            rdp_member: true,
            administrator: false,
            deny_console: true,
            deny_remote: false,
            seats_member: true,
        }
    }

    fn state(state: RuntimeState, detail: Option<&str>) -> RuntimeStatus {
        RuntimeStatus {
            state,
            detail: detail.map(Into::into),
        }
    }

    fn healthy_seat() -> SeatFacts {
        SeatFacts {
            account: Ok(inspection()),
            group: "punktfunk-seats",
            box_steam: true,
            seat_steam: true,
            status: state(RuntimeState::Running, None),
            session_id: Some(3),
            native_open: true,
            mgmt_open: true,
        }
    }

    /// The `code` row of `seat`, if it has one; every row names the seat.
    fn seat_pick(seat: &Seat, facts: &SeatFacts, code: &str) -> Option<(DiagnosticLevel, String)> {
        let rows = seat_rows(seat, facts);
        assert!(rows
            .iter()
            .all(|row| row.seat_id.as_ref() == Some(&seat.id)));
        rows.into_iter()
            .find(|row| row.code == code)
            .map(|row| (row.level, row.message))
    }

    #[test]
    fn a_healthy_seat_reads_info_throughout() {
        let seat = seat(12);
        let rows = seat_rows(&seat, &healthy_seat());
        assert!(rows
            .iter()
            .all(|row| row.seat_id.as_ref() == Some(&seat.id)));
        assert_eq!(
            shown(&rows),
            [
                (Info, "account", "Its Windows account pfs-one is set up."),
                (
                    Info,
                    "steam_client",
                    "Has its own Steam. Steam opened on this seat starts that copy."
                ),
                (Info, "child_state", "Running, in Windows session 3."),
            ]
        );
    }

    #[test]
    fn a_seat_off_its_slot_or_ports_is_an_error() {
        let row = |slot, native_port, mgmt_port| {
            let seat = Seat {
                native_port,
                mgmt_port,
                ..seat(slot)
            };
            seat_pick(&seat, &healthy_seat(), "seat_resources")
        };
        let wrong = |slot, native, mgmt| {
            Some((
                Error,
                format!(
                    "Its display slot and ports don't match ({slot}, {native}, {mgmt}). Remove the profile and add it again."
                ),
            ))
        };
        assert_eq!(row(12, 9779, 47995), None);
        assert_eq!(row(15, 9782, 47998), None);
        assert_eq!(row(12, 9780, 47995), wrong(12, 9780, 47995));
        assert_eq!(row(12, 9779, 47996), wrong(12, 9779, 47996));
        assert_eq!(row(13, 9779, 47995), wrong(13, 9779, 47995));
    }

    #[test]
    fn the_account_row_names_the_first_thing_wrong() {
        let seat = seat(12);
        let row = |account: AccountInspection| {
            let facts = SeatFacts {
                account: Ok(account),
                ..healthy_seat()
            };
            seat_pick(&seat, &facts, "account").unwrap()
        };
        let missing = (
            Error,
            "Its Windows account pfs-one is missing or isn't punktfunk's. Remove the profile and add it again."
                .to_string(),
        );
        let lost = (
            Error,
            "Its Windows account pfs-one lost a right it needs to sign in to its seat. \
             Remove the profile and add it again."
                .to_string(),
        );
        assert_eq!(
            row(inspection()),
            (Info, "Its Windows account pfs-one is set up.".into())
        );
        assert_eq!(
            row(AccountInspection {
                identity_matches: false,
                ..inspection()
            }),
            missing
        );
        assert_eq!(
            row(AccountInspection {
                credential_blob: false,
                ..inspection()
            }),
            (
                Error,
                "punktfunk has lost the password of its Windows account pfs-one. Remove the profile and add it again."
                    .into()
            )
        );
        assert_eq!(
            row(AccountInspection {
                administrator: true,
                ..inspection()
            }),
            (
                Error,
                "Its Windows account pfs-one is an administrator, which a seat must not be. \
                 Remove it from Administrators."
                    .into()
            )
        );
        for lacking in [
            AccountInspection {
                rdp_member: false,
                ..inspection()
            },
            AccountInspection {
                deny_console: false,
                ..inspection()
            },
            AccountInspection {
                deny_remote: true,
                ..inspection()
            },
        ] {
            assert_eq!(row(lacking), lost);
        }
        let left = (
            Warning,
            "Its Windows account pfs-one left the punktfunk-seats group. It's put back when the seat next starts."
                .to_string(),
        );
        assert_eq!(
            row(AccountInspection {
                seats_member: false,
                ..inspection()
            }),
            left
        );
        // The earlier check wins, and any error outranks the group warning.
        assert_eq!(
            row(AccountInspection {
                identity_matches: false,
                administrator: true,
                seats_member: false,
                ..inspection()
            }),
            missing
        );
        assert_eq!(
            row(AccountInspection {
                rdp_member: false,
                seats_member: false,
                ..inspection()
            }),
            lost
        );
    }

    /// An adopted row's account reads as the person's: each wrong state names its own move, and
    /// the fine row says the console deny.
    #[test]
    fn an_adopted_account_row_names_the_person_s_move() {
        let seat = Seat {
            account: "Bärbel Ott".into(),
            adopted_sid: Some("S-1-5-21-1-2-3-1001".into()),
            adopted_uid: None,
            rdp_granted: true,
            ..seat(12)
        };
        let row = |account: AccountInspection| {
            let facts = SeatFacts {
                account: Ok(account),
                ..healthy_seat()
            };
            seat_pick(&seat, &facts, "account").unwrap()
        };
        assert_eq!(
            row(AccountInspection {
                identity_matches: false,
                ..inspection()
            }),
            (
                Error,
                "Its Windows account Bärbel Ott is gone, or was made again. Remove the profile and add it again."
                    .into()
            )
        );
        assert_eq!(
            row(AccountInspection {
                credential_blob: false,
                ..inspection()
            }),
            (
                Error,
                "punktfunk has lost Bärbel Ott's password. Enter it again on the profile's card."
                    .into()
            )
        );
        assert_eq!(
            row(AccountInspection {
                administrator: true,
                ..inspection()
            }),
            (
                Error,
                "Its Windows account Bärbel Ott is an administrator, which a seat must not be. \
                 Remove it from Administrators."
                    .into()
            )
        );
        assert_eq!(
            row(AccountInspection {
                deny_console: false,
                ..inspection()
            }),
            (
                Error,
                "Its Windows account Bärbel Ott lost a right it needs to sign in to its seat. \
                 Remove the profile and add it again."
                    .into()
            )
        );
        assert_eq!(
            row(AccountInspection {
                seats_member: false,
                ..inspection()
            }),
            (
                Warning,
                "Its Windows account Bärbel Ott left the punktfunk-seats group. It's put back when the seat next starts."
                    .into()
            )
        );
        assert_eq!(
            row(inspection()),
            (
                Info,
                "Its Windows account Bärbel Ott is set up. While it's a seat, nobody signs in to it at this PC."
                    .into()
            )
        );
    }

    #[test]
    fn an_unreadable_account_is_an_error() {
        let facts = SeatFacts {
            account: Err("access denied".into()),
            ..healthy_seat()
        };
        assert_eq!(
            seat_pick(&seat(12), &facts, "account"),
            Some((
                Error,
                "Couldn't read its Windows account pfs-one — access denied.".into()
            ))
        );
    }

    #[test]
    fn the_steam_client_row_follows_the_two_installs() {
        let row = |box_steam, seat_steam| {
            let facts = SeatFacts {
                box_steam,
                seat_steam,
                ..healthy_seat()
            };
            seat_pick(&seat(12), &facts, "steam_client").unwrap()
        };
        let none = (Info, "No Steam: the PC has none to copy.".to_string());
        assert_eq!(row(false, false), none);
        assert_eq!(row(false, true), none);
        assert_eq!(
            row(true, true),
            (
                Info,
                "Has its own Steam. Steam opened on this seat starts that copy.".into()
            )
        );
        assert_eq!(
            row(true, false),
            (
                Warning,
                "Its own Steam isn't set up yet. It's copied the first time the seat starts; \
                 Steam games start on the seat once that's done."
                    .into()
            )
        );
    }

    #[test]
    fn the_state_row_follows_the_runtime_state() {
        let row = |status, session_id| {
            let facts = SeatFacts {
                status,
                session_id,
                native_open: false,
                mgmt_open: false,
                ..healthy_seat()
            };
            seat_pick(&seat(12), &facts, "child_state")
        };
        let info = |message: &str| Some((Info, message.to_string()));
        assert_eq!(
            row(state(RuntimeState::Running, None), Some(7)),
            info("Running, in Windows session 7.")
        );
        assert_eq!(
            row(state(RuntimeState::Running, None), None),
            info("Running.")
        );
        assert_eq!(
            row(state(RuntimeState::Starting, Some("Signing in…")), None),
            info("Signing in…")
        );
        assert_eq!(
            row(state(RuntimeState::Starting, None), None),
            info("Starting…")
        );
        let stopped = info("Stopped. It starts when its player connects.");
        assert_eq!(row(state(RuntimeState::Stopped, None), None), stopped);
        assert_eq!(row(state(RuntimeState::Unknown, None), None), stopped);
        assert_eq!(
            row(state(RuntimeState::Stopping, None), None),
            info("Stopping…")
        );
        // The service's own `runtime_failed` row says why.
        assert_eq!(row(state(RuntimeState::Failed, Some("why")), None), None);
    }

    /// The `seat_port` warning for a seat in `run` with the given ports open, if any.
    fn port_warning(run: RuntimeState, native_open: bool, mgmt_open: bool) -> Option<String> {
        let facts = SeatFacts {
            status: state(run, None),
            native_open,
            mgmt_open,
            ..healthy_seat()
        };
        seat_pick(&seat(12), &facts, "seat_port").map(|(level, message)| {
            assert_eq!(level, Warning);
            message
        })
    }

    #[test]
    fn a_running_seat_needs_both_ports_open() {
        let both =
            "It's running, but its ports 9779 and 47995 aren't both open. Stop and start the seat.";
        assert_eq!(port_warning(RuntimeState::Running, true, true), None);
        for open in [(false, true), (true, false), (false, false)] {
            assert_eq!(
                port_warning(RuntimeState::Running, open.0, open.1).as_deref(),
                Some(both),
                "{open:?}"
            );
        }
    }

    #[test]
    fn a_port_held_by_an_idle_seat_blames_another_program() {
        let native =
            "Another program holds its port 9779, so the seat can't start. Restart the PC.";
        let mgmt = "Another program holds its port 47995, so the seat can't start. Restart the PC.";
        for idle in [
            RuntimeState::Stopped,
            RuntimeState::Unknown,
            RuntimeState::Failed,
        ] {
            assert_eq!(port_warning(idle.clone(), false, false), None);
            assert_eq!(
                port_warning(idle.clone(), true, false).as_deref(),
                Some(native)
            );
            assert_eq!(
                port_warning(idle.clone(), false, true).as_deref(),
                Some(mgmt)
            );
            // The native port is named first.
            assert_eq!(port_warning(idle, true, true).as_deref(), Some(native));
        }
    }

    /// A starting seat opens its ports on the way and a stopping one holds them until it is gone.
    #[test]
    fn a_seat_changing_state_may_hold_its_ports() {
        for state in [RuntimeState::Starting, RuntimeState::Stopping] {
            for open in [(false, false), (true, false), (false, true), (true, true)] {
                assert_eq!(
                    port_warning(state.clone(), open.0, open.1),
                    None,
                    "{state:?} {open:?}"
                );
            }
        }
    }
}
