//! Title downloads. A plugin moves the bytes and reports snapshots; the host keeps state, speed
//! and time left, serves them to the console and clients, and holds a launch here until the
//! title it starts is on disk. Rows live in memory: a plugin restates its live set on every
//! report, so a host restart refills the table.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// No report for this long: the plugin is gone. Six missed 5 s heartbeats.
pub const SILENCE: Duration = Duration::from_secs(30);
/// `downloading` with no byte moved for this long: a read the plugin never timed out.
pub const BYTE_STALL: Duration = Duration::from_secs(300);
/// A finished, failed or cancelled row stays this long for the console.
const KEEP_TERMINAL: Duration = Duration::from_secs(600);
/// A row nothing has restated for this long goes, paused or not.
const FORGET: Duration = Duration::from_secs(24 * 3600);
/// Speed is the last 5 s; time left uses 30 s, so one slow burst doesn't swing it.
const RATE_WINDOW: Duration = Duration::from_secs(5);
const ETA_WINDOW: Duration = Duration::from_secs(30);
const PHASE_MAX: usize = 80;
const ERROR_MAX: usize = punktfunk_core::quic::LAUNCH_MESSAGE_MAX;

/// Whether a title's files are on this host, as the plugin that lists it reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Install {
    pub state: InstallState,
    /// Download size while `missing`, size on disk once `installed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u64, required = false)]
    pub size_bytes: Option<u64>,
    /// The folder the files go into. Must lie under one of the plugin's write grants; the
    /// operator's lane only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Free space under `target`, filled in by the host when it serves the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u64, required = false)]
    pub free_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Missing,
    /// Also what a state this host doesn't know reads as.
    #[default]
    #[serde(other)]
    Installed,
}

impl Install {
    pub fn missing(&self) -> bool {
        self.state == InstallState::Missing
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    /// Waiting for its turn (the store's own queue), or asked and not reported yet.
    Queued,
    Downloading,
    /// Stopped with its partial files kept; a start resumes it.
    Paused,
    /// Unpacking or verifying: no bytes move.
    Installing,
    Done,
    Failed,
    /// Stopped with its partial files discarded.
    Cancelled,
}

impl DownloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Installing => "installing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Making progress, or expected to.
    pub fn live(self) -> bool {
        matches!(self, Self::Queued | Self::Downloading | Self::Installing)
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// One title in a plugin's report.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct DownloadReport {
    pub external_id: String,
    pub state: DownloadState,
    #[serde(default)]
    pub done_bytes: u64,
    /// Absent while the size isn't known yet.
    #[serde(default)]
    #[schema(value_type = u64, required = false)]
    pub total_bytes: Option<u64>,
    /// What the plugin is doing, in its words: `File 2 of 3`, `Verifying`. At most 80 chars.
    #[serde(default)]
    pub phase: Option<String>,
    /// Why it failed, one sentence for the player.
    #[serde(default)]
    pub error: Option<String>,
}

/// One title's download as the host serves it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Download {
    /// Library id, as `GET /library` lists it.
    pub app_id: String,
    pub title: String,
    pub state: DownloadState,
    pub done_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u64, required = false)]
    pub total_bytes: Option<u64>,
    /// Bytes per second over the last 5 s, while downloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u64, required = false)]
    pub rate_bps: Option<u64>,
    /// Seconds left at the last 30 s's pace, while downloading with a known total.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u64, required = false)]
    pub eta_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339.
    pub updated_at: String,
    /// Who asked for it: `console`, a client's label. Absent when the plugin started it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

struct Row {
    view: Download,
    /// Last report, or the host's own start.
    reported: Instant,
    /// Last change of `done_bytes`.
    moved: Instant,
    samples: VecDeque<(Instant, u64)>,
    terminal_at: Option<Instant>,
}

impl Row {
    fn stalled(&self, now: Instant) -> bool {
        self.view.state.live()
            && (now.duration_since(self.reported) > SILENCE
                || (self.view.state == DownloadState::Downloading
                    && now.duration_since(self.moved) > BYTE_STALL))
    }

