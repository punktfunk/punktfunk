//! Client-side recent-log ring and the "send logs to host" uploader.
//!
//! Locked-down shells cannot export a log file, so the client keeps the newest
//! few thousand lines here. An explicit user action posts them to the paired host
//! (`POST /api/v1/client-logs`, same mTLS as the stream). The web console lists
//! them next to the host's own logs.
//!
//! Std-only ring so Android can render it; [`RingLayer`] (desktop) feeds it from
//! `tracing`. Bounded by [`MAX_LINES`] and [`MAX_BYTES`] so a log-storm cannot
//! grow memory; the byte budget stays under the host's 1 MiB upload cap.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

/// Same depth as the host's own ring.
pub const MAX_LINES: usize = 4096;

/// Under the host's 1 MiB bundle cap, with headroom for the header.
pub const MAX_BYTES: usize = 768 * 1024;

struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| {
    Mutex::new(Ring {
        lines: VecDeque::new(),
        bytes: 0,
        dropped: 0,
    })
});

/// No trailing newline. Truncates at 2048 bytes so one event cannot evict the whole ring.
pub fn note(mut line: String) {
    if line.len() > 2048 {
        // On a char boundary: `truncate` panics inside a multi-byte character.
        let mut end = 2048;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    let mut r = RING.lock().unwrap_or_else(|e| e.into_inner());
    r.bytes += line.len();
    r.lines.push_back(line);
    while r.lines.len() > MAX_LINES || r.bytes > MAX_BYTES {
        if let Some(evicted) = r.lines.pop_front() {
            r.bytes -= evicted.len();
            r.dropped += 1;
        } else {
            break;
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` from the system clock. Wall time so a bundle
/// correlates with the host log beside it. Shared by every feeder (`ring_layer`,
/// Android logcat tee).
pub fn wallclock() -> String {
    let ms = punktfunk_core::quic::wall_clock_ns() / 1_000_000;
    punktfunk_core::time::utc_rfc3339(ms, true)
}

/// Oldest-first text bundle. `header` is the shell identity line; an eviction
/// note is prefixed when the ring wrapped.
pub fn render(header: &str) -> String {
    let r = RING.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::with_capacity(r.bytes + header.len() + 64);
    out.push_str(header);
    out.push('\n');
    if r.dropped > 0 {
        out.push_str(&format!(
            "… {} older lines evicted from the ring …\n",
            r.dropped
        ));
    }
    for line in &r.lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// POST the ring to the paired host; returns the stored bundle id. Same TLS
/// client auth and pin as the library fetch. Errors reuse that classification
/// (`NotPaired`, `PinMismatch`) so existing shell strings apply.
#[cfg(desktop)]
pub fn send_to_host(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    header: &str,
) -> Result<String, crate::library::LibraryError> {
    use crate::library::LibraryError;
    let body = render(header);
    let agent = crate::library::agent(identity, pin)?;
    let url = format!(
        "{}/api/v1/client-logs",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent
        .post(&url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(mut resp) => {
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?;
            let id = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                .unwrap_or_default();
            Ok(id)
        }
        Err(e) => Err(crate::library::classify(e)),
    }
}

/// [`send_to_host`] with the bundle header and outcome every shell uses. `app` names the
/// binary in the header; the result is the sentence the shell shows the user.
#[cfg(desktop)]
pub fn send_bundle(
    app: &str,
    host_name: &str,
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    fp_hex: &str,
) -> String {
    let header = format!(
        "{app} {} ({} {}) — client log bundle",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let pin = crate::trust::parse_hex32(fp_hex);
    match send_to_host(addr, mgmt_port, identity, pin, &header) {
        Ok(id) => {
            tracing::info!(host = %host_name, id, "client logs uploaded");
            format!("Logs sent to {host_name} — download them from its web console's Logs page")
        }
        Err(e) => {
            tracing::warn!(host = %host_name, error = %e, "client log upload failed");
            format!("Couldn't send logs — {e}")
        }
    }
}

/// Install the process subscriber: a fmt layer on `writer` scoped by `RUST_LOG` (default
/// `info`), beside [`RingLayer`] at DEBUG regardless, since the ring exists for the
/// diagnostics nobody enabled before the bug happened. `ansi: false` turns colour off for a
/// log file; `true` keeps the fmt layer's default.
#[cfg(desktop)]
pub fn init_tracing<W>(writer: W, ansi: bool)
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;
    let fmt = tracing_subscriber::fmt::layer().with_writer(writer);
    let fmt = if ansi { fmt } else { fmt.with_ansi(false) };
    tracing_subscriber::registry()
        .with(fmt.with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        ))
        .with(RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG))
        .init();
}

/// `tracing` layer that feeds the ring. Installed beside the visible layer with
/// its own `LevelFilter::DEBUG`, not under the env filter: a field bundle must
/// carry diagnostics nobody enabled beforehand. Mirrors the host's
/// `log_capture::RingLayer`.
///
/// DEBUG/TRACE from [`NOISY_DEBUG_TARGETS`] is dropped. The vendored H.265
/// parser logs DPB bookkeeping every frame; at 120 fps that turns the ring over
/// in seconds and flushes the session the bundle exists to keep.
#[cfg(desktop)]
pub struct RingLayer;

/// DEBUG/TRACE from these module-path prefixes is chatter, not diagnostics.
/// Prefix-matched on `::` boundaries so `cros_codecs::…` is gated and
/// `cros_codecs_probe` is not. Same shape as the host's `NOISY_DEBUG_TARGETS`.
#[cfg(desktop)]
const NOISY_DEBUG_TARGETS: &[&str] = &["cros_codecs"];

#[cfg(desktop)]
fn is_noisy_debug(target: &str) -> bool {
    NOISY_DEBUG_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

#[cfg(desktop)]
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write as _;
        use tracing::field::{Field, Visit};
        // `log` crate events arrive under tracing-log's shim target "log", with the
        // real module in `log.target`. Normalize so the noise gate and the target
        // column both see `cros_codecs::…`.
        use tracing_log::NormalizeEvent;
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if *meta.level() > tracing::Level::INFO && is_noisy_debug(meta.target()) {
            return;
        }
        struct V(String);
        impl Visit for V {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    // Belt-and-braces against odd macro field order.
                    let rest = std::mem::take(&mut self.0);
                    let _ = write!(self.0, "{value:?}");
                    self.0.push_str(&rest);
                } else if !field.name().starts_with("log.") {
                    // Bridge bookkeeping (`log.target` / `log.module_path` / …) already lives in
                    // the normalized target; keeping it would add ~150 bytes of path per line.
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
        note(format!(
            "{} {:5} {} {}",
            wallclock(),
            meta.level().as_str(),
            meta.target(),
            v.0
        ));
    }
}

/// Line-buffered tee of a spawned session child's stderr into `out` and the ring.
/// `out` is our stderr, or the WinUI shell's log-file tee. Returns immediately; the
/// thread dies with the pipe.
#[cfg(desktop)]
pub fn forward_child_stderr(
    stderr: impl std::io::Read + Send + 'static,
    mut out: impl std::io::Write + Send + 'static,
) {
    let _ = std::thread::Builder::new()
        .name("pf-session-log".into())
        .spawn(move || {
            use std::io::BufRead as _;
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = Vec::new();
            // Bytes, not `read_line`: that fails the whole read on one non-UTF-8 byte, which
            // ends the drain — and a stderr pipe nobody empties blocks the child mid-stream.
            while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                let _ = out.write_all(&buf);
                note(String::from_utf8_lossy(&buf).trim_end().to_string());
                buf.clear();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-global ring; every writer test takes this so a parallel run cannot
    /// interleave lines into an order-sensitive assertion.
    static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ring_bounds_and_renders_with_eviction_note() {
        let _own = RING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..(MAX_LINES + 10) {
            note(format!("line {i}"));
        }
        let text = render("punktfunk-client test");
        assert!(text.starts_with("punktfunk-client test\n"));
        assert!(text.contains("older lines evicted"));
        assert!(
            !text.contains("\nline 0\n"),
            "oldest line survived eviction"
        );
        assert!(text.ends_with(&format!("line {}\n", MAX_LINES + 9)));
        note("x".repeat(10_000));
        let text = render("h");
        assert!(text.contains('…'));
    }

    /// A non-UTF-8 byte must not end the drain: the child blocks once its pipe fills.
    #[cfg(desktop)]
    #[test]
    fn stderr_forward_drains_past_a_non_utf8_line() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let _own = RING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let marker = format!("drain-{}", std::process::id());
        let input = [b"one\n\xff\n".as_slice(), marker.as_bytes(), b"\n"].concat();
        let sink = Sink::default();
        forward_child_stderr(std::io::Cursor::new(input.clone()), sink.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let drained = || {
            *sink.0.lock().unwrap() == input && render("test").contains(&format!("\n{marker}\n"))
        };
        while !drained() {
            assert!(std::time::Instant::now() < deadline, "drain stopped early");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(desktop)]
    #[test]
    fn noisy_gate_matches_the_crate_and_its_modules_only() {
        assert!(is_noisy_debug("cros_codecs"));
        assert!(is_noisy_debug("cros_codecs::codec::h265::dpb"));
        assert!(!is_noisy_debug("cros_codecs_probe"));
        assert!(!is_noisy_debug("pf_bitstream::h265"));
        assert!(!is_noisy_debug("pf_client_core::audio"));
    }

    /// Markers, not ring size: the ring is process-global.
    #[cfg(desktop)]
    #[test]
    fn bridged_decoder_debug_is_dropped_and_the_audio_line_survives() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        let _own = RING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sub = tracing_subscriber::registry()
            .with(RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG));
        // The bridge may already be installed by another test; either way DEBUG must
        // be admitted or the planted records never dispatch.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(log::LevelFilter::Debug)
            .init();
        log::set_max_level(log::LevelFilter::Debug);
        let _guard = tracing::subscriber::set_default(sub);

        let marker = format!("ringgate-{}", std::process::id());
        log::debug!(target: "cros_codecs::codec::h265::dpb", "Retaining pic POC {marker}-dpb: true");
        log::warn!(target: "cros_codecs::codec::h265::parser", "{marker}-parser-warn");
        tracing::debug!(target: "pf_client_core::audio", buffer_ms = 15u32, "audio playback {marker}-audio");

        let text = render("test");
        assert!(
            !text.contains(&format!("{marker}-dpb")),
            "decoder DPB DEBUG chatter must not reach the ring"
        );
        let warn_line = text
            .lines()
            .find(|l| l.contains(&format!("{marker}-parser-warn")))
            .expect("decoder WARN must be kept");
        assert!(
            warn_line.contains("cros_codecs::codec::h265::parser"),
            "bridged events must carry their real target, not the `log` shim: {warn_line}"
        );
        assert!(
            !warn_line.contains("log.target="),
            "bridge bookkeeping fields must be dropped: {warn_line}"
        );
        let audio_line = text
            .lines()
            .find(|l| l.contains(&format!("{marker}-audio")))
            .expect("our own DEBUG audio line must survive");
        assert!(audio_line.contains("pf_client_core::audio"));
        assert!(audio_line.contains("buffer_ms=15"));
    }
}
