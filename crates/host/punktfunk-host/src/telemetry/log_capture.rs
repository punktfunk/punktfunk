//! In-memory tail of the host's own log stream for the web console.
//!
//! A `tracing` layer tees every event at DEBUG and above — independent of
//! `RUST_LOG` — into a bounded ring. `GET /api/v1/logs` (see `mgmt.rs`) serves
//! it so an operator can read recent host logs without shell access.
//!
//! Newest [`CAPACITY`] entries; readers poll with an `after` sequence cursor.
//! Unlike the stats recorder, this is a tail, not a head.
//!
//! `log`-crate events (via the tracing-log bridge) are normalized to their
//! real module path. [`NOISY_DEBUG_TARGETS`] drop DEBUG/TRACE so LAN chatter
//! cannot evict the diagnostics the ring exists to keep.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use utoipa::ToSchema;

/// Ring capacity — bounds memory at a few MB worst case ([`MAX_MSG`]-sized entries).
const CAPACITY: usize = 4096;
/// Per-entry message cap; log lines are short, anything longer is a payload dump we truncate.
const MAX_MSG: usize = 2048;
/// Hard cap on entries returned per poll (the client immediately re-polls to drain a backlog).
pub const MAX_PAGE: usize = 1000;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct LogEntry {
    /// Monotonic sequence number (1-based) — pass the last one back as the `after` cursor.
    pub seq: u64,
    pub ts_ms: u64,
    /// `ERROR` | `WARN` | `INFO` | `DEBUG` | `TRACE`.
    pub level: String,
    pub target: String,
    /// Formatted message; structured fields appended as `key=value`.
    pub msg: String,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    /// Last returned seq, or the request's `after` when the page is empty.
    pub next: u64,
    /// Entries between `after` and the first returned one were already evicted.
    pub dropped: bool,
}

pub struct LogRing {
    inner: Mutex<Inner>,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

impl LogRing {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            }),
        }
    }

    /// `pub(crate)` for the mgmt handler tests; production entries only come from [`RingLayer`].
    pub(crate) fn push(&self, level: &tracing::Level, target: &str, msg: String) {
        let ts_ms = crate::clock::unix_ms();
        self.push_entry(level.to_string(), target.to_string(), msg, ts_ms);
    }

    /// Ingest a line produced outside this process (`POST /plugins/logs`).
    /// Plugin runners are separate processes, so [`RingLayer`] never sees them.
    /// Keep the caller's `ts_ms` (a flushed batch must not collapse onto arrival).
    /// `seq` stays the ring's: one cursor, several producers.
    pub fn push_remote(&self, level: &str, target: &str, msg: &str, ts_ms: u64) {
        self.push_entry(
            normalize_level(level).to_string(),
            target.to_string(),
            truncate_msg(msg.to_string()),
            ts_ms,
        );
    }

    fn push_entry(&self, level: String, target: String, msg: String, ts_ms: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() == CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back(LogEntry {
            seq,
            ts_ms,
            level,
            target,
            msg,
        });
    }

    pub fn since(&self, after: u64, limit: usize) -> LogPage {
        let limit = limit.clamp(1, MAX_PAGE);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Entries are seq-ordered and contiguous: index of the first wanted one is derivable.
        let first_seq = inner.entries.front().map_or(inner.next_seq, |e| e.seq);
        let dropped = after != 0 && after + 1 < first_seq;
        let skip = after
            .saturating_sub(first_seq)
            .saturating_add(u64::from(after >= first_seq)) as usize;
        let entries: Vec<LogEntry> = inner
            .entries
            .iter()
            .skip(skip)
            .take(limit)
            .cloned()
            .collect();
        let next = entries.last().map_or(after, |e| e.seq);
        LogPage {
            entries,
            next,
            dropped,
        }
    }
}

/// Process-wide ring. `OnceLock` so the tracing layer (installed in `main`
/// before host state exists) and the mgmt handler share it without an `Arc`.
pub fn ring() -> &'static LogRing {
    static RING: OnceLock<LogRing> = OnceLock::new();
    RING.get_or_init(LogRing::new)
}

/// Coerce an external level to the five the console filter ranks.
/// Unknown / empty becomes `INFO`: an unranked string sorts as `0` and hides.
fn normalize_level(level: &str) -> &'static str {
    match level.trim().to_ascii_uppercase().as_str() {
        "ERROR" | "FATAL" | "SEVERE" => "ERROR",
        "WARN" | "WARNING" => "WARN",
        "DEBUG" => "DEBUG",
        "TRACE" | "VERBOSE" => "TRACE",
        _ => "INFO",
    }
}

fn truncate_msg(mut msg: String) -> String {
    if msg.len() > MAX_MSG {
        let mut end = MAX_MSG;
        while !msg.is_char_boundary(end) {
            end -= 1;
        }
        msg.truncate(end);
        msg.push('…');
    }
    msg
}