    /// Bytes per second since the first sample inside `window`, measured up to `now`, so a
    /// download that stopped moving slows to zero instead of keeping its last speed.
    fn pace(&self, now: Instant, window: Duration) -> Option<u64> {
        let (_, newest) = *self.samples.back()?;
        let (t0, b0) = *self
            .samples
            .iter()
            .find(|(t, _)| now.duration_since(*t) <= window)?;
        let dt = now.duration_since(t0).as_secs_f64();
        (dt >= 1.0).then(|| (newest.saturating_sub(b0) as f64 / dt) as u64)
    }

    fn served(&self, now: Instant) -> Download {
        let mut d = self.view.clone();
        if d.state == DownloadState::Downloading {
            d.rate_bps = Some(self.pace(now, RATE_WINDOW).unwrap_or(0));
            d.eta_s = d.total_bytes.and_then(|total| {
                let pace = self.pace(now, ETA_WINDOW).filter(|p| *p > 0)?;
                Some(total.saturating_sub(d.done_bytes) / pace)
            });
        }
        d
    }
}

#[derive(Default)]
struct Rows {
    by_app: HashMap<String, Row>,
    /// Held while any row is live, so an idle timer doesn't suspend the host mid-download.
    #[cfg(target_os = "linux")]
    inhibit: Option<crate::sleep_inhibit::StreamHold>,
    #[cfg(windows)]
    inhibit: Option<pf_frame::session_tuning::SystemWakeRequest>,
}

impl Rows {
    fn prune(&mut self, now: Instant) {
        self.by_app.retain(|_, r| {
            r.terminal_at
                .is_none_or(|at| now.duration_since(at) < KEEP_TERMINAL)
                && now.duration_since(r.reported) < FORGET
        });
    }

    #[cfg(any(target_os = "linux", windows))]
    fn settle_inhibit(&mut self) {
        let live = self.by_app.values().any(|r| r.view.state.live());
        match (live, self.inhibit.is_some()) {
            #[cfg(target_os = "linux")]
            (true, false) => self.inhibit = Some(crate::sleep_inhibit::hold()),
            // Not `StreamHold`: on Windows that also pauses Instant Replay.
            #[cfg(windows)]
            (true, false) => {
                self.inhibit =
                    pf_frame::session_tuning::SystemWakeRequest::new("punktfunk downloading a game")
            }
            (false, true) => self.inhibit = None,
            _ => {}
        }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    fn settle_inhibit(&mut self) {}
}

struct Table {
    rows: Mutex<Rows>,
    changed: Condvar,
}

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| Table {
        rows: Mutex::new(Rows::default()),
        changed: Condvar::new(),
    })
}

fn rows() -> std::sync::MutexGuard<'static, Rows> {
    table().rows.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

