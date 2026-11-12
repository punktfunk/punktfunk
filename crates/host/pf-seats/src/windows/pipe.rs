//! The supervisor's named pipe, open to SYSTEM and Administrators only.
//!
//! One instance waits at a time. Each connection gets its own thread, answers one request
//! ([`crate::ipc::answer`]) and closes. The first instance carries
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that created the name first makes
//! [`PipeServer::bind`] fail instead of answering clients in the supervisor's place;
//! [`PipeServer::bind_until`] tries again until the name is free. The pipe mode refuses remote
//! clients. [`wake`] ends a wait so the loop can see its stop flag;
//! [`request`] is the client the console host calls. It talks only to a server that runs as
//! LocalSystem, since the name has no owner while the service is down, and it gives up on an
//! answer that takes too long.

use super::util::{io_error, process_is_local_system, wide, WinResult};
use crate::backend::PlatformBackend;
use crate::backoff::{Backoff, Curve, Reset};
use crate::service::SeatService;
use pf_paths_win::SecurityAttributes;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::IO::CancelIoEx;

/// Requests served at once. A connection past it is closed unanswered.
const MAX_CONNECTIONS: usize = 8;
/// A whole frame and its length prefix fit in the pipe's buffer.
const BUFFER_BYTES: u32 = crate::ipc::MAX_FRAME_BYTES as u32 + 4;
/// The wait before binding again a name another process holds: whoever made it first may hold
/// it for as long as it runs.
const BIND_RETRY: Backoff = Backoff::new(
    Curve::Doubling {
        first: Duration::from_secs(1),
        cap: Duration::from_secs(30),
    },
    Reset::Never,
);

/// Serves [`crate::ipc`] requests on one pipe name until a stop flag is set.
pub struct PipeServer<B> {
    name: String,
    service: Arc<SeatService<B>>,
    first: Option<OwnedHandle>,
    active: Arc<AtomicUsize>,
}

