//! Per-seat RDP, quality-gate, and host lifecycle supervision.
//!
//! Each runtime has its own stop flag, condition variable, child snapshot, and
//! worker; there is no process-global seat singleton. Start blocks until the
//! keeper creates one exact active WTS session, the same-session SDR quality
//! gate succeeds, and both host ports accept connections. Host crashes restart
//! with capped exponential delay while the keeper lives. Keeper loss ends the
//! host, closes the keeper's job, logs off only the recorded session, and
//! retries without spinning.
//! Explicit stop is idempotent and waits for that teardown to finish.

use crate::bootstrap::RdpBootstrap;
use crate::model::{RuntimeStatus, Seat};
use crate::persistence::SecretRoot;
use crate::windows::accounts::AccountManager;
use crate::windows::process::{self, ChildProcess, Job};
use crate::windows::rdp;
use crate::windows::util::{backend_error, io_error, port_open, udp_port_owners, WinResult};
use crate::windows::wts;
use rand::RngCore as _;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(60);
/// Fresh sessions a failing gate gets. A session whose display misses its path never
/// gets one; a fresh logon passes about 40% of the time on 9300.
const QUALITY_SESSIONS: u32 = 5;
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct Supervisor {
    root: SecretRoot,
    accounts: AccountManager,
    host_path: PathBuf,
    runtimes: Arc<Mutex<HashMap<String, Arc<Runtime>>>>,
}

struct Runtime {
    account: String,
    stop: AtomicBool,
    snapshot: Mutex<RuntimeSnapshot>,
    changed: Condvar,
    join: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub(super) struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub session_id: Option<u32>,
    pub keeper_pid: Option<u32>,
    pub host_pid: Option<u32>,
    pub finished: bool,
    initial: Option<Result<(), crate::backend::BackendError>>,
}