/// DEBUG/TRACE from these modules is steady chatter and would evict the tail.
/// `rustls` narrates four lines per accepted connection, and the console polls
/// this ring over TLS, so it outruns everything the ring exists to keep.
/// The ring keeps their INFO-and-up; the file/stderr EnvFilter caps them
/// separately. Prefix-matched on module-path boundaries.
const NOISY_DEBUG_TARGETS: &[&str] = &["mdns_sd", "rustls", "wasapi"];

fn is_noisy_debug(target: &str) -> bool {
    NOISY_DEBUG_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Init the `log`→`tracing` bridge and install `subscriber` as the global
/// default. Replaces `SubscriberInitExt::init()` so `wasapi` can be dropped
/// at the bridge: those records carry the shim target at filter time, so a
/// file-layer target filter cannot catch them. Bridge max-level stays DEBUG
/// so every other `log` crate still reaches the ring.
pub fn install_global<S>(subscriber: S)
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    let _ = tracing_log::LogTracer::builder()
        .with_max_level(log::LevelFilter::Debug)
        .ignore_crate("wasapi")
        .init();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Tee every event into [`ring`]. Install with per-layer `LevelFilter::DEBUG`
/// so the ring sees DEBUG even when `RUST_LOG` keeps stderr at `info`.
/// Messages carry their span context (`session{id=7}: …`) so concurrent
/// sessions stay separable in the console.
pub struct RingLayer;

/// A span's fields, formatted at open. The registry keeps span metadata but not
/// recorded values, so a layer that wants them stores its own copy.
struct SpanFields(String);

impl<S> tracing_subscriber::Layer<S> for RingLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = FieldFmt::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut()
                .insert(SpanFields(fields.fields.trim_start().to_string()));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        // `log`-crate events arrive under the bridge shim target `"log"`;
        // normalize to the real module path so the noise gate sees `mdns_sd::…`.
        use tracing_log::NormalizeEvent;
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if *meta.level() > tracing::Level::INFO && is_noisy_debug(meta.target()) {
            return;
        }
        let mut fields = FieldFmt::default();
        event.record(&mut fields);
        // Outermost span first, matching what the `fmt` layer prints to stderr.
        let mut prefix = String::new();
        if let Some(scope) = ctx.event_scope(event) {
            use std::fmt::Write;
            for span in scope.from_root() {
                let ext = span.extensions();
                let f = ext.get::<SpanFields>().map_or("", |s| s.0.as_str());
                let _ = write!(prefix, "{}{{{f}}}: ", span.name());
            }
        }
        let msg = fields.finish();
        let msg = if prefix.is_empty() {
            msg
        } else {
            truncate_msg(prefix + &msg)
        };
        ring().push(meta.level(), meta.target(), msg);
    }
}

/// Default-fmt shape: `message` first, every other field as ` key=value`.
#[derive(Default)]
struct FieldFmt {
    msg: String,
    fields: String,
}

