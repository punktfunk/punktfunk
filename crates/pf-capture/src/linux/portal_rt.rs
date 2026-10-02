//! Bounded ScreenCast handshake steps and the cursor-mode negotiation, shared by
//! capture and the virtual displays. They run on [`pf_portal::portal_runtime`],
//! the one runtime ashpd's process-global connection lives on.
//!
//! Every ScreenCast handshake shares [`HANDSHAKE_BUDGET`] with its bounded
//! steps ([`within`], [`finish_or_close`], [`close_session`]) and the
//! cursor-mode negotiation ([`negotiate_cursor_mode`]).

use ashpd::desktop::screencast::{CursorMode, Screencast};
use ashpd::enumflags2::BitFlags;
use pf_frame::cursor_mode::{parse_pin, pick, Mode, Pin};
use std::future::Future;
use std::time::Duration;

/// Ceiling on one ScreenCast handshake, connect through `open_pipe_wire_remote`.
/// Under the callers' 20 s setup wait, so the thread that owns a stuck portal
/// reports it and exits. A hung request poisons every later one from this process.
pub const HANDSHAKE_BUDGET: Duration = Duration::from_secs(15);

/// Ceiling on `Session.Close`. The D-Bus connection outlives every session, so
/// Close is the only teardown; bounded so a wedged portal cannot hang one.
pub const CAST_CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// Close a portal session within [`CAST_CLOSE_BUDGET`]. A failure is logged, not
/// fatal: the caller is already tearing the cast down.
pub async fn close_session(close: impl Future<Output = ashpd::Result<()>>) {
    match tokio::time::timeout(CAST_CLOSE_BUDGET, close).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            error = %e,
            "closing the portal session failed — the next cast may find the portal busy"
        ),
        Err(_) => tracing::warn!(
            budget_s = CAST_CLOSE_BUDGET.as_secs(),
            "the portal did not answer Session.Close in time — it is probably already wedged"
        ),
    }
}

/// One handshake step under the shared deadline ([`HANDSHAKE_BUDGET`]). An
/// await the portal never answers would park its thread on the process-global
/// connection for good.
pub async fn within<T, E>(
    deadline: tokio::time::Instant,
    step: impl Future<Output = Result<T, E>>,
) -> anyhow::Result<T>
where
    anyhow::Error: From<E>,
{
    match tokio::time::timeout_at(deadline, step).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(no_answer()),
    }
}

fn no_answer() -> anyhow::Error {
    anyhow::anyhow!(
        "the portal did not answer within {}s",
        HANDSHAKE_BUDGET.as_secs()
    )
}

/// The steps after `create_session`, under the deadline. On a timeout or a failed step the
/// half-built session is closed: nothing else ends it, and a started cast keeps casting.
pub async fn finish_or_close<T, C>(
    deadline: tokio::time::Instant,
    steps: impl Future<Output = anyhow::Result<T>>,
    close: impl FnOnce() -> C,
) -> anyhow::Result<T>
where
    C: Future<Output = ashpd::Result<()>>,
{
    match tokio::time::timeout_at(deadline, steps).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            close_session(close()).await;
            Err(e)
        }
        Err(_) => {
            close_session(close()).await;
            Err(no_answer())
        }
    }
}

