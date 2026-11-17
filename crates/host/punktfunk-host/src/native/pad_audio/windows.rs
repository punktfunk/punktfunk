//! Windows: capture the pad's WASAPI loopback endpoint ([`crate::audio::pad_endpoint`]), or in a
//! seat, the ring the seat listener's enumerator fills ([`crate::audio::seat_pad`]).

use super::engine::{pad_audio_thread, KIND_BIT_HAPTICS, KIND_BIT_SPEAKER};
use super::*;

/// A source exists when the host has a provisioned endpoint, or in a seat, when the session came
/// in through the seat listener.
pub(super) fn host_cap(asked: bool, enabled: bool) -> bool {
    if pf_paths::seat::role().is_seat_host() {
        return asked && enabled && crate::audio::seat_pad::available();
    }
    // Startup can fail transiently with nothing latched; retry here — first session ask.
    if asked {
        crate::audio::pad_endpoint::ensure_provisioned();
    }
    asked
        && enabled
        && crate::audio::pad_endpoint::provisioned_endpoints().is_some_and(|eps| !eps.is_empty())
}

/// Stream `kinds` (bit 0 = haptics, bit 1 = speaker) toward `conn`. `stop` is this handle's own
/// flag. `None` if the slot has no endpoint (failed/still-running provision, or slot ≥
/// `PUNKTFUNK_PAD_AUDIO_SLOTS`) or spawn fails; the pad still works, without audio.
///
/// `pad` addresses the client's wire pad; `slot` is the host-wide OS slot whose endpoint —
/// stamped with the virtual pad's own container — is the thing being captured. They differ
/// whenever a session's pads arrive out of wire order, and every host serving two sessions.
pub(in crate::native) fn spawn(
    conn: super::link::SessionLink,
    pad: u8,
    slot: u8,
    kinds: u8,
    _edge: bool,
    stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    if kinds & (KIND_BIT_HAPTICS | KIND_BIT_SPEAKER) == 0 {
        return None;
    }
    if pf_paths::seat::role().is_seat_host() {
        return spawn_seat(conn, pad, slot, kinds, stop);
    }
    let Some(ep) = crate::audio::pad_endpoint::endpoint_for(slot) else {
        tracing::debug!(
            pad,
            slot,
            "pad-audio arrival for a slot without a provisioned endpoint — not streaming"
        );
        return None;
    };
    if ep.endpoint_id.is_empty() {
        // Devnode-without-endpoint (`find`): refuse rather than spin open/backoff on an empty id.
        return None;
    }
    if crate::audio::pad_endpoint::refuses_format(slot) {
        tracing::warn!(
            pad,
            slot,
            "pad endpoint refuses the 4-channel format — not streaming (pad_audio diagnostics row)"
        );
        return None;
    }
    if ep.needs_aeb_kick {
        // Stamps stored but not served — DualSense identity never adopted. Opening anyway is
        // worse: AUTOCONVERTPCM succeeds on a wrong-format endpoint, so the stream looks
        // healthy while haptics/speaker mis-route.
        tracing::warn!(
            pad,
            endpoint = %ep.endpoint_id,
            "pad endpoint stamps are stored but not served — the audio stack has not adopted the \
             DualSense identity (a reboot, or a manual AudioEndpointBuilder+Audiosrv restart, \
             clears it). Not streaming: the endpoint would open and mis-route."
        );
        return None;
    }
    let stop_t = stop.clone();
    let endpoint_id = ep.endpoint_id;
    let vis_id = endpoint_id.clone();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            // COM for the visibility flips; capturer opens run on their own thread.
            let _ = wasapi::initialize_mta();
            // Park HIDDEN with no pad attached: a visible idle "Wireless Controller" speaker
            // makes libScePad titles take the DualSense-haptics path against an unserviced
            // endpoint. Show only for this pad's lifetime; backoff absorbs audiosrv re-activate.
            let ticket = super::SHOWN.show(slot, || {
                crate::audio::pad_endpoint::set_visibility(&vis_id, pad, true)
            });
            // One repair per streamer: a refused format gets the policy-API ladder on the
            // endpoint this thread has just shown, then one more open.
            let repaired = AtomicBool::new(false);
            pad_audio_thread(
                conn,
                pad,
                kinds,
                move || {
                    use crate::audio::pad_capture::{is_unsupported_format, PadLoopbackCapturer};
                    match PadLoopbackCapturer::open(&endpoint_id) {
                        Err(e)
                            if is_unsupported_format(&e)
                                && !repaired.swap(true, Ordering::SeqCst) =>
                        {
                            crate::audio::pad_endpoint::repair_shown(slot, &endpoint_id);
                            PadLoopbackCapturer::open(&endpoint_id)
                        }
                        r => r,
                    }
                },
                stop_t,
            );
            // Skipped once a newer streamer shows this endpoint: hiding it now would disable
            // it under that streamer's capture (`ShowGen`).
            super::SHOWN.hide_if_newest(slot, ticket, || {
                crate::audio::pad_endpoint::set_visibility(&vis_id, pad, false)
            });
        }) {
        Ok(join) => Some(PadAudioHandle {
            stop,
            join: Some(join),
        }),
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio thread spawn failed — pad streams without audio");
            None
        }
    }
}

/// A seat's pad: claim the slot so its endpoint appears in this session, stream the ring, and
/// give the slot back when the streamer ends.
fn spawn_seat(
    conn: super::link::SessionLink,
    pad: u8,
    slot: u8,
    kinds: u8,
    stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    let source = match crate::audio::seat_pad::SeatPadSource::create(slot) {
        Ok(source) => source,
        Err(e) => {
            tracing::warn!(pad, slot, error = %format!("{e:#}"), "seat pad audio slot did not open — pad streams without audio");
            return None;
        }
    };
    let stop_t = stop.clone();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            pad_audio_thread(conn, pad, kinds, || Ok(source.capturer()), stop_t);
            drop(source);
        }) {
        Ok(join) => Some(PadAudioHandle {
            stop,
            join: Some(join),
        }),
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio thread spawn failed — pad streams without audio");
            None
        }
    }
}
