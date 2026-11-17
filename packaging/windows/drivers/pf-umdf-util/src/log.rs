//! The bring-up file log every punktfunk UMDF driver keeps, in one place.
//!
//! One copy serves every driver, so a hardening of the sink reaches all of them.
//!
//! The path is the decision worth owning. WUDFHost's own (LocalService) temp dir — never
//! `C:\Users\Public`, where a non-admin could pre-create or hold the file, or read diagnostics
//! that carry per-pad identity material. The whole sink is OPT-IN besides: a release driver
//! writes nothing unless its knob is set, so neither the file nor the debugger trap exists on a
//! player's machine. DebugView cannot see a UMDF host across session 0, which is why the file is
//! the bring-up diagnostic at all.
//!
//! Each driver keeps its own gate — the env name is its own, and `pf-vdisplay`'s also reads the
//! machine registry — and hands it in as a `fn` pointer.

use std::fs::File;
use std::sync::{Mutex, OnceLock};

use wdk_sys::windows::OutputDebugStringA;

/// One driver's log file, opened at most once for the life of the process.
pub struct FileLog {
    file_name: &'static str,
    gate: fn() -> bool,
    on: OnceLock<bool>,
    appender: OnceLock<Option<Mutex<File>>>,
}

impl FileLog {
    /// `file_name` is a leaf, joined onto the host's temp dir — never a path. `gate` is resolved
    /// once, so a knob edited mid-session is stale.
    #[must_use]
    pub const fn new(file_name: &'static str, gate: fn() -> bool) -> Self {
        Self {
            file_name,
            gate,
            on: OnceLock::new(),
            appender: OnceLock::new(),
        }
    }

    /// Whether the syscall sinks are on. Callers check this before building a line: the
    /// pre-check skips the `format!` alloc too, which is why the per-report hex dumps cost
    /// nothing in a release driver.
    pub fn enabled(&self) -> bool {
        *self.on.get_or_init(self.gate)
    }

    /// Tee one line to the debugger and the file. No-op unless [`Self::enabled`].
    ///
    /// The file line carries a UTC stamp and is flushed: the host logs UTC too, and without a
    /// shared clock a driver line cannot be placed against the host event it explains — which is
    /// the whole question when frames stop. Flushing keeps the tail across a crash or a stall.
    /// The debugger string stays bare, because DebugView stamps its own.
    pub fn write(&self, line: &str) {
        if !self.enabled() {
            return;
        }
        if let Ok(c) = std::ffi::CString::new(line) {
            // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
            unsafe { OutputDebugStringA(c.as_ptr().cast()) };
        }
        use std::io::Write;
        if let Some(m) = self.appender()
            && let Ok(mut f) = m.lock()
        {
            let _ = writeln!(f, "{} {line}", utc_hms_millis());
            let _ = f.flush();
        }
    }

    /// Process-lifetime append handle, shared through a `Mutex` so a driver's worker threads
    /// write too. Per-call open/append raced the control thread and could fail under a worker's
    /// restricted token, hiding exactly the lines a repro needs.
    fn appender(&self) -> Option<&Mutex<File>> {
        self.appender
            .get_or_init(|| {
                if !self.enabled() {
                    return None;
                }
                File::options()
                    .create(true)
                    .append(true)
                    .open(std::env::temp_dir().join(self.file_name))
                    .ok()
                    .map(Mutex::new)
            })
            .as_ref()
    }
}

/// `HH:MM:SS.mmm` UTC. Date-free: same-day alignment is what a session needs.
fn utc_hms_millis() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        now.subsec_millis()
    )
}

/// The driver's one [`FileLog`] plus the two helpers every driver wrote around it: `log(s)`
/// writes unconditionally through the gate, and [`dbglog!`](crate::dbglog) formats only when
/// the log is on. `$env` is the driver's own opt-in variable; a debug build is always on.
#[macro_export]
macro_rules! file_log {
    ($file:literal, $env:literal) => {
        static FILE_LOG: $crate::log::FileLog = $crate::log::FileLog::new($file, || {
            cfg!(debug_assertions) || std::env::var_os($env).is_some()
        });

        fn file_log_enabled() -> bool {
            FILE_LOG.enabled()
        }

        fn log(s: &str) {
            FILE_LOG.write(s);
        }
    };
}

/// Format and write one line, only when the driver's [`file_log!`](crate::file_log) is on: the
/// gate runs before `format!`, so a release driver pays nothing per line.
#[macro_export]
macro_rules! dbglog {
    ($($a:tt)*) => {
        if file_log_enabled() {
            log(&format!($($a)*))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp is what lets a driver line be placed against the host event it explains, so it
    /// must be fixed-width and wrap at midnight rather than run past 24 h.
    #[test]
    fn the_stamp_is_fixed_width_hms_millis() {
        let s = utc_hms_millis();
        assert_eq!(s.len(), 12, "HH:MM:SS.mmm — got {s:?}");
        let (h, rest) = s.split_at(2);
        assert!(h.parse::<u32>().is_ok_and(|h| h < 24), "hours: {s:?}");
        assert!(rest.starts_with(':'), "separator: {s:?}");
    }
}