/// Trimmed, no control characters, at most `max` bytes on a char boundary; `None` when empty.
fn clean(s: Option<&str>, max: usize) -> Option<String> {
    let mut out: String = s?.trim().chars().filter(|c| !c.is_control()).collect();
    while out.len() > max {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

/// `app`'s files are gone: its row goes, and listeners hear `removed`.
pub fn removed(app: &str, title: &str) {
    rows().by_app.remove(app);
    table().changed.notify_all();
    changed(app, title, "removed");
}

fn changed(app: &str, title: &str, state: &str) {
    crate::events::emit(crate::events::EventKind::DownloadsChanged {
        app: app.to_string(),
        title: title.to_string(),
        state: state.to_string(),
    });
}

/// `provider`'s titles by `external_id`: library id and title. Only published entries resolve,
/// so a report can't name a title it doesn't own.
fn owned_by(provider: &str) -> HashMap<String, (String, String)> {
    super::load_custom()
        .into_iter()
        .filter(|e| e.provider.as_deref() == Some(provider))
        .filter_map(|e| {
            let external = e.external_id.clone()?;
            Some((external, (super::library_id_for(&e), e.title.clone())))
        })
        .collect()
}

/// Apply `provider`'s report. Returns how many rows matched one of its titles; the rest are
/// dropped. Rows it leaves out age toward [`SILENCE`].
pub fn report(provider: &str, reports: Vec<DownloadReport>) -> usize {
    let owned = owned_by(provider);
    let now = Instant::now();
    let stamp = now_rfc3339();
    let mut events = Vec::new();
    let mut matched = 0;
    {
        let mut rows = rows();
        for rep in reports {
            let Some((app, title)) = owned.get(&rep.external_id) else {
                tracing::debug!(provider, external_id = %rep.external_id, "download report for a title this provider doesn't list");
                continue;
            };
            matched += 1;
            let row = rows.by_app.entry(app.clone()).or_insert_with(|| Row {
                view: Download {
                    app_id: app.clone(),
                    title: title.clone(),
                    state: rep.state,
                    done_bytes: rep.done_bytes,
                    total_bytes: None,
                    rate_bps: None,
                    eta_s: None,
                    phase: None,
                    error: None,
                    started_at: stamp.clone(),
                    updated_at: stamp.clone(),
                    by: None,
                },
                reported: now,
                moved: now,
                samples: VecDeque::new(),
                terminal_at: None,
            });
            let was = row.view.state;
            if was.terminal() && !rep.state.terminal() {
                // A retry: a new run of the same title.
                row.view.started_at = stamp.clone();
                row.samples.clear();
            }
            if rep.done_bytes != row.view.done_bytes {
                row.moved = now;
            }
            if rep.state == DownloadState::Downloading {
                row.samples.push_back((now, rep.done_bytes));
                while row
                    .samples
                    .front()
                    .is_some_and(|(t, _)| now.duration_since(*t) > ETA_WINDOW)
                {
                    row.samples.pop_front();
                }
            } else {
                row.samples.clear();
            }
            row.view.state = rep.state;
            row.view.done_bytes = rep.done_bytes;
            row.view.total_bytes = rep.total_bytes;
            row.view.phase = clean(rep.phase.as_deref(), PHASE_MAX);
            row.view.error = clean(rep.error.as_deref(), ERROR_MAX);
            row.view.updated_at = stamp.clone();
            row.reported = now;
            row.terminal_at = match (rep.state.terminal(), row.terminal_at) {
                (true, None) => Some(now),
                (true, at) => at,
                (false, _) => None,
            };
            if was != rep.state {
                events.push((app.clone(), title.clone(), rep.state.as_str()));
            }
        }
        rows.prune(now);
        rows.settle_inhibit();
    }
    table().changed.notify_all();
    for (app, title, state) in events {
        changed(&app, &title, state);
    }
    matched
}

/// The host asked a plugin to start `app` and it said yes: a `queued` row until its first
/// report, so a launch waiting on it is held to [`SILENCE`] from now. A live row is left alone.
pub fn begin(app: &str, title: &str, by: Option<String>) {
    let now = Instant::now();
    let stamp = now_rfc3339();
    let fresh = {
        let mut rows = rows();
        let fresh = match rows.by_app.get_mut(app) {
            Some(r) if r.view.state.live() => false,
            Some(r) => {
                r.view.state = DownloadState::Queued;
                r.view.error = None;
                r.view.started_at = stamp.clone();
                r.view.updated_at = stamp;
                r.view.by = by;
                r.reported = now;
                r.moved = now;
                r.samples.clear();
                r.terminal_at = None;
                true
            }
            None => {
                rows.by_app.insert(
                    app.to_string(),
                    Row {
                        view: Download {
                            app_id: app.to_string(),
                            title: title.to_string(),
                            state: DownloadState::Queued,
                            done_bytes: 0,
                            total_bytes: None,
                            rate_bps: None,
                            eta_s: None,
                            phase: None,
                            error: None,
                            started_at: stamp.clone(),
                            updated_at: stamp,
                            by,
                        },
                        reported: now,
                        moved: now,
                        samples: VecDeque::new(),
                        terminal_at: None,
                    },
                );
                true
            }
        };
        rows.settle_inhibit();
        fresh
    };
    table().changed.notify_all();
    if fresh {
        changed(app, title, DownloadState::Queued.as_str());
    }
}

/// Every row, live ones first.
pub fn snapshot() -> Vec<Download> {
    let now = Instant::now();
    let mut rows = rows();
    rows.prune(now);
    let mut out: Vec<Download> = rows.by_app.values().map(|r| r.served(now)).collect();
    out.sort_by(|a, b| {
        b.state
            .live()
            .cmp(&a.state.live())
            .then_with(|| a.started_at.cmp(&b.started_at))
    });
    out
}

/// `app`'s row, if it has one.
pub fn get(app: &str) -> Option<Download> {
    let now = Instant::now();
    rows().by_app.get(app).map(|r| r.served(now))
}

/// Whether `app` has a download a launch should wait for or resume.
pub fn pending(app: &str) -> bool {
    rows()
        .by_app
        .get(app)
        .is_some_and(|r| r.view.state.live() || r.view.state == DownloadState::Paused)
}

/// How a launch's wait for its title ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Waited {
    Done,
    Failed(Option<String>),
    Cancelled,
    Paused,
    Stalled,
    /// The session ended first. The download goes on.
    Gone,
}

impl Waited {
    /// The sentence the player gets for a launch that didn't happen; `None` for `Done` and
    /// `Gone`, which need none.
    pub fn sentence(&self, title: &str) -> Option<String> {
        Some(match self {
            Self::Done | Self::Gone => return None,
            Self::Failed(Some(why)) => format!("{title} didn't download — {why}"),
            Self::Failed(None) => format!("{title} didn't download."),
            Self::Cancelled => format!("{title}'s download was cancelled."),
            Self::Paused => format!("{title}'s download was paused. Start it again to resume."),
            Self::Stalled => {
                format!("{title} stopped downloading. Start it again to try once more.")
            }
        })
    }
}

/// Block until `app`'s download ends, stalls, or `gone` says the session left. Checks `gone`
/// once a second. Call on a blocking thread.
pub fn wait(app: &str, gone: &dyn Fn() -> bool) -> Waited {
    let t = table();
    let mut rows = t.rows.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = Instant::now();
        let outcome = match rows.by_app.get(app) {
            None => Some(Waited::Stalled),
            Some(r) => match r.view.state {
                DownloadState::Done => Some(Waited::Done),
                DownloadState::Failed => Some(Waited::Failed(r.view.error.clone())),
                DownloadState::Cancelled => Some(Waited::Cancelled),
                DownloadState::Paused => Some(Waited::Paused),
                _ if r.stalled(now) => Some(Waited::Stalled),
                _ => None,
            },
        };
        if let Some(o) = outcome {
            return o;
        }
        if gone() {
            return Waited::Gone;
        }
        rows = t
            .changed
            .wait_timeout(rows, Duration::from_secs(1))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// What the host asks of a plugin at `POST /__install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Pause,
    Cancel,
    Uninstall,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Cancel => "cancel",
            Self::Uninstall => "uninstall",
        }
    }

    /// Removing files can take a while; the rest only start or stop work.
    fn timeout(self) -> Duration {
        match self {
            Self::Uninstall => Duration::from_secs(120),
            _ => Duration::from_secs(15),
        }
    }
}