/// `AvailableCursorModes`, re-read while it is empty.
///
/// A portal that the ScreenCast call itself D-Bus-activated publishes `0`
/// until its backend answers. xdg-desktop-portal validates `SelectSources`
/// against this same property, so the settled value is the one that counts.
/// Returns whatever it reads last, empty included, after 2 s.
pub async fn available_cursor_modes(proxy: &Screencast) -> ashpd::Result<BitFlags<CursorMode>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let avail = proxy.available_cursor_modes().await?;
        if !avail.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(avail);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `Metadata` when the session has a cursor channel (the client or encoder
/// draws, so the compositor must not burn the pointer in), else `Embedded`.
/// `PUNKTFUNK_PORTAL_CURSOR_MODE` overrides both. `backend` is the log line only.
fn want(hw_cursor: bool, backend: &str) -> Mode {
    let negotiated = if hw_cursor {
        Mode::Metadata
    } else {
        Mode::Embedded
    };
    let raw = match pf_host_config::config().portal_cursor_mode.as_deref() {
        Some(raw) => raw,
        None => return negotiated,
    };
    match parse_pin(raw) {
        Pin::Auto => negotiated,
        Pin::Mode(pinned) => {
            tracing::info!(
                backend,
                pinned = pinned.name(),
                negotiated = negotiated.name(),
                "ScreenCast: cursor mode pinned by PUNKTFUNK_PORTAL_CURSOR_MODE"
            );
            pinned
        }
        Pin::Unrecognised => {
            tracing::warn!(
                backend,
                value = raw,
                negotiated = negotiated.name(),
                "ScreenCast: unrecognised PUNKTFUNK_PORTAL_CURSOR_MODE (want auto|hidden|embedded|\
                 metadata) — ignoring"
            );
            negotiated
        }
    }
}

/// The `SelectSources` cursor mode for every portal cast, through the one
/// ladder in [`pf_frame::cursor_mode`]. Never an unadvertised bit: the portal
/// closes a session that asks for one. An empty or failed read requests
/// `Embedded`; the mode is fixed for the session.
pub async fn negotiate_cursor_mode(proxy: &Screencast, hw_cursor: bool, backend: &str) -> Mode {
    let want = want(hw_cursor, backend);
    let advertised = match available_cursor_modes(proxy).await {
        Ok(avail) if !avail.is_empty() => avail.bits(),
        Ok(_) => {
            tracing::warn!(
                backend,
                "ScreenCast: portal advertised no cursor modes — requesting Embedded cursor"
            );
            return Mode::Embedded;
        }
        Err(e) => {
            // ScreenCast v2 property. A portal that cannot publish it is too old for Metadata.
            tracing::warn!(
                backend,
                error = %e,
                "ScreenCast: AvailableCursorModes query failed — requesting Embedded cursor"
            );
            return Mode::Embedded;
        }
    };
    let choice = pick(advertised, want);
    match choice.wanted {
        None => tracing::info!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            mode = choice.mode.name(),
            "ScreenCast: cursor mode negotiated"
        ),
        Some(wanted) => tracing::warn!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            wanted = wanted.name(),
            mode = choice.mode.name(),
            "ScreenCast: requested cursor mode is not advertised by this portal — downgrading \
             (requesting it anyway would close the session)"
        ),
    }
    choice.mode
}

/// ashpd's flag for a negotiated [`Mode`].
pub fn to_ashpd(mode: Mode) -> CursorMode {
    match mode {
        Mode::Hidden => CursorMode::Hidden,
        Mode::Embedded => CursorMode::Embedded,
        Mode::Metadata => CursorMode::Metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ashpd `CursorMode` bits follow enumflags2 declaration order; a reorder
    /// silently repoints every mode.
    #[test]
    fn mode_bits_match_ashpd() {
        for m in [Mode::Hidden, Mode::Embedded, Mode::Metadata] {
            assert_eq!(
                BitFlags::from_flag(to_ashpd(m)).bits(),
                m.bit(),
                "{} drifted from ashpd",
                m.name()
            );
        }
        assert_eq!(BitFlags::from_flag(CursorMode::Metadata).bits(), 4);
    }
}

#[cfg(test)]
mod handshake_bound_tests {
    use super::finish_or_close;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn run<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build test runtime")
            .block_on(f)
    }

    /// A portal that never answers: the setup fails and the half-built session is closed.
    #[test]
    fn a_hung_step_closes_the_half_built_session() {
        let closed = AtomicBool::new(false);
        let result = run(finish_or_close(
            tokio::time::Instant::now(),
            std::future::pending::<anyhow::Result<()>>(),
            || async {
                closed.store(true, Ordering::Relaxed);
                Ok::<(), ashpd::Error>(())
            },
        ));
        assert!(result.is_err());
        assert!(closed.load(Ordering::Relaxed));
    }

    /// A step past `start` that fails (no streams, no PipeWire remote) closes the live cast too.
    #[test]
    fn a_failed_step_closes_the_half_built_session() {
        let closed = AtomicBool::new(false);
        let result = run(finish_or_close(
            tokio::time::Instant::now() + Duration::from_secs(5),
            async { Err::<(), _>(anyhow::anyhow!("portal returned no streams")) },
            || async {
                closed.store(true, Ordering::Relaxed);
                Ok::<(), ashpd::Error>(())
            },
        ));
        assert!(result.is_err());
        assert!(closed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_finished_handshake_keeps_its_session() {
        let closed = AtomicBool::new(false);
        let result = run(finish_or_close(
            tokio::time::Instant::now() + Duration::from_secs(5),
            async { Ok(7) },
            || async {
                closed.store(true, Ordering::Relaxed);
                Ok::<(), ashpd::Error>(())
            },
        ));
        assert_eq!(result.unwrap(), 7);
        assert!(!closed.load(Ordering::Relaxed));
    }
}
