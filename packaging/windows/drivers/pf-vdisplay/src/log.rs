//! Minimal driver logger. Every line lands in the ring the host drains over
//! [`IOCTL_DRAIN_LOG`](pf_driver_proto::control::IOCTL_DRAIN_LOG) — the encoder runs inside
//! WUDFHost, so that ring is how its backend rejections, retargets and wedges reach `host.log`.
//! The syscall sinks — the debugger string and the file tee — are [`pf_umdf_util::log`]'s, gated
//! on [`file_log_enabled`]. Best-effort; ignores all errors.

use std::collections::VecDeque;
use std::sync::Mutex;

use pf_driver_proto::control;

/// Lines the host has not drained yet. Bounded, because a per-frame failure loop must cost a
/// fixed ceiling rather than the WUDFHost: the oldest goes and `dropped` counts it, so a flood
/// reads as a flood instead of as silence. 256 lines covers several sessions' worth of open and
/// retarget chatter at the pinger's ~3.3 s cadence.
const RING_LINES: usize = 256;

/// Longest line kept. Past this a record could outgrow the host's drain buffer and never leave
/// the ring; only an `{e:#}` chain comes close.
const MAX_LINE: usize = 512;

/// The lines the host has not drained, with their severity, and how many were dropped since a
/// drain last reported it.
struct Ring {
    lines: VecDeque<(u8, String)>,
    dropped: u64,
}

/// The process's one ring: every [`log_at`] pushes here and every `IOCTL_DRAIN_LOG` drains it.
static RING: Mutex<Ring> = Mutex::new(Ring::new());

impl Ring {
    const fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Queue one line, cut to [`MAX_LINE`] bytes at a char boundary, dropping the oldest when
    /// full.
    fn push(&mut self, level: u8, line: &str) {
        let end = (0..=MAX_LINE.min(line.len()))
            .rev()
            .find(|&i| line.is_char_boundary(i))
            .unwrap_or(0);
        if self.lines.len() >= RING_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back((level, line[..end].to_string()));
    }

    /// Whole records, oldest first, up to `cap` bytes, led by an overflow line when lines were
    /// dropped. What does not fit stays queued for the next call — except a record that alone
    /// exceeds `cap`, which is dropped and counted rather than left to wedge every line behind it.
    fn drain(&mut self, cap: usize) -> Vec<u8> {
        let mut out = Vec::new();
        if self.dropped > 0 {
            let n = std::mem::take(&mut self.dropped);
            let line = format!("[pf-vd] log ring overflowed — {n} lines dropped");
            control::write_log_record(&mut out, control::LOG_WARN, &line);
        }
        while let Some((level, line)) = self.lines.pop_front() {
            let start = out.len();
            control::write_log_record(&mut out, level, &line);
            if out.len() > cap {
                out.truncate(start);
                if start == 0 {
                    self.dropped += 1;
                } else {
                    self.lines.push_front((level, line));
                }
                break;
            }
        }
        out
    }
}

/// Answer `IOCTL_DRAIN_LOG` from the process's ring ([`Ring::drain`]).
pub fn drain(cap: usize) -> Vec<u8> {
    RING.lock().map(|mut r| r.drain(cap)).unwrap_or_default()
}

/// The bring-up file log. Off in release builds unless the `PFVD_DEBUG_LOG` knob is set; the
/// gate is [`knob`], not plain `std::env`, so a `setx /M` takes effect on a device restart. The
/// path and the sink live in [`pf_umdf_util::log`], one copy for every driver.
static FILE_LOG: pf_umdf_util::log::FileLog =
    pf_umdf_util::log::FileLog::new("pfvd-driver.log", || {
        cfg!(debug_assertions) || knob("PFVD_DEBUG_LOG").is_some()
    });

/// Whether the syscall sinks (debug string + bring-up file) are on. The host's drain ring does
/// not ride this gate; only those two do, and so does whether `DEBUG` events are kept at all.
pub(crate) fn file_log_enabled() -> bool {
    FILE_LOG.enabled()
}

/// A driver knob: the process environment first, then the MACHINE environment in the registry
/// (where `setx /M` writes). WUDFHost inherits its environment from the SCM at boot and the SCM
/// never refreshes it, so a `setx /M` after boot is invisible to `std::env` until a reboot; the
/// registry read makes a device restart enough. An empty value is no value: `setx /M NAME ""`
/// leaves one behind, and a knob read as present turns on.
pub(crate) fn knob(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| machine_env(name))
        .filter(|v| !v.trim().is_empty())
}

/// Read a MACHINE environment variable from the registry (see [`knob`]).
fn machine_env(name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};
    const KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let (subkey, value) = (HSTRING::from(KEY), HSTRING::from(name));
    let mut buf = [0u16; 256];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: both name pointers address NUL-terminated HSTRING buffers alive for the call;
    // `buf`/`size` are a matched out-buffer and its byte length. RRF_RT_REG_SZ makes the call
    // reject any non-string value rather than write a foreign type into the buffer.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    // `size` is bytes INCLUDING the terminator; trim to chars and drop trailing NULs.
    let chars = (size as usize / 2).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..chars])
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// One line at `INFO`; see [`log_at`].
pub fn log(s: &str) {
    log_at(control::LOG_INFO, s);
}