impl<B: PlatformBackend> PipeServer<B> {
    /// Creates the first instance now, so a name someone else holds fails here.
    pub fn bind(name: &str, service: Arc<SeatService<B>>) -> WinResult<Self> {
        let first = create_instance(name, true)?;
        Ok(Self {
            name: name.to_owned(),
            service,
            first: Some(first),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// [`Self::bind`], tried again after [`BIND_RETRY`]'s wait while it fails, until it binds.
    /// `None` once `stop` is set. An outage logs its first failure and its end.
    pub fn bind_until(name: &str, service: Arc<SeatService<B>>, stop: &AtomicBool) -> Option<Self> {
        let mut retry = BIND_RETRY;
        while !stop.load(Ordering::SeqCst) {
            let error = match Self::bind(name, Arc::clone(&service)) {
                Ok(server) => {
                    if retry.failures() > 0 {
                        tracing::info!(pipe = name, "seats pipe open");
                    }
                    return Some(server);
                }
                Err(error) => error,
            };
            if retry.failures() == 0 {
                tracing::error!(pipe = name, %error, "seats pipe did not open; trying again");
            }
            let until = Instant::now() + retry.failed(Instant::now());
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        None
    }

    /// Accepts until `stop` is set and [`wake`] has ended the wait in progress.
    pub fn serve_until(mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            let instance = match self.first.take() {
                Some(instance) => instance,
                None => match create_instance(&self.name, false) {
                    Ok(instance) => instance,
                    Err(error) => {
                        tracing::warn!(%error, "seats pipe instance did not open");
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                },
            };
            // SAFETY: `instance` is a live pipe handle this loop owns; the call is synchronous.
            let connected = unsafe { ConnectNamedPipe(HANDLE(instance.as_raw_handle()), None) };
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match connected {
                Ok(()) => self.spawn(instance),
                Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                    self.spawn(instance)
                }
                Err(error) => tracing::warn!(%error, "seats pipe connect"),
            }
        }
    }

    fn spawn(&self, instance: OwnedHandle) {
        let Some(permit) = Permit::take(&self.active) else {
            tracing::warn!(
                limit = MAX_CONNECTIONS,
                "seats pipe busy; connection closed"
            );
            return;
        };
        let service = Arc::clone(&self.service);
        let spawned = std::thread::Builder::new()
            .name("seats-pipe".into())
            .spawn(move || {
                let _permit = permit;
                let mut file = std::fs::File::from(instance);
                if let Err(error) = crate::ipc::answer(&service, &mut file) {
                    tracing::debug!(%error, "seats pipe request");
                }
                // SAFETY: the handle is live inside `file`. The flush returns once the client
                // has read the answer, so closing next never drops it.
                let _ = unsafe { FlushFileBuffers(HANDLE(file.as_raw_handle())) };
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "seats pipe thread did not start");
        }
    }
}

/// Ends a [`PipeServer::serve_until`] wait by connecting to it.
pub fn wake(name: &str) {
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name);
}

/// How long `command` may take to answer. A read of the ledger is quick. The role query behind
/// `Seating` and `Doctor` takes about a minute after a restart, a start waits on a seat, and
/// turning seats on with `prepare` installs a Windows role.
fn answer_within(command: &crate::ipc::Command) -> Duration {
    use crate::ipc::Command;
    match command {
        Command::List => Duration::from_secs(60),
        Command::Enable { prepare: true, .. } => Duration::from_secs(30 * 60),
        _ => Duration::from_secs(5 * 60),
    }
}

/// Sends one request on `name` and reads its answer, the console host's side of the pipe. An
/// answer that takes longer than [`answer_within`] is given up on.
pub fn request(
    name: &str,
    command: crate::ipc::Command,
) -> Result<crate::ipc::CommandResult, crate::ipc::ApiError> {
    let answer = answer_within(&command);
    exchange(connect(name)?, command, answer)
}

/// The pipe's client end, once its server is the service. A local user can create the name
/// while the service is down and answer in its place.
fn connect(name: &str) -> Result<std::fs::File, crate::ipc::ApiError> {
    let file = open(name)?;
    match served_by_system(&file) {
        Ok(true) => Ok(file),
        Ok(false) => Err(transport(
            "check the seats pipe's server",
            &"it does not run as LocalSystem",
        )),
        Err(error) => Err(transport("check the seats pipe's server", &error)),
    }
}

/// Whether the process serving `file` runs as LocalSystem, as the service does.
fn served_by_system(file: &std::fs::File) -> WinResult<bool> {
    let mut pid = 0_u32;
    // SAFETY: the pipe handle is live inside `file` and `pid` is a writable out-parameter.
    unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &mut pid) }
        .map_err(|error| io_error("seats_pipe", "read the pipe server's process", error))?;
    // SAFETY: opens the server's process for a token query; the handle is closed on drop.
    let server = unsafe {
        Owned::new(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .map_err(|error| io_error("seats_pipe", "open the pipe server's process", error))?,
        )
    };
    process_is_local_system(*server)
}

fn transport(what: &str, error: &dyn std::fmt::Display) -> crate::ipc::ApiError {
    crate::ipc::ApiError::new(crate::ipc::ErrorCode::Transport, format!("{what}: {error}"))
}

/// The pipe's client end, whoever serves it. The server has one instance waiting at a time, so
/// a connect that lands between two retries for up to two seconds.
fn open(name: &str) -> Result<std::fs::File, crate::ipc::ApiError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(file) => return Ok(file),
            // ERROR_FILE_NOT_FOUND between instances, ERROR_PIPE_BUSY while one is taken.
            Err(error)
                if matches!(error.raw_os_error(), Some(2 | 231))
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(transport("open the seats pipe", &error)),
        }
    }
}

/// Writes the request, then waits `answer` for the reply.
fn exchange(
    mut file: std::fs::File,
    command: crate::ipc::Command,
    answer: Duration,
) -> Result<crate::ipc::CommandResult, crate::ipc::ApiError> {
    use crate::ipc::{Request, Response};
    crate::ipc::write_json_frame(&mut file, &Request::new(command))
        .map_err(|error| transport("write the seats request", &error))?;
    match read_answer(&file, answer).map_err(|error| transport("read the seats answer", &error))? {
        Response::Success { result, .. } => Ok(result),
        Response::Error { error, .. } => Err(error),
    }
}

