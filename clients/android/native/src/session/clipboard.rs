//! Shared-clipboard plane (text-only v1): Kotlin drives the Android `ClipboardManager`, these
//! shims drive [`punktfunk_core::client::NativeClient`]'s clipboard surface.
//!
//! Model (mirrors the desktop clients): opt-in via `nativeClipControl(true)`; local copies are
//! announced lazily as format-list offers (`nativeClipOfferText`) and the bytes cross only when
//! the host pastes (a `fetch` event answered by `nativeClipServeText`); a host copy arrives as an
//! `offer` event, which the Kotlin side fetches eagerly (Android's clipboard has no lazy provider
//! path worth the complexity) and lands in the system clipboard on the `data` event.
//!
//! Events cross to Kotlin as compact strings from the blocking `nativeNextClip` poll (drained on
//! a dedicated thread, same pattern as `nativeNextRumble`):
//! `state:<0|1>` · `offer:<seq>:<has_text 0|1>` · `fetch:<req_id>` · `data:<xfer_id>:<text>` ·
//! `cancel:<id>` · `error:<id>:<code>` · `closed` — null on a poll timeout. Non-text fetch
//! requests are cancelled natively (only text is ever offered, so they shouldn't occur).

use std::sync::Arc;
use std::time::Duration;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::clipboard::ClipEventCore;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::quic::{ClipKind, CLIP_FILE_INDEX_NONE, HOST_CAP_CLIPBOARD};

use super::{jni_guard, SessionHandle, SESSIONS};

/// The portable wire MIME both ends map to their platform text type.
const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Retain one keyed session for the duration of a clipboard JNI call.
fn client(handle: jlong) -> Option<Arc<SessionHandle>> {
    SESSIONS.get(handle)
}

/// `NativeBridge.nativeClipSupported(handle)` — the host advertised `HOST_CAP_CLIPBOARD`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipSupported(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        client(handle).is_some_and(|h| h.client.host_caps() & HOST_CAP_CLIPBOARD != 0)
    })
}

/// `NativeBridge.nativeHostMgmtPort(handle)` — the management-API port the host reported in this
/// session's `Welcome`, or `0` if it advertised none (older host / no management API).
///
/// Kotlin persists this on the host record, which is what lets the library screen reach a host that
/// moved its mgmt port off 47990 WITHOUT ever having seen an mDNS advert — the VPN / routed-subnet
/// / added-by-address cases, where the `mgmt` TXT the discovery path relies on never arrives.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostMgmtPort(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jint {
    jni_guard(0, || {
        client(handle).map_or(0, |h| jint::from(h.client.mgmt_port()))
    })
}

/// `NativeBridge.nativeClipControl(handle, enabled)` — session-level opt-in/out. Nothing
/// clipboard-related happens on either side until an `enabled: true` crosses.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipControl(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    enabled: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = client(handle) {
            let _ = h.client.clip_control(enabled, 0);
        }
    })
}

/// `NativeBridge.nativeClipOfferText(handle, seq)` — announce "the Android clipboard now holds
/// text" (format list only; bytes cross when the host fetches). `seq` is Kotlin's monotonic
/// counter, newest wins.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipOfferText(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    seq: jint,
) {
    jni_guard((), || {
        if let Some(h) = client(handle) {
            let _ = h.client.clip_offer(
                seq as u32,
                vec![ClipKind {
                    mime: TEXT_MIME.into(),
                    size_hint: 0,
                }],
            );
        }
    })
}

/// `NativeBridge.nativeClipFetchText(handle, seq)` — pull the text of the host's offer `seq`.
/// Returns the transfer id echoed on the matching `data:`/`error:` event, or −1.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipFetchText(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    seq: jint,
) -> jint {
    jni_guard(-1, || {
        client(handle)
            .and_then(|h| {
                h.client
                    .clip_fetch(seq as u32, TEXT_MIME.into(), CLIP_FILE_INDEX_NONE)
                    .ok()
            })
            .map_or(-1, |xfer| xfer as jint)
    })
}