/// Queue one line for the host and, when [`file_log_enabled`], tee it to the debugger and the
/// bring-up file.
pub(crate) fn log_at(level: u8, s: &str) {
    if let Ok(mut r) = RING.lock() {
        r.push(level, s);
    }
    FILE_LOG.write(s);
}

// Always formats: the line is the host's only view of this process. One `String` per event costs
// nothing beside the encode it describes, and the ring bounds what a failure loop can hold.
macro_rules! dbglog {
    ($($a:tt)*) => { $crate::log::log(&::std::format!($($a)*)) };
}

/// Route the encoder backends' `tracing` events into [`log_at`]: with no subscriber in WUDFHost
/// every NVENC status string and AMF rejection is dropped, and a failed open reaches the host as
/// a bare stage tag. Call from `DriverEntry`, once.
pub(crate) fn install_tracing_bridge() {
    let _ = tracing::subscriber::set_global_default(Bridge);
}

struct Bridge;

impl tracing::Subscriber for Bridge {
    /// `DEBUG` only under the local sinks — a backend may emit one per submitted frame, and the
    /// host's ring is for the session story, not a per-frame trace.
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        *m.level()
            <= if file_log_enabled() {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            }
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    /// The severity travels as the record's byte, so the host re-emits at the level the backend
    /// chose instead of flattening every encoder warning into an info line.
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Line(&mut line));
        let m = event.metadata();
        let level = match *m.level() {
            tracing::Level::ERROR => control::LOG_ERROR,
            tracing::Level::WARN => control::LOG_WARN,
            tracing::Level::INFO => control::LOG_INFO,
            _ => control::LOG_DEBUG,
        };
        log_at(level, &format!("[pf-vd] {}:{line}", m.target()));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// `message` first as written, every other field as `name=value`.
struct Line<'a>(&'a mut String);

impl tracing::field::Visit for Line<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        use core::fmt::Write;
        let _ = if field.name() == "message" {
            write!(self.0, " {value:?}")
        } else {
            write!(self.0, " {}={value:?}", field.name())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_driver_proto::control::{LOG_INFO, LOG_WARN};

    fn records(buf: &[u8]) -> Vec<(u8, &str)> {
        control::log_lines(buf).collect()
    }

    /// A ring that dropped its two oldest lines: `0` and `1` went, `2` ..= `RING_LINES + 1` stay.
    fn overflowed() -> Ring {
        let mut r = Ring::new();
        for i in 0..RING_LINES + 2 {
            r.push(LOG_INFO, &i.to_string());
        }
        r
    }

    #[test]
    fn a_record_over_the_cap_is_dropped_and_counted() {
        let mut r = Ring::new();
        r.push(LOG_INFO, &"x".repeat(100));
        r.push(LOG_INFO, "after");
        assert!(r.drain(50).is_empty());
        let overflow = "[pf-vd] log ring overflowed — 1 lines dropped";
        let out = r.drain(100);
        assert_eq!(records(&out), [(LOG_WARN, overflow), (LOG_INFO, "after")]);
    }

    #[test]
    fn a_record_that_does_not_fit_waits_for_the_next_drain() {
        let mut r = Ring::new();
        r.push(LOG_INFO, "first");
        r.push(LOG_WARN, "second");
        assert_eq!(records(&r.drain(10)), [(LOG_INFO, "first")]);
        assert_eq!(records(&r.drain(10)), [(LOG_WARN, "second")]);
        assert!(r.drain(10).is_empty());
    }

    #[test]
    fn the_overflow_line_leads_the_next_drain_once() {
        let mut r = overflowed();
        let out = r.drain(usize::MAX);
        let overflow = "[pf-vd] log ring overflowed — 2 lines dropped";
        assert_eq!(records(&out)[..2], [(LOG_WARN, overflow), (LOG_INFO, "2")]);
        r.push(LOG_INFO, "next");
        assert_eq!(records(&r.drain(usize::MAX)), [(LOG_INFO, "next")]);
    }

    #[test]
    fn the_ring_keeps_the_newest_lines() {
        let out = overflowed().drain(usize::MAX);
        let kept: Vec<String> = records(&out)[1..].iter().map(|r| r.1.into()).collect();
        let want: Vec<String> = (2..RING_LINES + 2).map(|i| i.to_string()).collect();
        assert_eq!(kept, want);
    }

    #[test]
    fn a_long_line_is_cut_at_a_char_boundary() {
        let mut r = Ring::new();
        let kept = "a".repeat(MAX_LINE - 1);
        r.push(LOG_INFO, &format!("{kept}é tail"));
        r.push(LOG_INFO, &"b".repeat(MAX_LINE + 1));
        let out = r.drain(usize::MAX);
        let whole = "b".repeat(MAX_LINE);
        assert_eq!(records(&out), [(LOG_INFO, &*kept), (LOG_INFO, &*whole)]);
    }
}
