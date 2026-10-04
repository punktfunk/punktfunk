//! The supervisor's named pipe, open to SYSTEM and Administrators only.
//!
//! One instance waits at a time. Each connection gets its own thread, answers one request
//! ([`crate::ipc::answer`]) and closes. The first instance carries
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that created the name first makes
//! [`PipeServer::bind`] fail instead of answering clients in the supervisor's place. The pipe
//! mode refuses remote clients. [`wake`] ends a wait so the loop can see its stop flag.

use super::util::{io_error, wide, WinResult};
use crate::backend::PlatformBackend;
use crate::service::SeatService;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

/// Requests served at once. A connection past it is closed unanswered.
const MAX_CONNECTIONS: usize = 8;
/// A whole frame and its length prefix fit in the pipe's buffer.
const BUFFER_BYTES: u32 = crate::ipc::MAX_FRAME_BYTES as u32 + 4;

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

/// The pipe's DACL: SYSTEM and Administrators, inherited ACEs blocked.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn new() -> WinResult<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL literal is NUL-terminated and `descriptor` is a live out-parameter.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)"),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| io_error("seats_pipe", "build the pipe DACL", error))?;
        Ok(Self(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
        // whose documented release is LocalFree, and this drop runs once.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

fn create_instance(name: &str, first: bool) -> WinResult<OwnedHandle> {
    let name_w = wide(name, "pipe name")?;
    let descriptor = Descriptor::new()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        bInheritHandle: false.into(),
    };
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
            Some(&attributes),
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
    use crate::ipc::{
        read_json_frame, write_json_frame, Command, CommandResult, Request, Response,
    };

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
}