/// Why a plugin didn't do what the host asked.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The plugin isn't running, or doesn't serve installs.
    NotServed,
    /// Not one of the plugin's titles.
    NotMine,
    /// The plugin's own sentence: no room, no write grant, nothing to remove.
    Said(String),
    /// No answer, or one the host can't read.
    Unreachable(String),
}

/// Ask `provider` to do `action` on `app`. Blocking.
pub fn call(provider: &str, app: &str, external_id: &str, action: Action) -> Result<(), Refusal> {
    let Some(cred) = crate::mgmt::plugins::installer(provider) else {
        return Err(Refusal::NotServed);
    };
    // No proxy: the secret must never leave loopback. No redirects, as for holds.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(action.timeout()))
        .http_status_as_error(false)
        .build()
        .into();
    let body = serde_json::json!({
        "app": app,
        "external_id": external_id,
        "action": action.as_str(),
    });
    // A plugin on the host's channel (port 0) is reached through it instead of a port.
    let (status, text) = if cred.port == 0 {
        crate::mgmt::plugin_channel::request_blocking(
            provider,
            "/__install",
            &body.to_string(),
            action.timeout(),
        )
        .map_err(|e| Refusal::Unreachable(e.to_string()))?
    } else {
        let mut res = agent
            .post(&format!("http://127.0.0.1:{}/__install", cred.port))
            .header("Authorization", &format!("Bearer {}", cred.secret))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| Refusal::Unreachable(e.to_string()))?;
        let status = res.status().as_u16();
        (status, res.body_mut().read_to_string().unwrap_or_default())
    };
    match status {
        200..=299 => Ok(()),
        404 => Err(Refusal::NotMine),
        409 => {
            let said = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(str::to_string));
            Err(Refusal::Said(
                clean(said.as_deref(), ERROR_MAX)
                    .unwrap_or_else(|| "The plugin refused without saying why.".into()),
            ))
        }
        _ => Err(Refusal::Unreachable(format!("answered {status}"))),
    }
}

