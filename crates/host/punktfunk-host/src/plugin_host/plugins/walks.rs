//! Access changes under way in this process. Windows rewrites every file below a folder and
//! reports nothing until it ends, minutes on a large tree, and an emulator a row asks for
//! downloads first, so whoever asked can see it is still running: the console through [`of`],
//! the CLI and a store job through [`current`]. An install that failed stays listed in
//! [`failures_of`] until its next try.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use utoipa::ToSchema;

/// One folder whose access is changing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccessWalk {
    pub path: String,
    /// The folder is being taken back, not opened.
    pub removing: bool,
    /// How long it has been running.
    pub seconds: u64,
    /// The emulator or cores the row asks for are installing; the grant follows.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    pub installing: bool,
    /// How much of the install's download is in, once its size is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    pub percent: Option<u8>,
}

/// An install a row asked for that did not land, until the row's next try.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct AccessFailure {
    pub path: String,
    /// What went wrong, as the installer said it.
    pub error: String,
}

/// [`Row::percent`] before the download's size is known.
const UNKNOWN: u8 = u8::MAX;
/// Failures kept across every plugin; the oldest drops.
const FAILURES_KEPT: usize = 32;

struct Row {
    id: u64,
    plugin: Option<String>,
    path: String,
    removing: bool,
    installing: bool,
    percent: Arc<AtomicU8>,
    started: Instant,
}

impl Row {
    fn walk(&self) -> AccessWalk {
        let percent = self.percent.load(Ordering::Relaxed);
        AccessWalk {
            path: self.path.clone(),
            removing: self.removing,
            seconds: self.started.elapsed().as_secs(),
            installing: self.installing,
            percent: (percent != UNKNOWN).then_some(percent),
        }
    }
}

static ROWS: Mutex<Vec<Row>> = Mutex::new(Vec::new());
static FAILED: Mutex<Vec<(String, AccessFailure)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

fn rows() -> std::sync::MutexGuard<'static, Vec<Row>> {
    ROWS.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn failed_rows() -> std::sync::MutexGuard<'static, Vec<(String, AccessFailure)>> {
    FAILED.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Listed until it drops.
pub(crate) struct Walk {
    id: u64,
    percent: Arc<AtomicU8>,
}

fn push(plugin: Option<&str>, path: &str, removing: bool, installing: bool) -> Walk {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let percent = Arc::new(AtomicU8::new(UNKNOWN));
    rows().push(Row {
        id,
        plugin: plugin.map(str::to_string),
        path: path.to_string(),
        removing,
        installing,
        percent: Arc::clone(&percent),
        started: Instant::now(),
    });
    Walk { id, percent }
}

/// `plugin` is `None` for a converge, which changes a folder for every plugin that holds it.
#[cfg_attr(not(windows), allow(dead_code, reason = "only Windows walks a folder"))]
pub(crate) fn begin(plugin: Option<&str>, path: &str, removing: bool) -> Walk {
    push(plugin, path, removing, false)
}

/// The install `plugin`'s row on `path` asked for; a failure of its last try is dropped.
pub(crate) fn install(plugin: &str, path: &str) -> Walk {
    failed_rows().retain(|(p, f)| !(p == plugin && f.path == path));
    push(Some(plugin), path, false, true)
}

impl Walk {
    /// The install's progress sink: the download's share, from its size when the server gives
    /// one, else from the size the channel resolved.
    pub(crate) fn meter(&self) -> impl Fn(hermir::progress::Event) + Send + Sync + 'static {
        use hermir::progress::Event;
        let (percent, size) = (Arc::clone(&self.percent), AtomicU64::new(0));
        move |event| match event {
            Event::Resolved { size: Some(s), .. } => size.store(s, Ordering::Relaxed),
            Event::Download { done, total } => {
                let total = total.unwrap_or_else(|| size.load(Ordering::Relaxed));
                if let Some(share) = done.min(total).saturating_mul(100).checked_div(total) {
                    percent.store(share as u8, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        rows().retain(|r| r.id != self.id);
    }
}

/// `plugin`'s install on `path` did not land.
pub(crate) fn failed(plugin: &str, path: &str, error: String) {
    let mut all = failed_rows();
    all.retain(|(p, f)| !(p == plugin && f.path == path));
    let failure = AccessFailure {
        path: path.to_string(),
        error,
    };
    all.push((plugin.to_string(), failure));
    let over = all.len().saturating_sub(FAILURES_KEPT);
    all.drain(..over);
}

/// `plugin`'s walks under way.
pub(crate) fn of(plugin: &str) -> Vec<AccessWalk> {
    rows()
        .iter()
        .filter(|r| r.plugin.as_deref() == Some(plugin))
        .map(Row::walk)
        .collect()
}

/// `plugin`'s installs that failed, each until its next try.
pub(crate) fn failures_of(plugin: &str) -> Vec<AccessFailure> {
    let all = failed_rows();
    all.iter()
        .filter(|(p, _)| p == plugin)
        .map(|(_, f)| f.clone())
        .collect()
}

/// The oldest walk under way, whoever it is for.
pub(crate) fn current() -> Option<AccessWalk> {
    rows().first().map(Row::walk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermir::progress::Event;

    #[test]
    fn a_walk_is_listed_for_its_plugin_until_it_drops() {
        let walk = begin(Some("walks-test"), "/games", false);
        assert_eq!(of("walks-test")[0].path, "/games");
        assert!(of("walks-other").is_empty());
        drop(walk);
        assert!(of("walks-test").is_empty());
    }

    /// The share comes from the server's size, else the resolved one; a failure lasts until
    /// the next try.
    #[test]
    fn an_install_shows_its_download_and_keeps_its_failure_until_the_next_try() {
        let walk = install("walks-install", "/emu");
        let meter = walk.meter();
        assert_eq!(of("walks-install")[0].percent, None);
        meter(Event::Resolved {
            release: "1".into(),
            file_name: None,
            size: Some(200),
        });
        meter(Event::Download {
            done: 50,
            total: None,
        });
        assert_eq!(of("walks-install")[0].percent, Some(25));
        meter(Event::Download {
            done: 90,
            total: Some(100),
        });
        let listed = of("walks-install").remove(0);
        assert!(listed.installing && listed.percent == Some(90));
        drop(walk);
        failed("walks-install", "/emu", "no network".into());
        assert_eq!(failures_of("walks-install")[0].error, "no network");
        let _again = install("walks-install", "/emu");
        assert!(failures_of("walks-install").is_empty());
    }
}
