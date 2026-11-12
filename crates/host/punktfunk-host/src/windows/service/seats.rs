//! The seat supervisor's slot in the service: the ledger, the seat hosts it starts, and the
//! `\\.\pipe\punktfunk-seats` pipe the console host reaches it through.
//!
//! Seat hosts run in the supervisor's own jobs, never the console host's, so a console relaunch
//! leaves them running; the service stopping logs their accounts off. The pipe opens first;
//! autostart seats then come up one at a time beside it, once TermService runs. Without
//! `HKLM\SOFTWARE\Punktfunk\Seats` the ledger is still served and every start answers
//! `seats_off`. A slot that cannot open logs the reason and stays inert: seats never stop the
//! service.

use super::*;
use pf_seats::windows::pipe::{wake, PipeServer};
use pf_seats::{SeatService, WindowsBackend};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How often the service narrows Remote Desktop to this PC again, as the idle check runs.
const RDP_LOCAL_TICK_SECS: u64 = 300;

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
        let loop_backend = backend.clone();
        let spawned = std::thread::Builder::new()
            .name("seats".into())
            .spawn(move || serve(service, &loop_backend, &loop_stop));
        if let Err(error) = spawned {
            tracing::error!("seat supervisor thread did not start: {error}");
        }
        // Windows takes about a minute to answer the first role query after a restart. Asked
        // here, the Seats dialog an operator opens after signing in is answered at once. The
        // firewall goes first and again on every tick: a rule added or reset since, or a
        // firewall not up yet at boot, must not leave Remote Desktop open to the network.
        let (warm, checks_stop) = (backend.clone(), Arc::clone(&stop));
        let _ = std::thread::Builder::new()
            .name("seats-checks".into())
            .spawn(move || {
                warm.keep_rdp_local();
                drop(pf_seats::backend::PlatformBackend::seating(&warm));
                for second in 1_u64.. {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if checks_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if second % RDP_LOCAL_TICK_SECS == 0 {
                        warm.keep_rdp_local();
                    }
                }
            });
        SeatsSlot {
            stop,
            backend: Some(backend),
        }
    }
}

/// The pipe until the slot stops, with autostart seats, then the seats of recent players, coming
/// up beside it: a start takes minutes, and the console reads the seats meanwhile. A start's
/// first failure is logged here; the supervisor logs every failure after that.
fn serve(
    service: Arc<SeatService<WindowsBackend>>,
    backend: &WindowsBackend,
    stop: &Arc<AtomicBool>,
) {
    let server = match PipeServer::bind(pf_seats::ipc::PIPE_NAME, Arc::clone(&service)) {
        Ok(server) => server,
        Err(error) => {
            tracing::error!("seats pipe did not open: {error}");
            return;
        }
    };
    {
        let (service, backend, stop) = (Arc::clone(&service), backend.clone(), Arc::clone(stop));
        let spawned = std::thread::Builder::new()
            .name("seats-warm".into())
            .spawn(move || warm(&service, &backend, &stop));
        if let Err(error) = spawned {
            tracing::warn!("seat warm-up did not start: {error}");
        }
    }
    tracing::info!(pipe = pf_seats::ipc::PIPE_NAME, "seat supervisor serving");
    server.serve_until(stop);
}

/// Autostart seats, the seats of recent players, then the idle watch until the slot stops. They
/// wait for TermService first: the service starts at boot without it, and a seat can't start
/// before it.
fn warm(
    service: &Arc<SeatService<WindowsBackend>>,
    backend: &WindowsBackend,
    stop: &Arc<AtomicBool>,
) {
    backend.wait_for_termservice(stop);
    if stop.load(Ordering::SeqCst) {
        return;
    }
    if let Err(error) = service.reconcile_startup() {
        tracing::warn!(code = ?error.code, "seat autostart: {}", error.message);
    }
    crate::seats::lifecycle::keep_warm(&**service);
    for seat in service.ledger().seats {
        if seat.runtime.state == pf_seats::RuntimeState::Failed {
            let why = seat.runtime.detail.as_deref().unwrap_or("no detail");
            tracing::warn!(seat = %seat.id, name = %seat.name, "seat did not start: {why}");
        }
    }
    if !stop.load(Ordering::SeqCst) {
        crate::seats::lifecycle::stop_idle(&**service, stop);
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