/// Fill each entry's `free_bytes` from its `target`, which must lie under one of its plugin's
/// write grants; a target outside them is dropped. One grant read and one stat per target.
pub fn fill_free<'a>(games: impl IntoIterator<Item = &'a mut GameEntry>) {
    let mut access = None;
    // (provider, target) → free bytes when granted, `None` when not.
    let mut seen: HashMap<(String, String), Option<Option<u64>>> = HashMap::new();
    for g in games {
        let provider = g.provider.clone().unwrap_or_default();
        let Some(install) = g.install.as_mut() else {
            continue;
        };
        install.free_bytes = None;
        let Some(target) = install.target.clone() else {
            continue;
        };
        let answer = seen
            .entry((provider.clone(), target.clone()))
            .or_insert_with(|| {
                let access = access.get_or_insert_with(|| {
                    crate::plugins::access::AccessStore::open(pf_paths::seat::library_dir())
                });
                (!provider.is_empty() && access.may_write(&provider, Path::new(&target)))
                    .then(|| free_bytes(Path::new(&target)))
            });
        match answer {
            Some(free) => install.free_bytes = *free,
            None => install.target = None,
        }
    }
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c` is a NUL-terminated path that outlives the call, and `s` is writable storage
    // for one `statvfs`, which the call fills completely when it returns 0.
    if unsafe { libc::statvfs(c.as_ptr(), s.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call returned 0, so it initialised `s`.
    let s = unsafe { s.assume_init() };
    // The field widths differ by platform: a cast one needs is a no-op on another.
    #[allow(clippy::unnecessary_cast)]
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(windows)]
fn free_bytes(path: &Path) -> Option<u64> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free: u64 = 0;
    // SAFETY: `HSTRING` is a valid path that outlives the call; `free` is a live local.
    // The API retains neither.
    unsafe {
        GetDiskFreeSpaceExW(
            &HSTRING::from(path.as_os_str()),
            Some(&mut free),
            None,
            None,
        )
    }
    .ok()?;
    Some(free)
}

#[cfg(not(any(unix, windows)))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: DownloadState, reported_ago: u64, moved_ago: u64) -> Row {
        let now = Instant::now();
        Row {
            view: Download {
                app_id: "a".into(),
                title: "T".into(),
                state,
                done_bytes: 0,
                total_bytes: Some(1000),
                rate_bps: None,
                eta_s: None,
                phase: None,
                error: None,
                started_at: String::new(),
                updated_at: String::new(),
                by: None,
            },
            reported: now - Duration::from_secs(reported_ago),
            moved: now - Duration::from_secs(moved_ago),
            samples: VecDeque::new(),
            terminal_at: None,
        }
    }

    #[test]
    fn a_silent_plugin_or_a_frozen_read_stalls_and_a_pause_never_does() {
        let now = Instant::now();
        assert!(!row(DownloadState::Downloading, 5, 60).stalled(now));
        assert!(row(DownloadState::Downloading, 31, 0).stalled(now));
        assert!(row(DownloadState::Downloading, 1, 301).stalled(now));
        // Unpacking moves no bytes; only silence counts.
        assert!(!row(DownloadState::Installing, 1, 600).stalled(now));
        assert!(row(DownloadState::Queued, 31, 0).stalled(now));
        assert!(!row(DownloadState::Paused, 3600, 3600).stalled(now));
    }

    #[test]
    fn speed_falls_to_zero_when_bytes_stop_and_time_left_follows_the_long_window() {
        let now = Instant::now();
        let mut r = row(DownloadState::Downloading, 0, 0);
        r.view.done_bytes = 400;
        r.samples = VecDeque::from([
            (now - Duration::from_secs(20), 0),
            (now - Duration::from_secs(4), 300),
            (now - Duration::from_secs(1), 400),
        ]);
        let d = r.served(now);
        // 300 → 400 over the 4 s since the first sample in the 5 s window.
        assert_eq!(d.rate_bps, Some(25));
        // 400 bytes in 20 s, 600 left.
        assert_eq!(d.eta_s, Some(30));
        r.samples = VecDeque::from([(now - Duration::from_secs(10), 400)]);
        assert_eq!(r.served(now).rate_bps, Some(0));
    }

    #[test]
    fn the_player_hears_why_a_launch_did_not_happen() {
        assert_eq!(Waited::Done.sentence("Quail"), None);
        assert_eq!(Waited::Gone.sentence("Quail"), None);
        assert_eq!(
            Waited::Failed(Some("the server is down".into())).sentence("Quail"),
            Some("Quail didn't download — the server is down".into())
        );
        assert!(Waited::Stalled
            .sentence("Quail")
            .unwrap()
            .starts_with("Quail stopped"));
    }

    #[test]
    fn plugin_text_is_trimmed_stripped_and_capped() {
        assert_eq!(clean(Some("  a\nb\u{7}  "), 80).as_deref(), Some("ab"));
        assert_eq!(clean(Some("   "), 80), None);
        assert_eq!(clean(Some("éééé"), 5).as_deref(), Some("éé"));
    }

    #[test]
    fn an_unknown_install_state_reads_as_installed() {
        let i: Install = serde_json::from_str(r#"{"state":"update"}"#).unwrap();
        assert_eq!(i.state, InstallState::Installed);
        let i: Install = serde_json::from_str(r#"{"state":"missing","size_bytes":5}"#).unwrap();
        assert!(i.missing());
    }

    /// The table is process-wide: each test uses its own app ids.
    fn put(app: &str, state: DownloadState) {
        let mut r = row(state, 0, 0);
        r.view.app_id = app.into();
        rows().by_app.insert(app.into(), r);
        table().changed.notify_all();
    }

    fn set(app: &str, state: DownloadState) {
        rows().by_app.get_mut(app).unwrap().view.state = state;
        table().changed.notify_all();
    }

    #[test]
    fn a_wait_ends_on_its_title_and_wakes_when_the_title_finishes() {
        put("wait:done", DownloadState::Done);
        assert_eq!(wait("wait:done", &|| false), Waited::Done);
        put("wait:paused", DownloadState::Paused);
        assert_eq!(wait("wait:paused", &|| false), Waited::Paused);
        put("wait:gone", DownloadState::Downloading);
        assert_eq!(wait("wait:gone", &|| true), Waited::Gone);
        assert_eq!(wait("wait:nothing", &|| false), Waited::Stalled);

        put("wait:late", DownloadState::Downloading);
        let waiter = std::thread::spawn(|| wait("wait:late", &|| false));
        std::thread::sleep(Duration::from_millis(50));
        set("wait:late", DownloadState::Done);
        assert_eq!(waiter.join().unwrap(), Waited::Done);
    }

    #[test]
    fn a_start_resumes_a_paused_row_and_leaves_a_live_one_alone() {
        put("begin:paused", DownloadState::Paused);
        begin("begin:paused", "T", Some("console".into()));
        let d = get("begin:paused").unwrap();
        assert_eq!(d.state, DownloadState::Queued);
        assert_eq!(d.by.as_deref(), Some("console"));
        assert!(pending("begin:paused"));

        put("begin:live", DownloadState::Downloading);
        begin("begin:live", "T", None);
        assert_eq!(get("begin:live").unwrap().state, DownloadState::Downloading);

        begin("begin:new", "T", None);
        assert!(get("begin:new").is_some());
        removed("begin:new", "T");
        assert!(get("begin:new").is_none());
    }
}