impl Runtime {
    fn new(account: String) -> Self {
        Self {
            account,
            stop: AtomicBool::new(false),
            snapshot: Mutex::new(RuntimeSnapshot {
                status: RuntimeStatus {
                    state: crate::model::RuntimeState::Starting,
                    detail: Some("waiting for managed RDP session".into()),
                },
                session_id: None,
                keeper_pid: None,
                host_pid: None,
                finished: false,
                initial: None,
            }),
            changed: Condvar::new(),
            join: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RuntimeSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn update(&self, update: impl FnOnce(&mut RuntimeSnapshot)) {
        update(&mut self.lock());
        self.changed.notify_all();
    }

    fn status(&self) -> RuntimeSnapshot {
        self.lock().clone()
    }
}

impl Supervisor {
    pub(super) fn new(root: SecretRoot, accounts: AccountManager, host_path: PathBuf) -> Self {
        Self {
            root,
            accounts,
            host_path,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn start(&self, seat: &Seat) -> WinResult<RuntimeStatus> {
        if !self.host_path.is_absolute() || !self.host_path.is_file() {
            return Err(backend_error(
                "host_missing",
                format!(
                    "configured host executable '{}' is missing",
                    self.host_path.display()
                ),
            ));
        }
        let keeper = super::keeper::keeper_path(&self.host_path);
        if !keeper.is_file() {
            return Err(backend_error(
                "keeper_missing",
                format!("seat keeper '{}' is missing", keeper.display()),
            ));
        }
        let key = seat.id.to_string();
        {
            let mut runtimes = self
                .runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(existing) = runtimes.get(&key) {
                let snapshot = existing.status();
                if !snapshot.finished {
                    return match snapshot.initial {
                        Some(Err(error)) => Err(error),
                        _ => Ok(snapshot.status),
                    };
                }
                runtimes.remove(&key);
            }
        }
        let _ = rdp::load_pin(&self.root)?;
        let _ = self.accounts.credential_for_start(seat)?;
        if let Some(session) = wts::active_for_account(&seat.account)? {
            return Err(backend_error(
                "wts_unmanaged_active",
                format!(
                    "account '{}' already has active non-console session {}",
                    seat.account, session.id
                ),
            ));
        }
        ensure_ports_free(seat)?;

        let runtime = {
            let mut runtimes = self
                .runtimes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let runtime = Arc::new(Runtime::new(seat.account.clone()));
            runtimes.insert(key.clone(), runtime.clone());
            runtime
        };
        let worker_runtime = runtime.clone();
        let root = self.root.clone();
        let accounts = self.accounts.clone();
        let host_path = self.host_path.clone();
        let seat_owned = seat.clone();
        let worker = match std::thread::Builder::new()
            .name(format!("seat-{}", seat.id))
            .spawn(move || run_worker(worker_runtime, root, accounts, host_path, seat_owned))
        {
            Ok(worker) => worker,
            Err(error) => {
                self.runtimes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&key);
                return Err(io_error("supervisor_spawn", "spawn seat supervisor", error));
            }
        };
        *runtime
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(worker);

        let deadline = Instant::now() + START_TIMEOUT;
        let mut snapshot = runtime.lock();
        loop {
            if let Some(result) = snapshot.initial.clone() {
                return result.map(|()| snapshot.status.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                runtime.stop.store(true, Ordering::Release);
                runtime.changed.notify_all();
                return Err(backend_error(
                    "start_timeout",
                    "seat did not become ready within 90 seconds",
                ));
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, _) = runtime
                .changed
                .wait_timeout(snapshot, wait)
                .unwrap_or_else(|poison| poison.into_inner());
            snapshot = next;
        }
    }

    pub(super) fn stop(&self, seat: &Seat) -> WinResult<RuntimeStatus> {
        let key = seat.id.to_string();
        let runtime = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&key)
            .cloned();
        let Some(runtime) = runtime else {
            wts::logoff_account_if_present(&seat.account)?;
            return Ok(RuntimeStatus::stopped());
        };
        runtime.stop.store(true, Ordering::Release);
        runtime.changed.notify_all();
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut snapshot = runtime.lock();
        while !snapshot.finished {
            let now = Instant::now();
            if now >= deadline {
                return Err(backend_error(
                    "stop_timeout",
                    format!("seat {} did not stop within 15 seconds", seat.id),
                ));
            }
            let (next, _) = runtime
                .changed
                .wait_timeout(snapshot, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poison| poison.into_inner());
            snapshot = next;
        }
        drop(snapshot);
        if let Some(worker) = runtime
            .join
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = worker.join();
        }
        wts::logoff_account_if_present(&seat.account)?;
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if runtimes
            .get(&key)
            .is_some_and(|stored| Arc::ptr_eq(stored, &runtime))
        {
            runtimes.remove(&key);
        }
        Ok(RuntimeStatus::stopped())
    }

    pub(super) fn status(&self, seat: &Seat) -> RuntimeSnapshot {
        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(seat.id.as_str())
            .map(|runtime| runtime.status())
            .unwrap_or(RuntimeSnapshot {
                status: RuntimeStatus::stopped(),
                session_id: None,
                keeper_pid: None,
                host_pid: None,
                finished: true,
                initial: Some(Ok(())),
            })
    }

    pub(super) fn stop_all(&self) {
        let runtimes: Vec<_> = self
            .runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect();
        for runtime in &runtimes {
            runtime.stop.store(true, Ordering::Release);
            runtime.changed.notify_all();
        }
        for runtime in runtimes {
            let deadline = Instant::now() + STOP_TIMEOUT;
            let mut snapshot = runtime.lock();
            while !snapshot.finished && Instant::now() < deadline {
                let (next, _) = runtime
                    .changed
                    .wait_timeout(snapshot, Duration::from_millis(250))
                    .unwrap_or_else(|poison| poison.into_inner());
                snapshot = next;
            }
            drop(snapshot);
            let _ = wts::logoff_account_if_present(&runtime.account);
            if runtime.status().finished
                && let Some(worker) = runtime
                    .join
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take()
            {
                let _ = worker.join();
            }
        }
        self.runtimes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|_, runtime| !runtime.status().finished);
    }
}

fn run_worker(
    runtime: Arc<Runtime>,
    root: SecretRoot,
    accounts: AccountManager,
    host_path: PathBuf,
    seat: Seat,
) {
    let mut quality_done = false;
    let mut gate_failures = 0_u32;
    let mut delay = Duration::from_secs(1);
    loop {
        if runtime.stop.load(Ordering::Acquire) {
            break;
        }
        runtime.update(|snapshot| {
            snapshot.status = RuntimeStatus {
                state: crate::model::RuntimeState::Starting,
                detail: Some("establishing managed RDP session".into()),
            };
            snapshot.session_id = None;
            snapshot.keeper_pid = None;
            snapshot.host_pid = None;
        });
        let result = run_cycle(
            &runtime,
            &root,
            &accounts,
            &host_path,
            &seat,
            &mut quality_done,
        );
        match result {
            Ok(()) => break,
            Err(error) => {
                let stopping = runtime.stop.load(Ordering::Acquire);
                if error.code == "quality_failed" {
                    gate_failures += 1;
                }
                // A failed gate retries in a fresh session; the start waits for those.
                let gate_retry = error.code == "quality_failed" && gate_failures < QUALITY_SESSIONS;
                let retry = !stopping && (gate_retry || !permanent_failure(&error));
                // The first answer reaches the caller; later failures have only this log.
                let answered = runtime.status().initial.is_some();
                if !stopping && (answered || gate_retry) {
                    tracing::warn!(
                        seat = %seat.id,
                        code = %error.code,
                        retry,
                        "seat did not start: {}",
                        error.message
                    );
                }
                runtime.update(|snapshot| {
                    snapshot.status = RuntimeStatus::failed(error.to_string());
                    if snapshot.initial.is_none() && !gate_retry {
                        snapshot.initial = Some(Err(error.clone()));
                    }
                    snapshot.session_id = None;
                    snapshot.keeper_pid = None;
                    snapshot.host_pid = None;
                });
                if !retry {
                    break;
                }
                if !sleep_stoppable(&runtime.stop, delay) {
                    break;
                }
                delay = delay.saturating_mul(2).min(MAX_RESTART_DELAY);
            }
        }
    }
    runtime.update(|snapshot| {
        if runtime.stop.load(Ordering::Acquire) {
            snapshot.status = RuntimeStatus::stopped();
            if snapshot.initial.is_none() {
                snapshot.initial = Some(Err(backend_error(
                    "seat_stopping",
                    "seat stopped before startup completed",
                )));
            }
        }
        snapshot.finished = true;
        snapshot.session_id = None;
        snapshot.keeper_pid = None;
        snapshot.host_pid = None;
    });
}

fn run_cycle(
    runtime: &Arc<Runtime>,
    root: &SecretRoot,
    accounts: &AccountManager,
    host_path: &Path,
    seat: &Seat,
    quality_done: &mut bool,
) -> WinResult<()> {
    let pin = rdp::load_pin(root)?;
    let credential = accounts.credential_for_start(seat)?;
    let bootstrap = RdpBootstrap::new(seat.account.clone(), credential.copy_zeroizing(), pin)
        .map_err(|error| io_error("keeper_bootstrap", "build RDP keeper bootstrap", error))?;
    let job = Job::new()?;
    let keeper_exe = super::keeper::keeper_path(host_path);
    let keeper = process::spawn_keeper(&job, &keeper_exe, &bootstrap)?;
    drop(bootstrap);
    runtime.update(|snapshot| snapshot.keeper_pid = Some(keeper.pid()));

    let session = wts::wait_for_active_account(&seat.account, SESSION_TIMEOUT, || {
        if runtime.stop.load(Ordering::Acquire) {
            return Ok(true);
        }
        Ok(keeper.exit_code()?.is_some())
    });
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            drop(keeper);
            drop(job);
            return Err(error);
        }
    };
    runtime.update(|snapshot| snapshot.session_id = Some(session.id));
    // The profile exists once the session does. A first copy runs beside the start; the seat's
    // Steam titles start once it lands.
    super::steam::ensure_copy(session.id, &seat.account);
    let steam = super::steam::box_install().and(super::steam::seat_exe(&seat.account));
    let result = (|| {
        let host_root = root
            .open_child_dir("hosts")
            .and_then(|hosts| hosts.open_child_dir(seat.id.as_str()))
            .map_err(|error| io_error("seat_root", "open per-seat host root", error))?;
        let temp_root = root
            .open_child_dir("temp")
            .and_then(|temp| temp.open_child_dir(seat.id.as_str()))
            .map_err(|error| io_error("seat_root", "open per-seat temporary root", error))?;
        let environment =
            process::seat_environment(host_root.path(), seat, steam.as_deref(), true)?;
        let workdir = host_path.parent().ok_or_else(|| {
            backend_error(
                "host_missing",
                "configured host executable has no parent directory",
            )
        })?;
        // The gate and the host write here, as the box host writes `logs\host.log`.
        let log = host_root
            .child_path("host.log")
            .map_err(|error| io_error("seat_log", "resolve the seat log", error))?;

        if !*quality_done {
            let gate_environment =
                process::seat_environment(host_root.path(), seat, steam.as_deref(), false)?;
            run_quality_gate(
                runtime,
                &keeper,
                session.id,
                host_path,
                workdir,
                &gate_environment,
                &temp_root,
                seat,
                &log,
            )?;
            *quality_done = true;
        }

        let mut host = spawn_host(session.id, host_path, workdir, &environment, &log)?;
        runtime.update(|snapshot| snapshot.host_pid = Some(host.pid()));
        wait_host_ready(runtime, &keeper, &host, seat)?;
        runtime.update(|snapshot| {
            snapshot.status = RuntimeStatus::running();
            if snapshot.initial.is_none() {
                snapshot.initial = Some(Ok(()));
            }
        });

        let mut host_delay = Duration::from_secs(1);
        loop {
            if runtime.stop.load(Ordering::Acquire) {
                runtime.update(|snapshot| {
                    snapshot.status = RuntimeStatus {
                        state: crate::model::RuntimeState::Stopping,
                        detail: None,
                    };
                });
                break;
            }
            if let Some(code) = keeper.exit_code()? {
                return Err(backend_error(
                    "rdp_keeper_exited",
                    format!("RDP keeper process exited with code {code}"),
                ));
            }
            if let Some(code) = host.exit_code()? {
                runtime.update(|snapshot| {
                    snapshot.status = RuntimeStatus {
                        state: crate::model::RuntimeState::Starting,
                        detail: Some(format!(
                            "host exited with code {code}; restart in {} seconds",
                            host_delay.as_secs()
                        )),
                    };
                    snapshot.host_pid = None;
                });
                if !sleep_stoppable_with_keeper(&runtime.stop, &keeper, host_delay)? {
                    break;
                }
                host_delay = host_delay.saturating_mul(2).min(MAX_RESTART_DELAY);
                host = spawn_host(session.id, host_path, workdir, &environment, &log)?;
                runtime.update(|snapshot| snapshot.host_pid = Some(host.pid()));
                wait_host_ready(runtime, &keeper, &host, seat)?;
                runtime.update(|snapshot| snapshot.status = RuntimeStatus::running());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    })();
    drop(keeper);
    drop(job);
    let logoff = wts::logoff(session.id);
    result.and(logoff)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the gate binds one process to one exact seat session"
)]
fn run_quality_gate(
    runtime: &Runtime,
    keeper: &ChildProcess,
    session_id: u32,
    host_path: &Path,
    workdir: &Path,
    environment: &[u16],
    temp_root: &SecretRoot,
    seat: &Seat,
    log: &Path,
) -> WinResult<()> {
    let mut random = [0_u8; 8];
    rand::rng().fill_bytes(&mut random);
    let name = format!("quality-{}-{}.h265", seat.id, hex::encode(random));
    temp_root
        .write_atomic(&name, &[])
        .map_err(|error| io_error("quality_output", "create quality output", error))?;
    let output = temp_root
        .child_path(&name)
        .map_err(|error| io_error("quality_output", "resolve quality output", error))?;
    let arguments = [
        "spike",
        "--source",
        "virtual",
        "--width",
        "1280",
        "--height",
        "720",
        "--fps",
        "60",
        "--seconds",
        "1",
        "--codec",
        "h265",
        "--bitrate",
        "5",
        "--no-loopback",
        "--out",
    ]
    .into_iter()
    .map(OsString::from)
    .chain(std::iter::once(output.as_os_str().to_owned()))
    .collect::<Vec<_>>();
    let result = (|| {
        let quality = process::spawn_in_session(
            session_id,
            host_path,
            &arguments,
            environment,
            workdir,
            log,
        )?;
        let code = quality.wait(QUALITY_TIMEOUT, || {
            runtime.stop.load(Ordering::Acquire) || keeper.exit_code().ok().flatten().is_some()
        })?;
        let Some(code) = code else {
            quality.terminate();
            return Err(backend_error(
                "quality_timeout",
                "same-session virtual-display quality gate did not finish within 60 seconds",
            ));
        };
        if code != 0 {
            return Err(backend_error(
                "quality_failed",
                format!("same-session virtual-display quality gate exited with code {code}"),
            ));
        }
        let bytes = temp_root
            .read_current(&name, 16 * 1024 * 1024)
            .map_err(|error| io_error("quality_output", "read quality output", error))?
            .unwrap_or_default();
        if bytes.is_empty() {
            return Err(backend_error(
                "quality_empty",
                "same-session virtual-display quality gate produced no output",
            ));
        }
        Ok(())
    })();
    let cleanup = temp_root
        .remove_file(&name)
        .map_err(|error| io_error("quality_output", "remove quality output", error));
    result.and(cleanup)
}

