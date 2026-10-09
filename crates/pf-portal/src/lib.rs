//! Process-lifetime tokio runtime for every portal call in the host.
//!
//! ashpd caches its D-Bus connection in a process-global `OnceLock`. The first
//! portal proxy creates it, and zbus spawns the connection's reader on
//! whichever tokio runtime is current at that moment.
//!
//! A per-session runtime that is dropped at teardown leaves that cached
//! connection with no executor. Every later portal call in the process then
//! waits for a reply nothing is left alive to read.
//!
//! Never build a per-session runtime, and never drop this one. `block_on`
//! takes `&self`, so every portal thread can park on it concurrently: capture,
//! the virtual displays and the libei injector. A portal session made here
//! outlives the thread that made it: close it explicitly (`close_session`).
//!
//! [`cursor_mode`] is the pure ScreenCast cursor ladder, compiled on every
//! target so its tests run without a compositor.

pub mod cursor_mode;
#[cfg(target_os = "linux")]
mod handshake;
#[cfg(target_os = "linux")]
pub use handshake::{
    close_session, finish_or_close, negotiate_cursor_mode, to_ashpd, within, CAST_CLOSE_BUDGET,
    HANDSHAKE_BUDGET,
};

#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use tokio::runtime::Runtime;

/// `Result` so a failed build fails the handshake with a reason instead of aborting the process.
#[cfg(target_os = "linux")]
static PORTAL_RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();

/// Multi-thread, 2 workers: the zbus reader must run across `create_session`
/// → `select_sources` → `start` while a portal thread blocks on `block_on`.
/// A current-thread runtime cannot pump that.
#[cfg(target_os = "linux")]
pub fn portal_runtime() -> Result<&'static Runtime, String> {
    match PORTAL_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("punktfunk-portal-rt")
            .enable_all()
            .build()
    }) {
        Ok(rt) => Ok(rt),
        Err(e) => Err(format!("build the shared portal runtime: {e}")),
    }
}
