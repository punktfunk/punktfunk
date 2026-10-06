//! `punktfunk-seats`: the Linux seat supervisor and the operator's client for it.
//!
//! `serve` is the root daemon (`punktfunk-seats.service`). The other verbs send one request to
//! its socket, as the box host does, and print the answer.

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
mod linux {
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
        \x20 start <id> | stop <id> | delete <id>\n\
        \x20 doctor                      what a seat needs and what is missing\n\
        \x20 seating                     whether seats are on\n";

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
            "list" => Command::List,
            "create" => {
                let name = argument.ok_or(format!("create needs a name\n{USAGE}"))?;
                Command::Create(CreateSeat {
                    account: free_account(&socket_path)?,
                    name,
                    autostart: false,
                })
            }
            "start" => Command::Start { id: id(argument)? },
            "stop" => Command::Stop { id: id(argument)? },
            "delete" => Command::Delete { id: id(argument)? },
            "doctor" => Command::Doctor,
            "seating" => Command::Seating,
            _ => return Err(USAGE.into()),
        };
        let result = socket::request(&socket_path, command)
            .map_err(|e| format!("{:?}: {}", e.code, e.message))?;
        print_result(&result)
    }

    /// `pf-seat-<n>` for the first n no seat uses.
    fn free_account(socket_path: &std::path::Path) -> Result<String, String> {
        let CommandResult::List { seats } = socket::request(socket_path, Command::List)
            .map_err(|e| format!("{:?}: {}", e.code, e.message))?
        else {
            return Err("the supervisor answered a list with something else".into());
        };
        (1..=pf_seats::model::MAX_SEATS)
            .map(|n| format!("pf-seat-{n}"))
            .find(|account| seats.iter().all(|s| &s.account != account))
            .ok_or_else(|| "four seats exist already".into())
    }

    fn print_result(result: &CommandResult) -> Result<(), String> {
        let line = |seat: &pf_seats::Seat| {
            let detail = seat.runtime.detail.as_deref().unwrap_or("");
            println!(
                "{} {:?} {} native={} mgmt={} {:?} {detail}",
                seat.id,
                seat.runtime.state,
                seat.account,
                seat.native_port,
                seat.mgmt_port,
                seat.name,
            );
        };
        match result {
            CommandResult::List { seats } => seats.iter().for_each(line),
            CommandResult::Created { seat }
            | CommandResult::Started { seat }
            | CommandResult::Stopped { seat } => line(seat),
            CommandResult::Deleted { id } => println!("{id} deleted"),
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
    /// Seats are units of their own, so stopping the supervisor leaves them running.
    fn serve(socket_path: &std::path::Path, steam_source: Option<PathBuf>) -> Result<(), String> {
        pf_seats::logging::to_stderr();
        let backend = LinuxBackend::open(pf_paths::config_dir(), steam_source)
            .map_err(|e| format!("open the seat backend: {e}"))?;
        let service = Arc::new(
            backend
                .open_service()
                .map_err(|e| format!("open the seat ledger: {e}"))?,
        );
        if let Err(error) = service.reconcile_startup() {
            tracing::warn!(code = ?error.code, "seat autostart: {}", error.message);
        }
        let listener = socket::bind(socket_path)
            .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;
        tracing::info!(socket = %socket_path.display(), "seat supervisor serving");
        socket::serve(listener, service);
        Ok(())
    }
}
