//! Folder access changes under way in this process. Windows rewrites every file below a folder and
//! reports nothing until it ends, minutes on a large tree, so whoever asked can see it is still
//! running: the console through [`of`], the CLI and a store job through [`current`].

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
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
}

struct Row {
    id: u64,
    plugin: Option<String>,
    path: String,
    removing: bool,
    started: Instant,
}

impl Row {
    fn walk(&self) -> AccessWalk {
        AccessWalk {
            path: self.path.clone(),
            removing: self.removing,
            seconds: self.started.elapsed().as_secs(),
        }
    }
}

static ROWS: Mutex<Vec<Row>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

fn rows() -> std::sync::MutexGuard<'static, Vec<Row>> {
    ROWS.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Listed until it drops.
#[cfg_attr(not(windows), allow(dead_code, reason = "only Windows walks a folder"))]
pub(crate) struct Walk(u64);

/// `plugin` is `None` for a converge, which changes a folder for every plugin that holds it.
#[cfg_attr(not(windows), allow(dead_code, reason = "only Windows walks a folder"))]
pub(crate) fn begin(plugin: Option<&str>, path: &str, removing: bool) -> Walk {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    rows().push(Row {
        id,
        plugin: plugin.map(str::to_string),
        path: path.to_string(),
        removing,
        started: Instant::now(),
    });
    Walk(id)
}

impl Drop for Walk {
    fn drop(&mut self) {
        rows().retain(|r| r.id != self.0);
    }
}

/// `plugin`'s walks under way.
pub(crate) fn of(plugin: &str) -> Vec<AccessWalk> {
    rows()
        .iter()
        .filter(|r| r.plugin.as_deref() == Some(plugin))
        .map(Row::walk)
        .collect()
}

/// The oldest walk under way, whoever it is for.
pub(crate) fn current() -> Option<AccessWalk> {
    rows().first().map(Row::walk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_is_listed_for_its_plugin_until_it_drops() {
        let walk = begin(Some("walks-test"), "/games", false);
        assert_eq!(of("walks-test")[0].path, "/games");
        assert!(of("walks-other").is_empty());
        drop(walk);
        assert!(of("walks-test").is_empty());
    }
}
