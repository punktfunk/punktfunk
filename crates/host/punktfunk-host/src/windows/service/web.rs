//! The web console child: bun/Nitro on :47992 in session 0, supervised beside the host. Not
//! `spawn_host` — a session retarget would tear it down on every switch. Own job, no
//! `BREAKAWAY_OK`. Backoff 0.5 s → 60 s; a run ≥ `WEB_GOOD_RUN` resets it.
//! Evidence: design/windows-web-console-lifecycle.md.

use super::*;

pub(super) fn web_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_private_dir(&dir);
    dir.join("web.log")
}

/// A run ≥ 60 s resets the consecutive-failure backoff.
pub(super) const WEB_GOOD_RUN: Duration = Duration::from_secs(60);

pub(super) const WEB_MAX_BACKOFF_MS: u64 = 60_000;

/// Console payload under the directory this exe runs from.
pub(super) struct WebConfig {
    bun: PathBuf,
    server: PathBuf,
    web_dir: PathBuf,
}

/// Supervised web-console slot. Every host-loop wait goes through `wait`.
pub(super) struct WebSlot {
    /// `None` when the payload is absent or `PUNKTFUNK_WEB_CONSOLE` opted out.
    cfg: Option<WebConfig>,
    child: Option<Child>,
    /// Kill-on-close, no breakaway. Created at first spawn; drop reaps bun and its children.
    job: Option<OwnedHandle>,
    spawned_at: Instant,
    /// Consecutive short-lived runs; spawn failures count. Drives the backoff.
    fast_exits: u32,
    next_spawn: Instant,
    /// Grace for `web-password` after the hard-gate files exist. Starting without it is safe:
    /// the console fail-closes until a password exists.
    password_deadline: Option<Instant>,
    /// One log line per wait episode, not one per poll.
    logged_wait: bool,
}

impl WebSlot {
    /// host.env is already loaded (`load_host_env` runs before `supervise`), so opt-out is an env check.
    pub(super) fn new(exe: &Path) -> WebSlot {
        let app = exe.parent().unwrap_or(Path::new("."));
        let cfg = WebConfig {
            bun: app.join("bun").join("bun.exe"),
            server: app
                .join("web")
                .join(".output")
                .join("server")
                .join("index.mjs"),
            web_dir: app.join("web"),
        };
        let opted_out = std::env::var("PUNKTFUNK_WEB_CONSOLE").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        });
        let cfg = if opted_out {
            tracing::info!(
                "web console disabled (PUNKTFUNK_WEB_CONSOLE in host.env) — not supervising it"
            );
            None
        } else if !cfg.bun.exists() || !cfg.server.exists() {
            // Host-only build. A payload appearing later comes from an installer, which restarts us.
            tracing::info!(
                bun = %cfg.bun.display(),
                server = %cfg.server.display(),
                "no web console payload — not supervising it"
            );
            None
        } else {
            Some(cfg)
        };
        let now = Instant::now();
        WebSlot {
            cfg,
            child: None,
            job: None,
            spawned_at: now,
            fast_exits: 0,
            next_spawn: now,
            password_deadline: None,
            logged_wait: false,
        }
    }

    /// Wait on `handles` for up to `ms`, (re)spawning the console and absorbing its exits.
    /// Same contract as `wait_any`.
    pub(super) fn wait(&mut self, handles: &[HANDLE], ms: u32) -> Option<usize> {
        let deadline =
            (ms != INFINITE).then(|| Instant::now() + Duration::from_millis(u64::from(ms)));
        loop {
            let own_wake = self.converge();
            let mut set: Vec<HANDLE> = handles.to_vec();
            if let Some(c) = &self.child {
                set.push(HANDLE(c.process.as_raw_handle()));
            }
            let caller_ms = match deadline {
                None => INFINITE,
                Some(d) => d
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            };
            let wait_ms = caller_ms.min(own_wake.unwrap_or(INFINITE));
            match wait_any(&set, wait_ms) {
                Some(i) if i < handles.len() => return Some(i),
                Some(_) => self.on_child_exit(),
                None => {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return None;
                    }
                    // Gate poll / respawn due — loop into `converge`.
                }
            }
        }
    }

    /// Spawn if due and gated. `None` = no wake appointment (child running, or nothing to supervise).
    fn converge(&mut self) -> Option<u32> {
        let Some(cfg) = &self.cfg else { return None };
        if self.child.is_some() {
            return None;
        }
        let now = Instant::now();
        if now < self.next_spawn {
            return Some(
                (self.next_spawn - now)
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            );
        }
        // Installer stops us before touching files; still do not spawn into a half-written `{app}`.
        if !cfg.bun.exists() || !cfg.server.exists() {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::warn!(
                    bun = %cfg.bun.display(),
                    "web console payload missing — an install/uninstall in progress? waiting"
                );
            }
            return Some(60_000);
        }
        // Host writes mgmt-token at argument parse and cert/key after RSA keygen. Hold start
        // until all three exist.
        let data = pf_paths::config_dir();
        let token = data.join("mgmt-token");
        let cert = data.join("cert.pem");
        let key = data.join("key.pem");
        if !(token.exists() && cert.exists() && key.exists()) {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::info!(
                    "waiting for the host to write its mgmt token + identity cert before starting \
                     the web console"
                );
            }
            return Some(1_000);
        }
        // Soft gate: give `web setup` time to write the login password on a fresh install.
        let password = data.join("web-password");
        if !password.exists() {
            let deadline = *self
                .password_deadline
                .get_or_insert_with(|| now + Duration::from_secs(60));
            if now < deadline {
                return Some(1_000);
            }
            // Start anyway: the console fail-closes until a password exists; a respawn picks it up.
        }
        self.spawn(&data);
        self.child.is_none().then(|| {
            (self.next_spawn.saturating_duration_since(Instant::now()))
                .as_millis()
                .min(u128::from(INFINITE)) as u32
        })
    }

    /// One attempt. `data` is the config dir already used by `converge`'s gates, so env paths match.
    fn spawn(&mut self, data: &Path) {
        let Some(cfg) = &self.cfg else { return };
        // Lazy: a console-less box never creates a job.
        if self.job.is_none() {
            match make_job(JOB_OBJECT_LIMIT(0)) {
                Ok(j) => self.job = Some(j),
                Err(e) => {
                    tracing::error!("create web console job object: {e:#}");
                    self.schedule_retry();
                    return;
                }
            }
        }
        let job = HANDLE(self.job.as_ref().expect("just set").as_raw_handle());
        match spawn_web(cfg, data, job) {
            Ok(child) => {
                tracing::info!(
                    pid = child.pid,
                    "web console launched (https://<host-ip>:47992)"
                );
                self.spawned_at = Instant::now();
                self.password_deadline = None;
                self.logged_wait = false;
                self.child = Some(child);
            }
            Err(e) => {
                tracing::error!("web console did not launch: {e:#}");
                self.schedule_retry();
            }
        }
    }

    fn on_child_exit(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let mut code: u32 = 0;
        // SAFETY: `child.process` is the live OwnedHandle of the just-signalled process (owned
        // until `child` drops at the end of this fn); `code` is a live local out-param.
        let _ = unsafe { GetExitCodeProcess(HANDLE(child.process.as_raw_handle()), &mut code) };
        let uptime = self.spawned_at.elapsed();
        if uptime >= WEB_GOOD_RUN {
            self.fast_exits = 0;
        }
        self.schedule_retry();
        tracing::warn!(
            pid = child.pid,
            exit_code = format!("{code:#x}"),
            uptime_secs = uptime.as_secs(),
            retry_in_ms = (self.next_spawn - Instant::now()).as_millis() as u64,
            "web console exited — relaunching"
        );
    }

    fn schedule_retry(&mut self) {
        self.fast_exits = self.fast_exits.saturating_add(1);
        let backoff_ms = (500u64 << (self.fast_exits - 1).min(7)).min(WEB_MAX_BACKOFF_MS);
        self.next_spawn = Instant::now() + Duration::from_millis(backoff_ms);
    }
}

