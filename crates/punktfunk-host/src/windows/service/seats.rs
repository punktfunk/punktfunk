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

/// Autostart seats first, then the seats of recent players, then the pipe until the slot
/// stops. An autostart's first failure is logged here; the supervisor logs every failure after
/// that.
fn serve(service: Arc<SeatService<WindowsBackend>>, stop: &Arc<AtomicBool>) {
    if let Err(error) = service.reconcile_startup() {
        tracing::warn!(code = ?error.code, "seat autostart: {}", error.message);
    }
    keep_warm(&service);
    {
        let (service, stop) = (Arc::clone(&service), Arc::clone(stop));
        let spawned = std::thread::Builder::new()
            .name("seats-idle".into())
            .spawn(move || stop_idle(&service, &stop));
        if let Err(error) = spawned {
            tracing::warn!("seat idle watch did not start: {error}");
        }
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

/// A profile played within this long gets its seat started at boot.
const WARM_WITHIN_SECS: u64 = 14 * 24 * 3600;
/// A seat with nobody on it this long is stopped: it holds a display slot, a GPU context and
/// an RDP session for no one.
const IDLE_STOP: std::time::Duration = std::time::Duration::from_secs(4 * 3600);
const IDLE_CHECK_SECS: u64 = 300;

/// Starts the seats of the most recent players, up to **Seats kept warm**, one at a time: a
/// first logon is heavy, and several at once starve each other.
fn keep_warm(service: &SeatService<WindowsBackend>) {
    use crate::profiles::OsAccount;
    let wanted = pf_host_config::config().steam_prewarm as usize;
    if wanted == 0 || !pf_seats::windows::seats_enabled() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut recent: Vec<(u64, String)> = crate::profiles::Profiles::load_with(None, None)
        .list()
        .into_iter()
        .filter_map(|p| match p.os_account {
            OsAccount::Seat { seat: Some(id), .. }
                if p.last_used_unix > 0
                    && now.saturating_sub(p.last_used_unix) < WARM_WITHIN_SECS =>
            {
                Some((p.last_used_unix, id))
            }
            _ => None,
        })
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (_, id) in recent.into_iter().take(wanted) {
        let Ok(id) = pf_seats::SeatId::parse(id) else {
            continue;
        };
        let running = service
            .ledger()
            .seat(&id)
            .is_some_and(|s| s.runtime.state == pf_seats::RuntimeState::Running);
        if running {
            continue;
        }
        if let Err(error) = service.dispatch(pf_seats::Command::Start { id }) {
            tracing::warn!(code = ?error.code, "kept-warm seat did not start: {}", error.message);
        }
    }
}

/// Stops a running seat once nobody has played on it for [`IDLE_STOP`]. A seat whose host
/// doesn't answer counts as busy: stopping it would be a guess.
fn stop_idle(service: &SeatService<WindowsBackend>, stop: &AtomicBool) {
    let mut busy_at: std::collections::HashMap<String, std::time::Instant> = Default::default();
    while !stop.load(Ordering::SeqCst) {
        for _ in 0..IDLE_CHECK_SECS {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let now = std::time::Instant::now();
        for seat in service.ledger().seats {
            let id = seat.id.as_str().to_string();
            if seat.runtime.state != pf_seats::RuntimeState::Running {
                busy_at.remove(&id);
                continue;
            }
            let busy = crate::seats::occupants(&seat).is_none_or(|o| !o.is_empty());
            let since = busy_at.entry(id.clone()).or_insert(now);
            if busy {
                *since = now;
            } else if now.duration_since(*since) >= IDLE_STOP {
                tracing::info!(seat = %id, name = %seat.name, idle_hours = 4, "idle seat stopped");
                if let Err(error) = service.dispatch(pf_seats::Command::Stop { id: seat.id }) {
                    tracing::warn!(code = ?error.code, "idle seat did not stop: {}", error.message);
                }
                busy_at.remove(&id);
            }
        }
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
