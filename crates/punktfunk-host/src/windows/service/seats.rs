//! The seat supervisor's slot in the service: the ledger, the seat hosts it starts, and the
//! `\\.\pipe\punktfunk-seats` pipe the console host reaches it through.
//!
//! Seat hosts run in the supervisor's own jobs, never the console host's, so a console relaunch
//! leaves them running; the service stopping logs their accounts off. Autostart seats come up
//! first, one at a time, then the pipe opens. Without `HKLM\SOFTWARE\Punktfunk\Seats` the ledger
//! is still served and every start answers `seats_off`. A slot that cannot open logs the reason
//! and stays inert: seats never stop the service.

use super::*;
use pf_seats::windows::pipe::{wake, PipeServer};
use pf_seats::{SeatService, WindowsBackend};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) struct SeatsSlot {
    stop: Arc<AtomicBool>,
    backend: Option<WindowsBackend>,
}

impl SeatsSlot {
    /// Opens the ledger beside the host config and serves it in the background. `exe` is this
    /// service's own `punktfunk-host.exe`, which every seat host runs.
    pub(super) fn start(exe: &Path) -> SeatsSlot {
        let stop = Arc::new(AtomicBool::new(false));
        let root = pf_seats::persistence::default_root();
        let opened = WindowsBackend::open(&root, exe.to_path_buf())
            .map_err(anyhow::Error::from)
            .and_then(|backend| {
                let service = SeatService::open(&root, backend.clone())?;
                Ok((backend, Arc::new(service)))
            });
        let (backend, service) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                tracing::error!(root = %root.display(), "seat supervisor did not open: {error:#}");
                return SeatsSlot {
                    stop,
                    backend: None,
                };
            }
        };
        let loop_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("seats".into())
            .spawn(move || serve(service, &loop_stop));
        if let Err(error) = spawned {
            tracing::error!("seat supervisor thread did not start: {error}");
        }
        SeatsSlot {
            stop,
            backend: Some(backend),
        }
    }
}

/// Autostart seats first, then the pipe until the slot stops. A seat that did not start is
/// logged here: the ledger records why, and nothing else reports it.
fn serve(service: Arc<SeatService<WindowsBackend>>, stop: &AtomicBool) {
    if let Err(error) = service.reconcile_startup() {
        tracing::warn!(code = ?error.code, "seat autostart: {}", error.message);
    }
    for seat in service.ledger().seats {
        if seat.runtime.state == pf_seats::RuntimeState::Failed {
            let why = seat.runtime.detail.as_deref().unwrap_or("no detail");
            tracing::warn!(seat = %seat.id, name = %seat.name, "seat did not start: {why}");
        }
    }
    if stop.load(Ordering::SeqCst) {
        return;
    }
    match PipeServer::bind(pf_seats::ipc::PIPE_NAME, service) {
        Ok(server) => {
            tracing::info!(pipe = pf_seats::ipc::PIPE_NAME, "seat supervisor serving");
            server.serve_until(stop);
        }
        Err(error) => tracing::error!("seats pipe did not open: {error}"),
    }
}

impl Drop for SeatsSlot {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        wake(pf_seats::ipc::PIPE_NAME);
        if let Some(backend) = &self.backend {
            backend.stop_all();
        }
    }
}