/// One-line `KEY=VALUE` or a bare value — same as `mgmt_token::parse_token`.
pub(super) fn read_env_file_value(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The console password line as the env name it must keep plus its value. `web-password` carries
/// `PUNKTFUNK_UI_PASSWORD_HASH` once the console has migrated it, and the clear-text key only
/// until then — forwarding either under the other's name hands the console a hash to compare as a
/// password. The value stays as written; the console strips the quotes the hash is stored in.
pub(super) fn read_password_env(path: &Path) -> Option<(&'static str, String)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut hash = None;
    let mut clear = None;
    for line in contents.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "PUNKTFUNK_UI_PASSWORD_HASH" => hash = Some(value.to_string()),
            "PUNKTFUNK_UI_PASSWORD" => clear = Some(value.to_string()),
            _ => {}
        }
    }
    // Clear text wins, the rule the console's own compare follows: writing one back beside a hash
    // is how a password is reset, and the console hashes it again on the next sign-in.
    clear
        .map(|v| ("PUNKTFUNK_UI_PASSWORD", v))
        .or_else(|| hash.map(|v| ("PUNKTFUNK_UI_PASSWORD_HASH", v)))
}

/// This process's env plus `overrides` (case-insensitive win) as a double-NUL UTF-16 block.
/// Same serialization as `interactive::merged_env_block`; the base is ours, not a user token.
pub(super) fn env_block_with(overrides: &[(&str, String)]) -> Vec<u16> {
    let mut entries: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !overrides.iter().any(|(ok, _)| ok.eq_ignore_ascii_case(k)))
        .collect();
    entries.extend(overrides.iter().map(|(k, v)| (k.to_string(), v.clone())));
    // CreateProcess* requires the block sorted case-insensitively by name.
    entries.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in entries {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// bun serving the Nitro bundle in session 0, stdout → web.log, inside `job`. Env wiring
/// matches `scripts/punktfunk-web.service`.
pub(super) fn spawn_web(cfg: &WebConfig, data: &Path, job: HANDLE) -> Result<Child> {
    // Env-over-file, same as `mgmt_token.rs`: a host.env override must reach both processes.
    let token = std::env::var("PUNKTFUNK_MGMT_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-token")))
        .context("read mgmt-token")?;
    let pw_path = data.join("web-password");
    let password = std::env::var("PUNKTFUNK_UI_PASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| ("PUNKTFUNK_UI_PASSWORD", v))
        .or_else(|| read_password_env(&pw_path));
    // Env-over-file; `mgmt::publish_endpoint` rewrites the file each `serve`. Default 47990 is
    // last-resort — a Sunshine fork often owns that port as its web UI.
    let mgmt_url = std::env::var("PUNKTFUNK_MGMT_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-endpoint")))
        .unwrap_or_else(|| "https://127.0.0.1:47990".into());

    let mut overrides: Vec<(&str, String)> = vec![
        ("PORT", "47992".into()),
        // No HOST override: the bind is host.env's PUNKTFUNK_UI_BIND, which `load_host_env` has
        // already put in this process's environment, and the console defaults to loopback without
        // it. Overriding here would out-rank the file the operator edits.
        // Proxy hop to the host's loopback HTTPS mgmt API. Self-signed cert is accepted
        // per-request, never process-wide.
        ("PUNKTFUNK_MGMT_URL", mgmt_url),
        // Host identity cert; cookie is Secure. These names are the legacy pair — the console
        // prefers the native sibling (`web/nitro-entry/tls-paths.mjs`) when it exists.
        (
            "PUNKTFUNK_UI_TLS_CERT",
            data.join("cert.pem").to_string_lossy().into_owned(),
        ),
        (
            "PUNKTFUNK_UI_TLS_KEY",
            data.join("key.pem").to_string_lossy().into_owned(),
        ),
        ("PUNKTFUNK_UI_SECURE", "1".into()),
        ("PUNKTFUNK_MGMT_TOKEN", token),
        // The file the console rewrites when it turns a clear-text password into a salted hash.
        // It is reached through this name, not %ProgramData%, so the console needs no path rules.
        (
            "PUNKTFUNK_UI_PASSWORD_FILE",
            pw_path.to_string_lossy().into_owned(),
        ),
    ];
    if let Some((key, pw)) = password {
        overrides.push((key, pw));
    }
    let env = env_block_with(&overrides);

    let web_log = web_log_path();
    rotate_if_large(&web_log);
    let log = open_log_handle(&web_log)?;

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log,
        hStdError: log,
        ..Default::default()
    };
    let mut cmd: Vec<u16> = format!("\"{}\" \"{}\"", cfg.bun.display(), cfg.server.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd = HSTRING::from(cfg.web_dir.as_os_str());
    let mut pi = PROCESS_INFORMATION::default();

    // CREATE_SUSPENDED: assign to the job before the first instruction or children escape it.
    // SAFETY: `cmd`, `cwd`, `env` and `si` (whose handles are the live inheritable `log`) are live,
    // NUL-terminated locals for the call (`env` doubly, per `env_block_with`); `pi` is a live
    // local out-param; no pointer is retained past the call.
    let created = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true, // inherit handles
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            Some(env.as_ptr() as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &si,
            &mut pi,
        )
    };
    // SAFETY: `log` is live and owned here, closed exactly once and not used after — on success
    // the child holds its own inherited copy.
    let _ = unsafe { CloseHandle(log) };
    created.context("CreateProcessW(web console)")?;

    // Own the handles first so assignment failure still closes them via drop.
    // SAFETY: `created` was `Ok`, so `pi.hProcess` is an owned handle nothing else closes;
    // wrapping it transfers that ownership to the `OwnedHandle`, which closes it exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    // SAFETY: the same, for the distinct thread handle `CreateProcessW` filled in.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };
    let child = Child {
        process,
        _thread: thread,
        pid: pi.dwProcessId,
    };

    // Not best-effort: containment is the point of this job. Unassigned → do not run.
    // SAFETY: `job` is a live job object per this fn's contract; `child.process` is the live handle
    // of the still-suspended process just created.
    if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(child.process.as_raw_handle())) }
    {
        // SAFETY: `child.process` is live and owned (dropped at the end of this scope);
        // TerminateProcess only signals termination by handle. The process never ran (suspended).
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        return Err(e).context("AssignProcessToJobObject(web console)");
    }
    // SAFETY: `child._thread` is the live primary-thread handle of the process just created; a
    // suspended primary thread is exactly what ResumeThread expects.
    let resumed = unsafe { ResumeThread(HANDLE(child._thread.as_raw_handle())) };
    if resumed == u32::MAX {
        // SAFETY: live owned handle; process never ran (still suspended). Tear it down.
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        bail!("ResumeThread(web console) failed");
    }
    Ok(child)
}
