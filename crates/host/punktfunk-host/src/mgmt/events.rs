//! `GET /api/v1/events` — host lifecycle events as Server-Sent Events.
//!
//! One frame per event: `id:` is `seq`, `event:` is the kind, `data:` is
//! [`crate::events::HostEvent`] JSON. Resume with `Last-Event-ID` or `?since=`.
//! A cursor that fell off the ring gets `event: dropped` first and must resync
//! from REST snapshots. `event: live` closes the catch-up: everything after it
//! happened after the consumer connected. `?kinds=` filters server-side (exact
//! names or `domain.*`, comma-separated).
//!
//! At most [`MAX_EVENT_STREAMS`] concurrent streams (503 beyond). A consumer too
//! slow for the live tail is disconnected, not buffered; reconnect reads the ring.

use super::shared::*;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// 32: a handful of consumers (console + scripts) plus headroom; bounds a reconnect storm.
const MAX_EVENT_STREAMS: usize = 32;

/// 15 s: dead-peer probe and idle-middlebox keep-alive between sparse lifecycle events.
pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(15);

static LIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Connection-cap token. `pub(crate)` so [`test_support`] can return the slots it holds.
pub(crate) struct StreamSlot;

/// The 503 every SSE route answers with once [`MAX_EVENT_STREAMS`] is reached.
pub(crate) fn stream_cap_reached() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "event-stream connection cap reached — close an existing stream or retry",
    )
}

/// One cap across every SSE route on the box: a pad stream costs a connection like a
/// lifecycle stream does.
pub(crate) fn try_acquire_slot() -> Option<StreamSlot> {
    LIVE_STREAMS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_EVENT_STREAMS).then_some(n + 1)
        })
        .ok()
        .map(|_| StreamSlot)
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        LIVE_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct KindFilter(Option<Vec<String>>);

impl KindFilter {
    fn parse(kinds: Option<&str>) -> Self {
        let pats: Option<Vec<String>> = kinds.map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        });
        KindFilter(pats.filter(|p| !p.is_empty()))
    }

    fn matches(&self, kind: &str) -> bool {
        match &self.0 {
            None => true,
            Some(pats) => pats.iter().any(|p| crate::events::kind_matches(p, kind)),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    since: Option<u64>,
    kinds: Option<String>,
}

/// `data:` repeats `kind` so a client that ignores the SSE `event:` field still has it.
fn sse_event(ev: &crate::events::HostEvent) -> Event {
    Event::default()
        .id(ev.seq.to_string())
        .event(ev.kind.name())
        .data(serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string()))
}

/// Dropping the SSE body drops `_slot` and the broadcast receiver (cap + subscription).
struct StreamState {
    pending: VecDeque<Event>,
    rx: broadcast::Receiver<crate::events::HostEvent>,
    filter: KindFilter,
    _slot: StreamSlot,
}

/// Stream host lifecycle events (SSE)
///
/// `id:` is `seq`, `event:` is kind, `data:` is HostEvent JSON. Resume with `Last-Event-ID`
/// or `?since=`; `event: dropped` means the ring no longer has that cursor — resync from REST.
/// `event: live` follows the catch-up; frames after it happened after you connected.
#[utoipa::path(
    get,
    path = "/events",
    tag = "events",
    operation_id = "streamEvents",
    params(
        ("since" = Option<u64>, Query, description = "Resume cursor: only events with `seq` greater than this are sent (the ring keeps the newest ~1024). `Last-Event-ID` takes precedence."),
        ("kinds" = Option<String>, Query, description = "Comma-separated server-side kind filter: exact kinds (`pairing.pending`) or `domain.*` prefixes (`stream.*`)."),
        ("Last-Event-ID" = Option<u64>, Header, description = "SSE auto-reconnect cursor — the `id:` of the last received frame."),
    ),
    responses(
        (status = OK, description = "SSE stream; each frame's `data:` is one HostEvent", body = crate::events::HostEvent, content_type = "text/event-stream"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Concurrent event-stream cap reached — retry shortly", body = ApiError),
    )
)]
pub(crate) async fn stream_events(Query(q): Query<EventsQuery>, headers: HeaderMap) -> Response {
    let Some(slot) = try_acquire_slot() else {
        return stream_cap_reached();
    };
    // SSE auto-reconnect sends `Last-Event-ID`; it wins over the URL's `?since=` when both exist.
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(q.since)
        .unwrap_or(0);
    let filter = KindFilter::parse(q.kinds.as_deref());
    let sub = crate::events::bus().subscribe(since);

    let mut pending = VecDeque::new();
    if sub.dropped {
        // Cursor is older than the ring; `dropped` means resync from REST, not a complete replay.
        pending.push_back(
            Event::default()
                .event("dropped")
                .data(r#"{"dropped":true}"#),
        );
    }
    pending.extend(
        sub.catch_up
            .iter()
            .filter(|ev| filter.matches(ev.kind.name()))
            .map(sse_event),
    );
    // Where history ends. A fresh page load replays the whole ring, and a consumer that
    // announces knocks must not re-announce one from an hour ago.
    pending.push_back(Event::default().event("live").data(r#"{"live":true}"#));

    let state = StreamState {
        pending,
        rx: sub.rx,
        filter,
        _slot: slot,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((Ok::<_, std::convert::Infallible>(ev), st));
            }
            match st.rx.recv().await {
                Ok(ev) => {
                    if st.filter.matches(ev.kind.name()) {
                        return Some((Ok(sse_event(&ev)), st));
                    }
                }
                // Lagged: drop this consumer (no unbounded buffer); reconnect reads the ring.
                // Closed: host shutdown.
                Err(broadcast::error::RecvError::Lagged(_))
                | Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Hold every cap slot until dropped. Tests that call this must serialize; otherwise
    /// unrelated streams 503.
    pub(crate) fn saturate_slots() -> Vec<super::StreamSlot> {
        std::iter::from_fn(super::try_acquire_slot).collect()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn kind_filter_semantics() {
        let all = KindFilter::parse(None);
        assert!(all.matches("stream.started"));

        let f = KindFilter::parse(Some("stream.*, pairing.pending"));
        assert!(f.matches("stream.started"));
        assert!(f.matches("stream.stopped"));
        assert!(f.matches("pairing.pending"));
        assert!(!f.matches("pairing.completed"));
        assert!(!f.matches("client.connected"));
        // A prefix pattern must match on the dot boundary, not raw text.
        assert!(!f.matches("streamx.started"));

        // Empty/blank filter strings mean "no filter", not "nothing matches".
        assert!(KindFilter::parse(Some("")).matches("host.started"));
        assert!(KindFilter::parse(Some(" , ")).matches("host.started"));
    }
}
