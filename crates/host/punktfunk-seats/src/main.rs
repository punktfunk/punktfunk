//! `punktfunk-seats`: the Linux seat supervisor and the operator's client for it.
//!
//! `serve` is the root daemon (`punktfunk-seats.service`). The other verbs send one request to
//! its socket, as the box host does, and print the answer; `serve-usb` is the pad server the
//! daemon starts for a seat's USB device. The pad broker, [`pads`], and each seat's
//! `/dev/uinput`, [`uinput`], are the daemon's too; they live here rather than in `pf-seats`
//! because they link `pf-inject`, whose dependencies the Windows seat keeper's own workspace
//! cannot resolve beside IronRDP.

#![forbid(unsafe_code)]

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("punktfunk-seats runs on Linux only.");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod cuse;
#[cfg(target_os = "linux")]
mod pads;
#[cfg(target_os = "linux")]
mod uinput;
#[cfg(target_os = "linux")]
mod vhci;

#[cfg(target_os = "linux")]
mod linux {
    use crate::pads;
    use pf_seats::ipc::{Command, CommandResult, DiagnosticLevel};
    use pf_seats::linux::{socket, LinuxBackend, SOCKET_PATH};
    use pf_seats::{CreateSeat, SeatId};
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;

    const USAGE: &str = "usage: punktfunk-seats [--socket PATH] <verb>\n\
        \n\
        verbs:\n\
        \x20 serve [--steam-source DIR]  run the supervisor (root)\n\
        \x20 list                        every seat and its state\n\
        \x20 create <name>               add a seat\n\
        \x20 adopt-owner <account>       the owner's row, for the user door-helper recorded\n\
        \x20 release-owner               delete the owner's row, if there is one\n\
        \x20 stop-all                    stop every seat that runs or starts\n\
        \x20 check-owner on|off <account>  door-helper's account rules: `<uid> <home>` or `gone`\n\
        \x20 start <id> | stop <id> | delete <id>\n\
        \x20 doctor                      what a seat needs and what is missing\n\
        \x20 seating                     whether seats are on\n\
        \x20 fence                       udev's IMPORT{program}: whose a seat's pad is\n\
        \x20 ungate                      open the vhci gate and the box's hidraw (ExecStopPost)\n\
        \x20 drain                       answer the seats' waiting uinput reads (ExecStop)\n\
        \x20 serve-usb                   a seat's USB pad server, which the supervisor starts\n";

