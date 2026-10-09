//! The open handshake every audio backend thread shares: spawn, wait for the thread's first
//! word, and on a timeout stop it. A missing device or daemon is the open's `Err` (the
//! caller's backoff), never a silent dead thread.

use anyhow::{anyhow, Context, Result};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Spawn thread `name` running `body` and wait up to `timeout` for the value it sends on its
/// ready channel; an `Err` sent there is the open's error. On a timeout, or a thread that
/// ends without a word, `on_timeout` gets the handle: it stops the thread, then reaps
/// ([`reap_timed_out`]) or detaches it, and returns the open's error.
pub(crate) fn spawn_ready<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    body: impl FnOnce(SyncSender<Result<T>>) + Send + 'static,
    on_timeout: impl FnOnce(JoinHandle<()>) -> anyhow::Error,
) -> Result<(T, JoinHandle<()>)> {
    let (ready_tx, ready_rx) = sync_channel::<Result<T>>(1);
    let join = thread::Builder::new()
        .name(name.into())
        .spawn(move || body(ready_tx))
        .with_context(|| format!("spawn {name}"))?;
    match ready_rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Ok((value, join)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(on_timeout(join)),
    }
}

/// The timeout error for `what`'s thread once told to stop. It gets 2 s to exit and is
/// joined, so the next open never runs beside it; one that stays is named, not hidden.
pub(crate) fn reap_timed_out(what: &str, join: JoinHandle<()>) -> anyhow::Error {
    match reap_with_timeout(join, Duration::from_secs(2)) {
        true => anyhow!("{what} init timed out"),
        false => anyhow!("{what} init timed out, and its thread is stuck in the audio stack"),
    }
}

/// Join `join` if it exits within `budget`. `false`: still running, left detached.
fn reap_with_timeout(join: JoinHandle<()>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while !join.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = join.join();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn never(_: JoinHandle<()>) -> anyhow::Error {
        unreachable!("the thread answered")
    }

    #[test]
    fn the_threads_answer_is_the_open() {
        let (value, join) = spawn_ready(
            "ready-ok",
            Duration::from_secs(5),
            |tx| {
                let _ = tx.send(Ok(7));
            },
            never,
        )
        .unwrap();
        assert_eq!(value, 7);
        join.join().unwrap();
        let err = spawn_ready::<()>(
            "ready-err",
            Duration::from_secs(5),
            |tx| {
                let _ = tx.send(Err(anyhow!("no device")));
            },
            never,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no device");
    }

    #[test]
    fn a_timeout_stops_and_reaps_the_thread() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let err = spawn_ready::<()>(
            "ready-slow",
            Duration::from_millis(50),
            move |_tx| {
                while !stop_t.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            },
            |join| {
                stop.store(true, Ordering::SeqCst);
                reap_timed_out("test", join)
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "test init timed out");
    }

    #[test]
    fn a_thread_that_ignores_stop_is_reported() {
        let release = Arc::new(AtomicBool::new(false));
        let release_t = release.clone();
        let join = thread::spawn(move || {
            while !release_t.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
        });
        assert!(!reap_with_timeout(join, Duration::from_millis(50)));
        release.store(true, Ordering::SeqCst);
    }
}
