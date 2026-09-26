//! Launch holds: plugins and automations that must act before a game starts.
//!
//! [`launching`] emits `game.launching`, then waits for every participant at once: each live
//! plugin registered with the stage (`POST /__hold` on its loopback UI port) and each hook with
//! `hold` set. Each runs to its own deadline. The stage is fail-open: a late, failed or refused
//! participant is logged and the launch goes ahead.

use crate::events::{EventKind, GameRefPayload};
use std::time::{Duration, Instant};

/// The stages a plugin or hook may hold.
pub const STAGES: &[&str] = &["game.launching"];

/// The longest a plugin may hold. Clients cover a launch for 120 s.
pub const HOLD_MAX_MS: u32 = 120_000;
/// A plugin's deadline when it names none.
pub const HOLD_DEFAULT_MS: u32 = 30_000;

/// Emit `game.launching` and block until every participant answered or ran out of time.
/// Call on a blocking thread, only when this session spawns the game.
pub fn launching(game: GameRefPayload) {
    let ev = crate::events::bus().emit(EventKind::GameLaunching { game });
    let plugins = crate::mgmt::plugins::holders("game.launching");
    let hooks = crate::hooks::holding(&ev);
    if plugins.is_empty() && hooks.is_empty() {
        return;
    }
    let started = Instant::now();
    let json = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
    std::thread::scope(|s| {
        for p in &plugins {
            s.spawn(|| hold_plugin(p, &json));
        }
        for h in &hooks {
            s.spawn(|| crate::hooks::hold(h, &ev));
        }
    });
    tracing::info!(
        plugins = plugins.len(),
        hooks = hooks.len(),
        ms = started.elapsed().as_millis() as u64,
        "launch hold released"
    );
}

/// One plugin's hold: `POST /__hold` with the event, 2xx means done.
fn hold_plugin(p: &crate::mgmt::plugins::Holder, json: &str) {
    // No proxy: the secret must never leave loopback. No redirects, as for webhooks.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(p.timeout))
        .build()
        .into();
    let url = format!("http://127.0.0.1:{}/__hold", p.port);
    let res = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", p.secret))
        .header("Content-Type", "application/json")
        .send(json);
    let timeout_ms = p.timeout.as_millis() as u64;
    match res {
        Ok(_) => tracing::debug!(plugin = %p.id, "plugin released the launch"),
        Err(ureq::Error::Timeout(_)) => tracing::warn!(
            plugin = %p.id, timeout_ms,
            "plugin launch hold ran past its deadline — launching anyway"
        ),
        Err(ureq::Error::StatusCode(status)) => tracing::warn!(
            plugin = %p.id, status,
            "plugin launch hold refused — launching anyway"
        ),
        Err(e) => tracing::warn!(
            plugin = %p.id, error = %e,
            "plugin launch hold unanswered — launching anyway"
        ),
    }
}

/// A hold's deadline from a registration: default when absent, `None` when out of range.
pub fn deadline(ms: Option<u32>) -> Option<Duration> {
    match ms.unwrap_or(HOLD_DEFAULT_MS) {
        0 => None,
        ms if ms > HOLD_MAX_MS => None,
        ms => Some(Duration::from_millis(u64::from(ms))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::HostEvent;
    use std::io::{Read, Write};

    /// The event a plugin receives, for tests that fake one.
    fn launching_event(title: &str) -> HostEvent {
        HostEvent {
            seq: 1,
            ts_ms: 0,
            schema: crate::events::SCHEMA_VERSION,
            kind: EventKind::GameLaunching {
                game: GameRefPayload {
                    app: Some("steam:570".into()),
                    title: title.into(),
                    store: Some("steam".into()),
                    client: "deck".into(),
                    fingerprint: Some("ab12".into()),
                    plane: crate::events::Plane::Native,
                    preset: None,
                },
            },
        }
    }

    #[test]
    fn a_deadline_defaults_and_refuses_the_out_of_range() {
        assert_eq!(deadline(None), Some(Duration::from_secs(30)));
        assert_eq!(deadline(Some(0)), None);
        assert_eq!(deadline(Some(HOLD_MAX_MS + 1)), None);
        assert_eq!(deadline(Some(500)), Some(Duration::from_millis(500)));
    }

    #[test]
    fn a_plugin_that_never_answers_holds_only_to_its_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and read, never answer.
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        let p = crate::mgmt::plugins::Holder {
            id: "slow".into(),
            port,
            secret: "s".repeat(16),
            timeout: Duration::from_millis(300),
        };
        let json = serde_json::to_string(&launching_event("Dota 2")).unwrap();
        let t = Instant::now();
        hold_plugin(&p, &json);
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(250), "{took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn a_plugin_gets_the_event_with_its_secret() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers and a small body arrive together; read until the body's closing brace.
            while !req.ends_with(b"}") {
                let n = c.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            c.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            String::from_utf8(req).unwrap()
        });
        let p = crate::mgmt::plugins::Holder {
            id: "fast".into(),
            port,
            secret: "a".repeat(16),
            timeout: Duration::from_secs(5),
        };
        let json = serde_json::to_string(&launching_event("Dota 2")).unwrap();
        hold_plugin(&p, &json);
        let req = seen.join().unwrap();
        assert!(req.starts_with("POST /__hold "), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains(&format!("authorization: bearer {}", "a".repeat(16))),
            "{req}"
        );
        assert!(req.contains(r#""kind":"game.launching""#), "{req}");
    }
}
