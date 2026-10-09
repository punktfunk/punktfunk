//! Per-session access: the expiry clock, the T−5 m / T−1 m warnings, and the pairing watch that
//! re-points the live grant mask or closes the session.

use super::{close_rejected, link};
use punktfunk_core::quic::{AccessUpdate, GRANT_CLIPBOARD};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Remaining lifetime on the wire: saturating whole seconds, floor 1. `0` means *permanent*,
/// so a deadline due this second still advertises as expiring.
pub(super) fn remaining_secs_wire(deadline: Option<i64>, now: i64) -> u32 {
    deadline
        .map(|d| u32::try_from((d - now).max(1)).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// Seconds before the deadline for best-effort toasts (T−5 m, T−1 m). Older clients miss them.
const ACCESS_WARN_SECS: [i64; 2] = [300, 60];

/// Thresholds already behind the deadline at `now` are spent, not fired — at admission
/// (Welcome just advertised remaining) and after an edit (`AccessUpdate` just did). A
/// threshold only fires by being crossed live.
fn spent_warnings(deadline: Option<i64>, now: i64) -> [bool; 2] {
    match deadline {
        None => [true, true],
        Some(d) => [
            d - now <= ACCESS_WARN_SECS[0],
            d - now <= ACCESS_WARN_SECS[1],
        ],
    }
}

/// Sleep until the next unfired boundary, re-derived from `deadline − now` each lap and
/// capped at 30 s so an NTP step moves the deadline within one cap interval.
fn access_sleep(deadline: Option<i64>, warned: &[bool; 2], now: i64) -> std::time::Duration {
    let Some(d) = deadline else {
        // Permanent: park; the watch/close arms wake.
        return std::time::Duration::from_secs(3600);
    };
    let mut next = d;
    for (i, w) in ACCESS_WARN_SECS.iter().enumerate() {
        if !warned[i] {
            next = next.min(d - w);
        }
    }
    std::time::Duration::from_secs((next - now).clamp(1, 30) as u64)
}

/// Per-session access: expiry deadline + watch. Best-effort `AccessUpdate` at T−5 m / T−1 m
/// and on every grant edit; folds the live mask within one event; typed-close at deadline,
/// "expire now", or unpair. Closes only this connection — the owner's stream is untouched.
///
/// A pairing edit wins over a console per-session re-point: this task rewrites both the live
/// mask and the ceiling the route clamps to.
pub(super) async fn access_lifecycle(
    conn: link::SessionLink,
    mut watch_rx: tokio::sync::watch::Receiver<crate::native_pairing::AccessState>,
    controls: crate::session_status::SessionControls,
    clip_enabled: Arc<AtomicBool>,
    access_tx: tokio::sync::mpsc::UnboundedSender<AccessUpdate>,
    mut deadline: Option<i64>,
    device: crate::events::DeviceRef,
) {
    let mut warned = spent_warnings(deadline, crate::clock::unix_secs());
    // `power.*` ending every session: typed close so the client does not see a transport error.
    let mut power_rx = crate::power::closing_rx();
    loop {
        let now = crate::clock::unix_secs();
        if let Some(d) = deadline {
            if now >= d {
                // Wall clock at fire: `d − now` is recomputed each lap, so an NTP step moves it.
                tracing::info!(
                    device = %device.name,
                    fingerprint = %device.fingerprint,
                    "temporary access expired — closing this device's session"
                );
                crate::events::emit(crate::events::EventKind::AccessExpired { device });
                close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired).await;
                return;
            }
            let remaining = d - now;
            for (i, w) in ACCESS_WARN_SECS.iter().enumerate() {
                if !warned[i] && remaining <= *w {
                    warned[i] = true;
                    let _ = access_tx.send(AccessUpdate {
                        grants: controls.grants.load(Ordering::Relaxed),
                        remaining_secs: u32::try_from(remaining).unwrap_or(u32::MAX),
                    });
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(access_sleep(deadline, &warned, crate::clock::unix_secs())) => {}
            changed = watch_rx.changed() => {
                if changed.is_err() {
                    return; // registry gone — host shutting down
                }
                let st = *watch_rx.borrow_and_update();
                if st.revoked {
                    // Unpair is terminal: end the session, do not merely mute it.
                    tracing::info!(
                        device = %device.name,
                        fingerprint = %device.fingerprint,
                        "device unpaired — closing its live session"
                    );
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::AccessExpired).await;
                    return;
                }
                // Live mask updates now; the datagram filter reads it on the next event.
                // Wider-mask resources stay up and starve (tearing a live uinput pad is churn).
                // Clipboard is the cheap exception: clear the flag, stop forwarding copies.
                controls.grants.store(st.grants, Ordering::Relaxed);
                controls.ceiling.store(st.grants, Ordering::Relaxed);
                if st.grants & GRANT_CLIPBOARD == 0 {
                    clip_enabled.store(false, Ordering::SeqCst);
                }
                deadline = st.deadline_unix;
                controls
                    .deadline_unix
                    .store(deadline.unwrap_or(0), Ordering::Relaxed);
                let now = crate::clock::unix_secs();
                warned = spent_warnings(deadline, now);
                // Skip an "expire now" (deadline already past) so we do not advertise a phantom second.
                if deadline.is_none_or(|d| d > now) {
                    let _ = access_tx.send(AccessUpdate {
                        grants: st.grants,
                        remaining_secs: remaining_secs_wire(deadline, now),
                    });
                }
            }
            changed = power_rx.changed() => {
                if changed.is_ok() && *power_rx.borrow_and_update() {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::HostPower).await;
                    return;
                }
            }
            _ = conn.closed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Access clock and threshold arithmetic. The session tests run the timed task itself.
    #[test]
    fn access_deadline_math() {
        let now = 1_700_000_000i64;
        // Wire: 0 = permanent; a due/past deadline still reads as expiring (floor 1).
        assert_eq!(remaining_secs_wire(None, now), 0);
        assert_eq!(remaining_secs_wire(Some(now + 90), now), 90);
        assert_eq!(remaining_secs_wire(Some(now), now), 1);
        assert_eq!(remaining_secs_wire(Some(now - 50), now), 1);

        // Thresholds already behind the deadline are spent, not fired.
        assert_eq!(spent_warnings(None, now), [true, true]);
        assert_eq!(spent_warnings(Some(now + 400), now), [false, false]);
        assert_eq!(spent_warnings(Some(now + 120), now), [true, false]);
        assert_eq!(spent_warnings(Some(now + 30), now), [true, true]);

        // Sleep toward the next unfired boundary, 1..=30 s; permanent parks long.
        assert_eq!(
            access_sleep(None, &[true, true], now),
            std::time::Duration::from_secs(3600)
        );
        // 400 s out, T−5 m unfired → 100 s away, capped at the 30 s NTP-staleness bound.
        assert_eq!(
            access_sleep(Some(now + 400), &[false, false], now),
            std::time::Duration::from_secs(30)
        );
        // 90 s out, only T−1 m left → 30 s away.
        assert_eq!(
            access_sleep(Some(now + 90), &[true, false], now),
            std::time::Duration::from_secs(30)
        );
        // 10 s out, all warned → the deadline itself.
        assert_eq!(
            access_sleep(Some(now + 10), &[true, true], now),
            std::time::Duration::from_secs(10)
        );
        // Due now → 1 s floor (never a busy-spin zero sleep).
        assert_eq!(
            access_sleep(Some(now), &[true, true], now),
            std::time::Duration::from_secs(1)
        );
    }
}