fn spawn_host(
    session_id: u32,
    host_path: &Path,
    workdir: &Path,
    environment: &[u16],
    log: &Path,
) -> WinResult<ChildProcess> {
    process::spawn_in_session(
        session_id,
        host_path,
        &[OsString::from("serve")],
        environment,
        workdir,
        log,
    )
}

fn wait_host_ready(
    runtime: &Runtime,
    keeper: &ChildProcess,
    host: &ChildProcess,
    seat: &Seat,
) -> WinResult<()> {
    let deadline = Instant::now() + HOST_READY_TIMEOUT;
    loop {
        if runtime.stop.load(Ordering::Acquire) {
            return Err(backend_error(
                "seat_stopping",
                "seat stopped while waiting for host readiness",
            ));
        }
        if let Some(code) = keeper.exit_code()? {
            return Err(backend_error(
                "rdp_keeper_exited",
                format!("RDP keeper exited with code {code} during host startup"),
            ));
        }
        if let Some(code) = host.exit_code()? {
            return Err(backend_error(
                "host_exited",
                format!("punktfunk-host exited with code {code} before readiness"),
            ));
        }
        // Native is QUIC: ready once this host holds the UDP port, not when TCP answers.
        if udp_port_owners(seat.native_port).contains(&host.pid()) && port_open(seat.mgmt_port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(backend_error(
                "host_ready_timeout",
                "punktfunk-host did not open both assigned ports within 30 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn ensure_ports_free(seat: &Seat) -> WinResult<()> {
    let native = seat.native_port;
    if let Some(pid) = udp_port_owners(native).first() {
        return Err(backend_error(
            "port_in_use",
            format!("assigned native port {native} is already held by process {pid}"),
        ));
    }
    let mgmt = seat.mgmt_port;
    if port_open(mgmt) {
        return Err(backend_error(
            "port_in_use",
            format!("assigned management port {mgmt} already accepts connections"),
        ));
    }
    Ok(())
}

fn sleep_stoppable(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn sleep_stoppable_with_keeper(
    stop: &AtomicBool,
    keeper: &ChildProcess,
    duration: Duration,
) -> WinResult<bool> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        if keeper.exit_code()?.is_some() {
            return Err(backend_error(
                "rdp_keeper_exited",
                "RDP keeper exited during host restart backoff",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(true)
}

fn permanent_failure(error: &crate::backend::BackendError) -> bool {
    error.code.starts_with("quality_")
        || matches!(
            error.code.as_str(),
            "account_not_owned"
                | "account_is_administrator"
                | "account_policy"
                | "credential_missing"
                | "credential_invalid"
                | "credential_verify"
                | "host_missing"
                | "rdp_pin_missing"
                | "rdp_pin_invalid"
                | "local_system_required"
        )
}