impl tracing::field::Visit for FieldFmt {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.msg, "{value:?}");
        } else if !field.name().starts_with("log.") {
            // `log.*` is tracing-log bookkeeping; already on the normalized target.
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.msg.push_str(value);
        } else if !field.name().starts_with("log.") {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

impl FieldFmt {
    fn finish(mut self) -> String {
        if self.msg.is_empty() {
            self.msg = self.fields.trim_start().to_string();
        } else {
            self.msg.push_str(&self.fields);
        }
        truncate_msg(self.msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_n(ring: &LogRing, n: usize) {
        for i in 0..n {
            ring.push(&tracing::Level::INFO, "test", format!("m{i}"));
        }
    }

    #[test]
    fn cursor_pagination_and_eviction() {
        let ring = LogRing::new();
        push_n(&ring, 10);

        let page = ring.since(0, 100);
        assert_eq!(page.entries.len(), 10);
        assert_eq!(page.next, 10);
        assert!(!page.dropped);

        let page = ring.since(10, 100);
        assert!(page.entries.is_empty());
        assert_eq!(page.next, 10);

        let page = ring.since(4, 3);
        assert_eq!(
            page.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
        assert_eq!(page.next, 7);
        assert!(!page.dropped);
    }

    #[test]
    fn eviction_reports_dropped() {
        let ring = LogRing::new();
        push_n(&ring, CAPACITY + 50);
        // Cursor inside the evicted gap must set `dropped`.
        let page = ring.since(10, 5);
        assert!(page.dropped);
        assert_eq!(page.entries.first().map(|e| e.seq), Some(51));
        // A cursor at the ring head is not a gap.
        let head = ring.since(page.next, 5);
        assert!(!head.dropped);
        assert_eq!(head.entries.first().map(|e| e.seq), Some(page.next + 1));
    }

    /// The singleton ring is process-wide — tests find its current tail first (parallel tests
    /// may interleave, so they only assert on THEIR events appearing after it).
    fn tail_seq() -> u64 {
        let mut cur = 0;
        loop {
            let page = ring().since(cur, MAX_PAGE);
            if page.entries.is_empty() {
                return cur;
            }
            cur = page.next;
        }
    }

    #[test]
    fn layer_captures_events_into_the_singleton_ring() {
        use tracing_subscriber::layer::SubscriberExt;

        let cur = tail_seq();

        let subscriber = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(answer = 42, "ring layer test message");
        });

        let page = ring().since(cur, MAX_PAGE);
        let hit = page
            .entries
            .iter()
            .find(|e| e.msg.contains("ring layer test message"))
            .expect("event captured");
        assert_eq!(hit.level, "WARN");
        assert!(
            hit.msg.contains("answer=42"),
            "fields appended: {}",
            hit.msg
        );
        assert!(hit.target.contains("log_capture"), "target: {}", hit.target);
        assert!(hit.ts_ms > 0);
    }

    #[test]
    fn events_inside_a_span_carry_its_fields() {
        use tracing_subscriber::layer::SubscriberExt;

        let cur = tail_seq();

        let subscriber = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(subscriber, || {
            let _entered = tracing::info_span!("session", id = 7u64).entered();
            tracing::warn!(dropped = 3, "capture loss");
        });

        let page = ring().since(cur, MAX_PAGE);
        let hit = page
            .entries
            .iter()
            .find(|e| e.msg.contains("capture loss"))
            .expect("event captured");
        assert_eq!(hit.msg, "session{id=7}: capture loss dropped=3");
    }

    #[test]
    fn log_bridge_events_normalize_target_and_noisy_debug_is_dropped() {
        use tracing_subscriber::layer::SubscriberExt;

        // Global `LogTracer`; tolerate a prior install. Explicit max_level so
        // `debug!` records reach the bridge.
        let _ = tracing_log::LogTracer::init();
        log::set_max_level(log::LevelFilter::Trace);

        let cur = tail_seq();

        let subscriber = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(subscriber, || {
            log::debug!(target: "mdns_sd::service_daemon", "Invalid incoming DNS message: flood");
            log::warn!(target: "mdns_sd::service_daemon", "a real mdns problem");
            log::debug!(target: "mdns_sdx", "not actually mdns-sd");
            log::debug!(target: "rustls::server::hs", "decided upon suite");
        });

        let page = ring().since(cur, MAX_PAGE);
        assert!(
            !page.entries.iter().any(|e| e.msg.contains("flood")),
            "noisy-target DEBUG must not reach the ring"
        );
        let warn = page
            .entries
            .iter()
            .find(|e| e.msg.contains("a real mdns problem"))
            .expect("noisy-target WARN kept");
        assert_eq!(warn.target, "mdns_sd::service_daemon");
        assert!(!warn.msg.contains("log.target"), "msg: {}", warn.msg);
        assert!(page.entries.iter().any(|e| e.target == "mdns_sdx"));
        assert!(
            !page
                .entries
                .iter()
                .any(|e| e.msg.contains("decided upon suite")),
            "rustls handshake DEBUG must not reach the ring"
        );
    }

    #[test]
    fn remote_entries_keep_their_own_timestamp_and_share_the_cursor() {
        let ring = LogRing::new();
        ring.push(&tracing::Level::INFO, "punktfunk_host", "local".into());
        ring.push_remote("WARN", "plugin:virtualhere", "remote", 1_700_000_000_123);

        let page = ring.since(0, 10);
        assert_eq!(page.entries.len(), 2);
        // One sequence across both producers — the console's cursor cannot see two rings.
        assert_eq!(page.entries[0].seq, 1);
        assert_eq!(page.entries[1].seq, 2);
        let remote = &page.entries[1];
        assert_eq!(remote.level, "WARN");
        assert_eq!(remote.target, "plugin:virtualhere");
        assert_eq!(remote.msg, "remote");
        assert_eq!(remote.ts_ms, 1_700_000_000_123);
    }

    #[test]
    fn remote_levels_are_coerced_not_rejected() {
        assert_eq!(normalize_level("error"), "ERROR");
        assert_eq!(normalize_level(" Warning "), "WARN");
        assert_eq!(normalize_level("TRACE"), "TRACE");
        // An unranked level would sort as 0 in the console's filter and hide under every setting.
        assert_eq!(normalize_level("NOTICE"), "INFO");
        assert_eq!(normalize_level(""), "INFO");
    }

    #[test]
    fn remote_messages_are_truncated_like_local_ones() {
        let ring = LogRing::new();
        ring.push_remote("INFO", "plugin:x", &"ä".repeat(MAX_MSG), 1);
        let page = ring.since(0, 10);
        let msg = &page.entries[0].msg;
        assert!(msg.ends_with('…'));
        assert!(msg.len() <= MAX_MSG + '…'.len_utf8());
    }

    #[test]
    fn message_truncation_keeps_char_boundary() {
        let f = FieldFmt {
            msg: "ä".repeat(MAX_MSG), // 2 bytes each — exceeds the cap at a multi-byte boundary
            ..Default::default()
        };
        let out = f.finish();
        assert!(out.ends_with('…'));
        assert!(out.len() <= MAX_MSG + '…'.len_utf8());
    }
}