    pub fn main() -> ExitCode {
        match run(std::env::args().skip(1).collect()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("{message}");
                ExitCode::FAILURE
            }
        }
    }

    fn run(args: Vec<String>) -> Result<(), String> {
        // No flags here: the account is data from a unit's instance name.
        if let [verb, direction, account] = args.as_slice()
            && verb == "check-owner"
        {
            let on = match direction.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err(USAGE.into()),
            };
            let line = pf_seats::linux::check_owner(account, on).map_err(|e| e.to_string())?;
            println!("{line}");
            return Ok(());
        }
        let mut socket_path = PathBuf::from(SOCKET_PATH);
        let mut steam_source = None;
        let mut words = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value =
                |flag: &str| args.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
            match arg.as_str() {
                "--socket" => socket_path = PathBuf::from(value("--socket")?),
                "--steam-source" => steam_source = Some(PathBuf::from(value("--steam-source")?)),
                _ => words.push(arg),
            }
        }
        let mut words = words.into_iter();
        let verb = words.next().ok_or(USAGE)?;
        let argument = words.next();
        let id = |text: Option<String>| {
            let text = text.ok_or(format!("{verb} needs a seat id\n{USAGE}"))?;
            SeatId::parse(text).map_err(|e| e.to_string())
        };
        let command = match verb.as_str() {
            "serve" => return serve(&socket_path, steam_source),
            "fence" => {
                let devpath = std::env::var("DEVPATH").unwrap_or_default();
                if !crate::vhci::fence(&devpath) {
                    pf_seats::linux::fence::run();
                }
                return Ok(());
            }
            "ungate" => {
                pf_seats::logging::to_stderr();
                crate::vhci::ungate();
                pads::unbind();
                return Ok(());
            }
            "drain" => {
                return crate::uinput::ask_to_drain().map_err(|e| format!("drain: {e}"));
            }
            "serve-usb" => {
                pf_seats::logging::to_stderr();
                return pads::serve_usb_device();
            }
            "list" => Command::List,
            "release-owner" => return release_owner(&socket_path),
            "stop-all" => return stop_all(&socket_path),
            "create" => {
                let name = argument.ok_or(format!("create needs a name\n{USAGE}"))?;
                Command::Create(CreateSeat {
                    account: free_account(&socket_path)?,
                    name,
                    autostart: false,
                })
            }
            "adopt-owner" => Command::AdoptOwner {
                account: argument.ok_or(format!("adopt-owner needs an account\n{USAGE}"))?,
            },
            "start" => Command::Start { id: id(argument)? },
            "stop" => Command::Stop { id: id(argument)? },
            "delete" => Command::Delete { id: id(argument)? },
            "doctor" => Command::Doctor,
            "seating" => Command::Seating,
            _ => return Err(USAGE.into()),
        };
        print_result(&ask(&socket_path, command)?)
    }

    fn ask(socket_path: &std::path::Path, command: Command) -> Result<CommandResult, String> {
        socket::request(socket_path, command).map_err(|e| format!("{:?}: {}", e.code, e.message))
    }

    fn seats(socket_path: &std::path::Path) -> Result<Vec<pf_seats::Seat>, String> {
        match ask(socket_path, Command::List)? {
            CommandResult::List { seats } => Ok(seats),
            _ => Err("the supervisor answered a list with something else".into()),
        }
    }

    /// Deletes the owner's row; none is not an error. The account and its home stay.
    fn release_owner(socket_path: &std::path::Path) -> Result<(), String> {
        let Some(owner) = seats(socket_path)?.into_iter().find(|s| s.owner) else {
            return Ok(());
        };
        ask(socket_path, Command::Delete { id: owner.id }).map(drop)
    }

    /// Stops every seat that runs or starts. Each is asked even after one refuses; any refusal
    /// fails the whole.
    fn stop_all(socket_path: &std::path::Path) -> Result<(), String> {
        let live = |s: &pf_seats::Seat| {
            matches!(
                s.runtime.state,
                pf_seats::RuntimeState::Running | pf_seats::RuntimeState::Starting
            )
        };
        let refused: Vec<String> = seats(socket_path)?
            .into_iter()
            .filter(live)
            .filter_map(|s| ask(socket_path, Command::Stop { id: s.id }).err())
            .collect();
        if refused.is_empty() {
            Ok(())
        } else {
            Err(refused.join("\n"))
        }
    }

    /// `pf-seat-<n>` for the first n no seat uses.
    fn free_account(socket_path: &std::path::Path) -> Result<String, String> {
        let seats = seats(socket_path)?;
        (1..=pf_seats::model::MAX_SEATS)
            .map(|n| format!("pf-seat-{n}"))
            .find(|account| seats.iter().all(|s| &s.account != account))
            .ok_or_else(|| "four seats exist already".into())
    }

    fn print_result(result: &CommandResult) -> Result<(), String> {
        let line = |seat: &pf_seats::Seat| {
            let detail = seat.runtime.detail.as_deref().unwrap_or("");
            println!(
                "{} {:?} {} native={} mgmt={} owner={} {:?} {detail}",
                seat.id,
                seat.runtime.state,
                seat.account,
                seat.native_port,
                seat.mgmt_port,
                seat.owner,
                seat.name,
            );
        };
        match result {
            CommandResult::List { seats } => {
                seats.iter().for_each(line);
                pads::listing().iter().for_each(|pad| println!("{pad}"));
            }
            CommandResult::Created { seat }
            | CommandResult::Started { seat }
            | CommandResult::Stopped { seat }
            | CommandResult::Resealed { seat } => line(seat),
            CommandResult::Deleted { id } => println!("{id} deleted"),
            CommandResult::Flatpak { id, version } => {
                println!("{id} {}", version.as_deref().unwrap_or("-"));
            }
            CommandResult::Doctor { report } => {
                for d in &report.diagnostics {
                    println!("{:?} {} {}", d.level, d.code, d.message);
                }
                if !report.healthy {
                    return Err("doctor found problems".into());
                }
            }
            CommandResult::Seating { status } => {
                println!("enabled={}", status.enabled);
                for d in &status.checks {
                    println!("{:?} {} {}", d.level, d.code, d.message);
                }
                if status
                    .checks
                    .iter()
                    .any(|d| d.level == DiagnosticLevel::Error)
                {
                    return Err("a check failed".into());
                }
            }
        }
        Ok(())
    }

    /// Runs the supervisor: autostart seats first, then the socket until the process is stopped.
    /// Seats are units of their own, so stopping the supervisor leaves them running. Each seat's
    /// uinput proxy comes and goes with its unit, and its pads go when it stops.
    fn serve(socket_path: &std::path::Path, steam_source: Option<PathBuf>) -> Result<(), String> {
        pf_seats::logging::to_stderr();
        // Adopts systemd's kept fds close-on-exec before anything here can spawn a child.
        crate::uinput::resume(pads::resume());
        socket::ensure_door_group();
        crate::uinput::listen_for_drain();
        let state = PathBuf::from(pf_seats::linux::STATE_DIR);
        let backend = LinuxBackend::open(pf_paths::config_dir(), state, steam_source)
            .map_err(|e| format!("open the seat backend: {e}"))?
            .on_unit(Box::new(|account, uid| {
                crate::uinput::hook(account, uid);
                if uid.is_none() {
                    pads::revoke(account);
                }
            }));
        let service = Arc::new(
            backend
                .open_service()
                .map_err(|e| format!("open the seat ledger: {e}"))?,
        );
        service.backend().prepare_owners(&service.ledger());
        let listener = socket::bind(socket_path)
            .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;
        // The pad broker: a seat host's pads, made here and relayed to it.
        crate::vhci::prepare();
        let pads_path = std::path::Path::new(pads::SOCKET_PATH);
        let pads_listener =
            pads::bind(pads_path).map_err(|e| format!("bind {}: {e}", pads_path.display()))?;
        pads::refresh_pad_nodes();
        let for_pads = Arc::clone(&service);
        std::thread::Builder::new()
            .name("seat-pads".into())
            .spawn(move || pads::serve(pads_listener, for_pads))
            .map_err(|e| format!("start the pad broker: {e}"))?;
        notify_ready();
        // A request that arrives while autostart seats come up waits in the socket's queue.
        if let Err(error) = service.reconcile_startup() {
            tracing::warn!(code = ?error.code, "seat autostart: {}", error.message);
        }
        tracing::info!(socket = %socket_path.display(), "seat supervisor serving");
        socket::serve(listener, service);
        Ok(())
    }

    /// `READY=1` to systemd (`Type=notify`): the owner's files are written and the socket is
    /// bound. Only a path socket, which is what systemd hands a system service.
    fn notify_ready() {
        use std::os::unix::net::UnixDatagram;
        let Some(path) =
            std::env::var_os("NOTIFY_SOCKET").filter(|p| !p.as_encoded_bytes().starts_with(b"@"))
        else {
            return;
        };
        let sent = UnixDatagram::unbound().and_then(|s| s.send_to(b"READY=1", path));
        if let Err(error) = sent {
            tracing::warn!(%error, "systemd was not told the supervisor is ready");
        }
    }
}