/// `NativeBridge.nativeClipServeText(handle, reqId, text)` — answer a `fetch:` event with the
/// clipboard's current text (the host is pasting our offer).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipServeText(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    req_id: jint,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = client(handle) else {
            return Ok(());
        };
        // An unreadable payload still has to answer the host's `fetch:` — leaving it unanswered
        // stalls the paste — so cancel the transfer rather than propagating the error.
        let Ok(s) = text.try_to_string(env) else {
            let _ = h.client.clip_cancel(req_id as u32);
            return Ok(());
        };
        let _ = h.client.clip_serve(req_id as u32, s.into_bytes(), true);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeClipCancel(handle, id)` — abort a transfer (either direction).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipCancel(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    id: jint,
) {
    jni_guard((), || {
        if let Some(h) = client(handle) {
            let _ = h.client.clip_cancel(id as u32);
        }
    })
}

/// What one poll of the clipboard plane hands Kotlin.
#[derive(Debug, PartialEq, Eq)]
enum ClipPoll {
    /// The event as its protocol line (module docs).
    Line(String),
    /// A fetch for something other than text: the shim cancels it and returns null. Only text is
    /// ever offered, and an unanswered fetch stalls the host.
    CancelFetch(u32),
    /// The poll window passed with no event.
    Nothing,
}

/// Whether `mime` names text: the wire MIME and any other charset of it.
fn is_text(mime: &str) -> bool {
    mime.starts_with("text/plain")
}

/// The protocol line for one pulled clipboard event, or the one thing to do instead.
///
/// `data:<xfer_id>:<text>` decodes the bytes lossily. That is safe because the clipboard task
/// delivers a whole payload in ONE event (`last = true`), so a chunk boundary can never split a
/// UTF-8 sequence. Any pull error but a timeout means the plane is gone: `closed`.
fn clip_poll(pulled: Result<ClipEventCore, PunktfunkError>) -> ClipPoll {
    use ClipPoll::{CancelFetch, Line, Nothing};
    match pulled {
        Ok(ClipEventCore::State { enabled, .. }) => Line(format!("state:{}", u8::from(enabled))),
        Ok(ClipEventCore::RemoteOffer { seq, kinds }) => {
            let has_text = kinds.iter().any(|k| is_text(&k.mime));
            Line(format!("offer:{seq}:{}", u8::from(has_text)))
        }
        Ok(ClipEventCore::FetchRequest { req_id, mime, .. }) => {
            if is_text(&mime) {
                Line(format!("fetch:{req_id}"))
            } else {
                CancelFetch(req_id)
            }
        }
        Ok(ClipEventCore::Data { xfer_id, bytes, .. }) => Line(format!(
            "data:{xfer_id}:{}",
            String::from_utf8_lossy(&bytes)
        )),
        Ok(ClipEventCore::Cancelled { id }) => Line(format!("cancel:{id}")),
        Ok(ClipEventCore::Error { id, code }) => Line(format!("error:{id}:{code}")),
        Err(PunktfunkError::NoFrame) => Nothing,
        Err(_) => Line("closed".into()),
    }
}

/// `NativeBridge.nativeNextClip(handle)` — block ≤250 ms for the next clipboard event, encoded
/// as a compact string ([`clip_poll`]); null on timeout, `"closed"` once the session is gone.
/// Call from a dedicated poll thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextClip<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    // `JString::default()` is the null reference of the "null on timeout" contract.
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let Some(h) = client(handle) else {
            return env.new_string("closed");
        };
        match clip_poll(h.client.next_clip(Duration::from_millis(250))) {
            ClipPoll::Line(msg) => env.new_string(msg),
            ClipPoll::CancelFetch(req_id) => {
                let _ = h.client.clip_cancel(req_id);
                Ok(JString::default())
            }
            ClipPoll::Nothing => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::clipboard::INBOUND_REQ_FLAG;
    use ClipPoll::{CancelFetch, Line, Nothing};

    type Pulled = Result<ClipEventCore, PunktfunkError>;

    fn line(s: &str) -> ClipPoll {
        Line(s.to_string())
    }

    fn fetch(req_id: u32, mime: &str) -> Pulled {
        Ok(ClipEventCore::FetchRequest {
            req_id,
            seq: 1,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: mime.to_string(),
        })
    }

    fn offer(seq: u32, mimes: &[&str]) -> Pulled {
        let kinds = mimes
            .iter()
            .map(|m| ClipKind {
                mime: m.to_string(),
                size_hint: 0,
            })
            .collect();
        Ok(ClipEventCore::RemoteOffer { seq, kinds })
    }

    fn data(xfer_id: u32, bytes: &[u8]) -> Pulled {
        Ok(ClipEventCore::Data {
            xfer_id,
            bytes: bytes.to_vec(),
            last: true,
        })
    }

    #[test]
    fn a_poll_that_ends_the_stream_or_needs_a_cancel_says_so_first() {
        let rows: Vec<(&str, Pulled, ClipPoll)> = vec![
            ("a poll timeout", Err(PunktfunkError::NoFrame), Nothing),
            (
                "a dropped plane",
                Err(PunktfunkError::Closed),
                line("closed"),
            ),
            (
                "a timeout error is the plane gone",
                Err(PunktfunkError::Timeout),
                line("closed"),
            ),
            (
                "a socket error is the plane gone",
                Err(PunktfunkError::Io(std::io::Error::other("reset"))),
                line("closed"),
            ),
            (
                "a fetch for an image",
                fetch(INBOUND_REQ_FLAG | 4, "image/png"),
                CancelFetch(INBOUND_REQ_FLAG | 4),
            ),
            ("a fetch with no MIME", fetch(7, ""), CancelFetch(7)),
            ("a fetch for html", fetch(7, "text/html"), CancelFetch(7)),
            (
                "the MIME match is case-sensitive",
                fetch(7, "TEXT/PLAIN"),
                CancelFetch(7),
            ),
            (
                "text/plain must open the MIME",
                fetch(7, "application/x-text/plain"),
                CancelFetch(7),
            ),
        ];
        for (name, pulled, want) in rows {
            assert_eq!(clip_poll(pulled), want, "{name}");
        }
    }

    #[test]
    fn a_host_event_becomes_the_line_kotlin_parses() {
        let rows: Vec<(&str, Pulled, ClipPoll)> = vec![
            (
                "sharing on",
                Ok(ClipEventCore::State {
                    enabled: true,
                    policy: 3,
                    reason: 9,
                }),
                line("state:1"),
            ),
            (
                "sharing off",
                Ok(ClipEventCore::State {
                    enabled: false,
                    policy: 0,
                    reason: 0,
                }),
                line("state:0"),
            ),
            ("an offer with no kinds", offer(3, &[]), line("offer:3:0")),
            (
                "an offer of an image",
                offer(3, &["image/png"]),
                line("offer:3:0"),
            ),
            (
                "an offer of text",
                offer(3, &["text/plain;charset=utf-8"]),
                line("offer:3:1"),
            ),
            (
                "an offer of bare text/plain",
                offer(3, &["text/plain"]),
                line("offer:3:1"),
            ),
            (
                "text among other kinds",
                offer(3, &["image/png", "text/html", "text/plain;charset=utf-8"]),
                line("offer:3:1"),
            ),
            (
                "an offer matches on the text/plain prefix",
                offer(3, &["text/plainish"]),
                line("offer:3:1"),
            ),
            (
                "the largest offer sequence",
                offer(u32::MAX, &["text/plain"]),
                line("offer:4294967295:1"),
            ),
            (
                "a text fetch",
                fetch(5, "text/plain;charset=utf-8"),
                line("fetch:5"),
            ),
            // Core numbers a host paste with its high bit set, and Kotlin reads this id with
            // `toIntOrNull`, which refuses anything above `Int.MAX_VALUE`.
            (
                "a host paste's request id carries core's high bit",
                fetch(INBOUND_REQ_FLAG | 1, "text/plain"),
                line("fetch:2147483649"),
            ),
            (
                "a cancelled transfer",
                Ok(ClipEventCore::Cancelled { id: 9 }),
                line("cancel:9"),
            ),
            (
                "a failed transfer carries its negative code",
                Ok(ClipEventCore::Error { id: 9, code: -3 }),
                line("error:9:-3"),
            ),
        ];
        for (name, pulled, want) in rows {
            assert_eq!(clip_poll(pulled), want, "{name}");
        }
    }

    #[test]
    fn clip_text_rides_the_line_decoded_lossily_and_whole() {
        let rows: Vec<(&str, Pulled, ClipPoll)> = vec![
            ("empty text", data(5, b""), line("data:5:")),
            ("plain text", data(5, b"hello"), line("data:5:hello")),
            (
                "colons stay in the text",
                data(5, b"a:b:c"),
                line("data:5:a:b:c"),
            ),
            (
                "newlines stay in the text",
                data(5, b"one\ntwo"),
                line("data:5:one\ntwo"),
            ),
            (
                "multi-byte text is intact",
                data(5, "naïve 😀".as_bytes()),
                line("data:5:naïve 😀"),
            ),
            (
                "an invalid byte becomes U+FFFD",
                data(5, &[b'a', 0xff, b'b']),
                line("data:5:a\u{fffd}b"),
            ),
            (
                "a cut UTF-8 sequence becomes U+FFFD",
                data(5, &"é".as_bytes()[..1]),
                line("data:5:\u{fffd}"),
            ),
            (
                "the transfer id leads",
                data(u32::MAX, b"x"),
                line("data:4294967295:x"),
            ),
        ];
        for (name, pulled, want) in rows {
            assert_eq!(clip_poll(pulled), want, "{name}");
        }
    }
}
