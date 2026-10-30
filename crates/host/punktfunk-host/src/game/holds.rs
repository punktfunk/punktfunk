//! Launch holds: plugins and automations that must act before a game starts.
//!
//! [`launching`] emits `game.launching`, then waits for every participant at once: each live
//! plugin registered with the stage (`POST /__hold` on its loopback UI port) and each hook with
//! `hold` set. Each runs to its own deadline. The stage is fail-open: a late, failed or refused
//! participant is logged and the launch goes ahead.
//!
//! A seat host runs no plugins. Its event carries a `hold` token, and the launch also waits, up
//! to [`RELAYED_HOLD`], for the host with the plugins to run them ([`plugins`]) and call
//! `POST /game/holds/{token}/release` ([`release`]).

use crate::events::{EventKind, GameRefPayload, HostEvent};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// `crate::hooks` checks a hook's stage against the same list.
pub use crate::plugin_host::registry::STAGES;

/// How long a seat's launch waits for its release: the longest plugin hold, plus 10 s for the
/// relay there and back.
pub const RELAYED_HOLD: Duration =
    Duration::from_millis(crate::plugin_host::registry::HOLD_MAX_MS as u64 + 10_000);

/// Emit `game.launching` and block until every participant answered or ran out of time.
/// Call on a blocking thread, only when this session spawns the game.
pub fn launching(game: GameRefPayload) {
    let relayed = pf_paths::seat::box_library_dir()
        .is_some()
        .then(Relayed::new);
    let hold = relayed.as_ref().map(|r| r.token.clone());
    let ev = crate::events::bus().emit(EventKind::GameLaunching { game, hold });
    let plugins = crate::plugin_host::registry::holders("game.launching");
    let hooks = crate::hooks::holding(&ev);
    if plugins.is_empty() && hooks.is_empty() && relayed.is_none() {
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
        if let Some(r) = &relayed {
            s.spawn(|| r.wait(RELAYED_HOLD));
        }
    });
    tracing::info!(
        plugins = plugins.len(),
        hooks = hooks.len(),
        relayed = relayed.is_some(),
        ms = started.elapsed().as_millis() as u64,
        "launch hold released"
    );
}

/// The plugins' half of a seat's held launch, on the host that has them: every plugin holding
/// the stage gets `ev` at once. Returns when all answered or ran out of time. Blocking.
pub(crate) fn plugins(ev: &HostEvent) {
    let plugins = crate::plugin_host::registry::holders("game.launching");
    if plugins.is_empty() {
        return;
    }
    let started = Instant::now();
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    std::thread::scope(|s| {
        for p in &plugins {
            s.spawn(|| hold_plugin(p, &json));
        }
    });
    tracing::info!(
        plugins = plugins.len(),
        seat = ?ev.seat,
        ms = started.elapsed().as_millis() as u64,
        "seat's launch hold released"
    );
}

/// Seat launches waiting for their release, by token: `true` once released.
static WAITING: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());
static RELEASED: Condvar = Condvar::new();

fn waiting() -> MutexGuard<'static, BTreeMap<String, bool>> {
    WAITING.lock().unwrap_or_else(|e| e.into_inner())
}

/// A seat launch on [`WAITING`] for as long as this lives.
struct Relayed {
    token: String,
}

impl Relayed {
    /// Registered before the event goes out, so no release can come first.
    fn new() -> Relayed {
        let token = crate::mgmt_token::random_token();
        waiting().insert(token.clone(), false);
        Relayed { token }
    }

    /// Until released, or `deadline` passed.
    fn wait(&self, deadline: Duration) {
        let (guard, waited) = RELEASED
            .wait_timeout_while(waiting(), deadline, |w| w.get(&self.token) == Some(&false))
            .unwrap_or_else(|e| e.into_inner());
        drop(guard);
        if waited.timed_out() {
            tracing::warn!(
                timeout_ms = deadline.as_millis() as u64,
                "the plugins' host did not release the launch — launching anyway"
            );
        }
    }
}

impl Drop for Relayed {
    fn drop(&mut self) {
        waiting().remove(&self.token);
    }
}

/// Lets the seat launch waiting on `token` go ahead. `false` when none waits on it.
pub(crate) fn release(token: &str) -> bool {
    let mut waiting = waiting();
    let Some(released) = waiting.get_mut(token) else {
        return false;
    };
    *released = true;
    RELEASED.notify_all();
    true
}

/// One plugin's hold: `POST /__hold` with the event, 2xx means done. A plugin on the host's
/// channel (port 0) is reached through it instead of a port.
fn hold_plugin(p: &crate::plugin_host::registry::Holder, json: &str) {
    let timeout_ms = p.timeout.as_millis() as u64;
    if p.port == 0 {
        use crate::plugin_host::channel::{request_blocking, Unreached};
        match request_blocking(&p.id, "/__hold", json, p.timeout) {
            Ok((200..=299, _)) => tracing::debug!(plugin = %p.id, "plugin released the launch"),
            Ok((status, _)) => tracing::warn!(
                plugin = %p.id, status,
                "plugin launch hold refused — launching anyway"
            ),
            Err(Unreached::Io(e)) if e == "timed out" => tracing::warn!(
                plugin = %p.id, timeout_ms,
                "plugin launch hold ran past its deadline — launching anyway"
            ),
            Err(e) => tracing::warn!(
                plugin = %p.id, error = %e,
                "plugin launch hold unanswered — launching anyway"
            ),
        }
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::HostEvent;
    use std::io::{Read, Write};
    use std::time::Duration;

    /// The event a plugin receives, for tests that fake one.
    fn launching_event(title: &str) -> HostEvent {
        HostEvent {
            seq: 1,
            ts_ms: 0,
            schema: crate::events::SCHEMA_VERSION,
            seat: None,
            kind: EventKind::GameLaunching {
                game: GameRefPayload {
                    app: Some("steam:570".into()),
                    title: title.into(),
                    store: Some("steam".into()),
                    client: "deck".into(),
                    fingerprint: Some("ab12".into()),
                    plane: crate::events::Plane::Native,
                    preset: None,
                    profile: None,
                },
                hold: None,
            },
        }
    }

    /// A seat's launch goes ahead on its release, or at its deadline when none comes; a token
    /// nobody waits on releases nothing.
    #[test]
    fn a_relayed_launch_waits_for_its_release_or_its_deadline() {
        let held = Relayed::new();
        let t = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                assert!(release(&held.token));
            });
            held.wait(Duration::from_secs(10));
        });
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

        let late = Relayed::new();
        let t = Instant::now();
        late.wait(Duration::from_millis(200));
        assert!(t.elapsed() >= Duration::from_millis(200));

        assert!(!release("not-a-hold"));
        let token = held.token.clone();
        drop(held);
        assert!(
            !release(&token),
            "a launch that went ahead is no longer held"
        );
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
        let p = crate::plugin_host::registry::Holder {
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
        let p = crate::plugin_host::registry::Holder {
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
