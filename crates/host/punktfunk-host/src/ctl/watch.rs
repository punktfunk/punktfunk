//! Line-JSON stdout for `punktfunk-host ctl watch`.
//!
//! One flushed object per line over the host SSE stream (`mgmt/events.rs`). The first
//! connect starts at the live tail unless `--since` names a cursor; every frame with an
//! `id:` advances the cursor so a host restart mid-watch continues from the last
//! delivered event. The host's `live` frame passes through as `{"kind":"live"}`.
//!
//! Synthetic kinds:
//! - `ctl.resync` — after a `dropped` frame, or a reconnect that could not resume. The
//!   consumer must re-snapshot (`ctl status` / `ctl pending`); further events will not.
//! - `ctl.disconnected` — the stream dropped; reconnect is automatic.
//! - `ctl.heartbeat` — host keep-alive, ~15 s. Writing it is how [`emit`] notices the
//!   consumer is gone (EPIPE on a read-only pipe never fires).
//!
//! Backoff 1–30 s so a tight loop cannot exhaust `MAX_EVENT_STREAMS` (32, shared with
//! the console). First connect and a pin mismatch still exit; retry cannot fix those.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use super::client::{Client, Failure, Result, SCHEMA_VERSION};

/// 1 s hides a host restart from a widget; 30 s is the cap against a down host.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn run(kinds: Option<&str>, since: Option<u64>) -> Result<()> {
    // No `--since` = the live tail. A cursor past any real seq gets an empty catch-up and no
    // `dropped`, so a widget restart does not replay 1024 events, nor a knock from an hour
    // ago. The first frame's real id replaces it for reconnects. `--since 0` replays the ring.
    let mut cursor = Some(since.unwrap_or(u64::MAX));
    let mut backoff = BACKOFF_MIN;
    // First connect may fail the process: a bad pin, missing token, or never-run host
    // cannot be retried; spinning forever hides that from the caller.
    let mut client = Client::connect(None)?;
    loop {
        match pump(&client, kinds, &mut cursor) {
            // The stream ended cleanly (host shutdown) — reconnect like any other drop.
            Ok(()) => emit_control("ctl.disconnected", Some("stream closed by the host")),
            Err(e) if e.code == super::client::EXIT_PIN => return Err(e),
            Err(e) => emit_control("ctl.disconnected", Some(&e.message)),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
        // Rebuild so we re-read `native-cert.pem`. Reusing the client pins forever if the
        // host regenerated its identity while we were down.
        match Client::connect(None) {
            Ok(c) => {
                client = c;
                backoff = BACKOFF_MIN;
            }
            Err(e) if e.code == super::client::EXIT_PIN => return Err(e),
            Err(e) => emit_control("ctl.disconnected", Some(&e.message)),
        }
    }
}

/// `Ok(())` means the server closed the stream, not that watch is done.
fn pump(client: &Client, kinds: Option<&str>, cursor: &mut Option<u64>) -> Result<()> {
    let mut path = String::from("/api/v1/events");
    let mut sep = '?';
    if let Some(k) = kinds {
        path.push_str(&format!("{sep}kinds={}", urlencode(k)));
        sep = '&';
    }
    if let Some(c) = *cursor {
        path.push_str(&format!("{sep}since={c}"));
    }
    let reader = BufReader::new(client.stream(&path)?);

    let mut id: Option<u64> = None;
    let mut kind: Option<String> = None;
    let mut data = String::new();
    for line in reader.lines() {
        let line =
            line.map_err(|e| Failure::unreachable(format!("event stream read failed: {e}")))?;
        if line.is_empty() {
            if let Some(k) = kind.take() {
                dispatch(&k, &data, id, cursor);
            }
            id = None;
            data.clear();
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            // Empty field: SSE comment (`: keep-alive`, ~15 s). Writing the heartbeat is
            // the liveness check — a read-only stdout never sees the widget die, so idle
            // watchers would hold a `MAX_EVENT_STREAMS` slot until reboot. [`emit`] exits on EPIPE.
            "" => emit(serde_json::json!({ "v": SCHEMA_VERSION, "kind": "ctl.heartbeat" })),
            "id" => id = value.parse().ok(),
            "event" => kind = Some(value.to_string()),
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            _ => {}
        }
    }
    Ok(())
}

fn dispatch(kind: &str, data: &str, id: Option<u64>, cursor: &mut Option<u64>) {
    if let Some(seq) = id {
        *cursor = Some(seq);
    }
    if kind == "dropped" {
        // Fell off the catch-up ring. Further events cannot repair stale widget state.
        emit_control("ctl.resync", None);
        return;
    }
    let payload = serde_json::from_str::<serde_json::Value>(data)
        .unwrap_or_else(|_| serde_json::Value::String(data.to_string()));
    emit(serde_json::json!({
        "v": SCHEMA_VERSION,
        "kind": kind,
        "seq": id,
        "data": payload,
    }));
}

fn emit_control(kind: &str, error: Option<&str>) {
    let mut line = serde_json::json!({ "v": SCHEMA_VERSION, "kind": kind });
    if let Some(e) = error {
        line["data"] = serde_json::json!({ "error": e });
    }
    emit(line);
}

/// One flushed line: an 8 KiB stdio buffer would hide the next event from a widget.
/// Write/flush failure means the consumer is gone. Exit 0 (normal end) so we
/// drop the SSE slot instead of holding `MAX_EVENT_STREAMS` until reboot.
fn emit(line: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").is_err() || out.flush().is_err() {
        std::process::exit(0);
    }
}

/// Host `kinds` grammar is `[a-z0-9_.*,-]`; encode only what a typo could introduce.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' | b',' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_survive_encoding_and_typos_are_escaped() {
        assert_eq!(
            urlencode("stream.*,pairing.pending"),
            "stream.*,pairing.pending"
        );
        assert_eq!(urlencode("a b"), "a%20b");
    }

    #[test]
    fn a_dropped_frame_advances_the_cursor_and_asks_for_a_resync() {
        // Advance even for `dropped`: resuming from before it would replay the ring miss.
        let mut cursor = None;
        dispatch("dropped", r#"{"dropped":true}"#, Some(7), &mut cursor);
        assert_eq!(cursor, Some(7));
    }

    #[test]
    fn an_ordinary_frame_advances_the_cursor() {
        let mut cursor = Some(3);
        dispatch(
            "pairing.pending",
            r#"{"kind":"pairing.pending"}"#,
            Some(9),
            &mut cursor,
        );
        assert_eq!(cursor, Some(9));
        // A frame with no id (the synthetic ones) must not rewind it.
        dispatch("stream.started", "{}", None, &mut cursor);
        assert_eq!(cursor, Some(9));
    }
}