/// Reads one answer frame. A read still waiting after `answer` is cancelled from here: a read
/// on a pipe handle has no timeout of its own.
fn read_answer(file: &std::fs::File, answer: Duration) -> Result<crate::ipc::Response, String> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let read = crate::ipc::read_json_frame::<_, crate::ipc::Response>(&mut &*file);
            let _ = done.send(());
            read
        });
        let late = finished.recv_timeout(answer).is_err();
        // A cancel between the frame's two reads finds nothing to cancel, so it repeats.
        while late && !reader.is_finished() {
            // SAFETY: the pipe handle is live inside `file`; cancelling another thread's
            // pending read on it is what the call is for.
            let _ = unsafe { CancelIoEx(HANDLE(file.as_raw_handle()), None) };
            std::thread::sleep(Duration::from_millis(10));
        }
        match reader.join() {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) if late => Err(format!("no answer within {} s", answer.as_secs())),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("the reading thread panicked".into()),
        }
    })
}

/// One counted connection; the count drops with it.
struct Permit(Arc<AtomicUsize>);

impl Permit {
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(active)))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn create_instance(name: &str, first: bool) -> WinResult<OwnedHandle> {
    let name_w = wide(name, "pipe name")?;
    // SYSTEM and Administrators, inherited ACEs blocked.
    let attributes = SecurityAttributes::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)")
        .map_err(|error| io_error("seats_pipe", "build the pipe DACL", error))?;
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: `name_w` is NUL-terminated, and `attributes` and the descriptor it points at
    // outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name_w.as_ptr()),
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
        return Err(io_error(
            "seats_pipe",
            "open the seats pipe",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: a valid handle fresh from CreateNamedPipeW, owned here once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;
    use crate::ipc::{
        read_json_frame, write_json_frame, Command, CommandResult, Diagnostic, ErrorCode, Request,
        Response,
    };
    use crate::model::{Ledger, RuntimeStatus, Seat, SeatId};
    use std::sync::Mutex;
    use std::time::Instant;

    fn name(tag: &str) -> String {
        format!(
            r"\\.\pipe\punktfunk-seats-test-{tag}-{}",
            std::process::id()
        )
    }

    /// A list over the pipe answers with the empty ledger; a second server on the same name
    /// is refused; a wake ends the loop.
    #[test]
    fn a_list_crosses_the_pipe() {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap());
        let name = name("list");
        let server = PipeServer::bind(&name, Arc::clone(&service)).unwrap();
        assert!(
            PipeServer::bind(&name, service).is_err(),
            "the first instance owns the name"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let loop_stop = Arc::clone(&stop);
        let serving = std::thread::spawn(move || server.serve_until(&loop_stop));

        let mut pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .unwrap();
        write_json_frame(&mut pipe, &Request::new(Command::List)).unwrap();
        let response: Response = read_json_frame(&mut pipe).unwrap();
        assert_eq!(
            response,
            Response::success(CommandResult::List { seats: Vec::new() })
        );

        stop.store(true, Ordering::SeqCst);
        wake(&name);
        serving.join().unwrap();
    }

    /// A name another process holds is bound once it is free, and a stop ends the tries.
    #[test]
    fn a_held_name_is_bound_once_it_is_free() {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap());
        let name = name("held-name");
        let squatter = create_instance(&name, true).unwrap();
        let stopped = AtomicBool::new(true);
        assert!(PipeServer::bind_until(&name, Arc::clone(&service), &stopped).is_none());
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let binding = scope.spawn(|| PipeServer::bind_until(&name, service, &stop));
            std::thread::sleep(Duration::from_millis(500));
            assert!(!binding.is_finished(), "the name is still held");
            drop(squatter);
            assert!(binding.join().unwrap().is_some());
        });
    }

    /// A backend whose Doctor holds until the test drops the sending end.
    struct Held(Mutex<std::sync::mpsc::Receiver<()>>);

    impl PlatformBackend for Held {
        fn provision(&self, _seat: &Seat) -> Result<(), BackendError> {
            Ok(())
        }

        fn start(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(RuntimeStatus::stopped())
        }

        fn stop(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(RuntimeStatus::stopped())
        }

        fn remove(&self, _seat: &Seat) -> Result<(), BackendError> {
            Ok(())
        }

        fn status(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(RuntimeStatus::stopped())
        }

        fn doctor(&self, _ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
            let _ = self.0.lock().unwrap().recv();
            Ok(Vec::new())
        }
    }

    /// A server that never answers costs the client its deadline, not its thread.
    #[test]
    fn a_server_that_never_answers_is_given_up_on() {
        let temp = tempfile::tempdir().unwrap();
        let (release, held) = std::sync::mpsc::channel();
        let service = Arc::new(SeatService::open(temp.path(), Held(Mutex::new(held))).unwrap());
        let name = name("held");
        let server = PipeServer::bind(&name, service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let loop_stop = Arc::clone(&stop);
        let serving = std::thread::spawn(move || server.serve_until(&loop_stop));

        // The exchange runs apart, so a deadline that fails fails the test instead of hanging it.
        let (done, finished) = std::sync::mpsc::channel();
        let client_name = name.clone();
        std::thread::spawn(move || {
            let file = open(&client_name).unwrap();
            let _ = done.send(exchange(file, Command::Doctor, Duration::from_secs(1)));
        });
        let asked = Instant::now();
        let answer = finished.recv_timeout(Duration::from_secs(30));
        let error = answer.expect("the client gave up").unwrap_err();
        assert_eq!(error.code, ErrorCode::Transport);
        assert!(error.message.contains("no answer within 1 s"), "{error:?}");
        assert!(asked.elapsed() < Duration::from_secs(10));

        drop(release);
        stop.store(true, Ordering::SeqCst);
        wake(&name);
        serving.join().unwrap();
    }

    #[test]
    fn a_command_waits_as_long_as_its_work_takes() {
        let id = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let waits = [
            (Command::List, 60),
            (Command::Seating, 300),
            (Command::Doctor, 300),
            (Command::Start { id }, 300),
            (
                Command::Enable {
                    allow_rdp_from_network: false,
                    prepare: false,
                },
                300,
            ),
            (
                Command::Enable {
                    allow_rdp_from_network: false,
                    prepare: true,
                },
                1800,
            ),
        ];
        for (command, seconds) in waits {
            assert_eq!(answer_within(&command).as_secs(), seconds, "{command:?}");
        }
    }

    /// The doctor's queries and the keeper's look end before the client stops waiting.
    #[test]
    fn the_doctor_ends_before_the_client_gives_up() {
        let doctor = super::answer_within(&Command::Doctor);
        let work = crate::windows::enable::DOCTOR_BUDGET + crate::windows::keeper::OBSERVE_LIMIT;
        assert!(work < doctor, "{work:?} against {doctor:?}");
    }

    /// The client talks to a server that runs as LocalSystem and to no other. Run as LocalSystem
    /// this test expects the answer, run as anyone else it expects the refusal.
    #[test]
    fn the_client_trusts_only_a_system_server() {
        let temp = tempfile::tempdir().unwrap();
        let service = Arc::new(SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap());
        let name = name("trust");
        let server = PipeServer::bind(&name, service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let loop_stop = Arc::clone(&stop);
        let serving = std::thread::spawn(move || server.serve_until(&loop_stop));

        let system = crate::windows::util::is_local_system().unwrap();
        let answer = request(&name, Command::List);
        if system {
            assert!(
                matches!(answer, Ok(CommandResult::List { .. })),
                "{answer:?}"
            );
        } else {
            let error = answer.unwrap_err();
            assert_eq!(error.code, ErrorCode::Transport);
            assert!(error.message.contains("LocalSystem"), "{error:?}");
        }

        stop.store(true, Ordering::SeqCst);
        wake(&name);
        serving.join().unwrap();
    }
}
