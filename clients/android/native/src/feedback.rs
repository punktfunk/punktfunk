//! Host→client gamepad feedback pulls (Option B): blocking JNI shims that forward to the connector's
//! rumble (0xCA) / HID-output (0xCD) planes and return one decoded event. Kotlin owns the poll
//! threads + the Android Vibrator/Lights rendering (see `GamepadFeedback.kt`) — no JNI upcalls, no
//! `JavaVM` attach, no cached method ids. Mirrors the audio plane's one-thread-per-plane contract,
//! except the thread lives in Kotlin and we just expose the blocking pull.
//!
//! Not android-gated: `next_rumble`/`next_hidout` are pure-Rust on the `quic` feature, so these
//! compile on the host build too (parity with the input shims in [`crate::session`]).

use crate::session::{get_session, jni_guard};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteBuffer, JObject};
use jni::sys::{jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::quic::HidOutput;
use std::time::Duration;

/// Short blocking timeout: long enough not to busy-spin, short enough that the Kotlin poll thread
/// observes its `running=false` flag promptly on teardown.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

/// Width of the packed `pad` field in [`pack_rumble`] — 4 bits, i.e. indices 0..15.
const PAD_BITS: u32 = 4;
/// The packing is only lossless while every representable pad index fits in [`PAD_BITS`]. This was
/// a comment before; growing `MAX_PADS` past 16 would have silently aliased pad 16 onto pad 0
/// rather than failing the build.
const _: () = assert!(
    punktfunk_core::input::MAX_PADS <= 1usize << PAD_BITS,
    "MAX_PADS no longer fits the 4-bit pad field in the packed rumble long"
);

/// Pack one effective rumble command into the `jlong` `nativeNextRumble` returns.
///
/// Layout — mirrored by `unpackRumbleEvent` in `RumbleWire.kt`: bits 49..52 `pad`, 32..47
/// `backstop_ms`, 16..31 `low`, 0..15 `high`. Always non-negative, so the `-1` timeout/closed
/// sentinel stays unambiguous. Split out from the JNI entry point purely so it can be tested
/// without a live session handle — the shift arithmetic is the part worth pinning.
fn pack_rumble(pad: u16, low: u16, high: u16, backstop_ms: u32) -> jlong {
    (jlong::from(pad & ((1 << PAD_BITS) - 1)) << 49)
        | (jlong::from(backstop_ms.min(0xFFFF) as u16) << 32)
        | (jlong::from(low) << 16)
        | jlong::from(high)
}

// HID-output kind tags written into the returned ByteBuffer (Kotlin reads them back).
const TAG_LED: u8 = 0x01;
const TAG_PLAYER_LEDS: u8 = 0x02;
const TAG_TRIGGER: u8 = 0x03;
const TAG_HID_RAW: u8 = 0x05;
const TAG_MIC_LED: u8 = 0x07;

/// `NativeBridge.nativeNextRumble(handle): Long` — block up to ~100 ms for the next EFFECTIVE
/// rumble command from the core's shared policy engine (`design/rumble-root-fix.md` §D). The
/// engine owns ALL rumble policy — v2 lease expiry, legacy-host staleness (a uniform 1 s, ending
/// the old 60 s Android exposure), connection-close drain zeros — so Kotlin applies commands
/// verbatim: `(0, 0)` = cancel now, non-zero = one-shot at this level.
///
/// Returns a packed positive long: bits 49..52 = wire `pad` index (0..15), bits 32..47 = the
/// command's `backstop_ms` (≤ 5000 — the one-shot duration, i.e. the hardware net under a stalled
/// poll thread; the engine emits explicit zeros at every policy stop, so it is never the stop
/// mechanism), bits 16..31 = `low`, bits 0..15 = `high` (0..=0xFFFF). `-1` on timeout / session
/// closed (all packed values are positive, so `-1` stays unambiguous). Kotlin routes the command
/// back to the controller holding that wire `pad` index (multi-pad rumble). Run from a Kotlin
/// poll thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextRumble(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jlong {
    // Runs on a Kotlin poll thread, so a panic here would abort the process; guard the boundary.
    jni_guard(-1, || {
        let Some(h) = get_session(handle) else {
            return -1;
        };
        match h.client.next_rumble_command(PULL_TIMEOUT) {
            // A pad whose coils are ACTIVELY being driven by the 0xD1 haptics stream must not see
            // wire rumble: `DsDevice` sets `valid_flag0` bit 1 (`HAPTICS_SELECT`) on every rumble
            // write, and that bit disables the audio-haptics path — so one replayed command would
            // mute the coils the stream is driving. Gating on *arrival of haptics frames* rather
            // than on "a stream is open" is what keeps a rumble-only title working: it renders no
            // haptics audio, so the host emits nothing on 0xD1 and the pad keeps its rumble.
            // Dropping it here rather than in Kotlin keeps the rule next to the reason.
            Ok(cmd) if crate::pad_audio::haptics_owns_coils((cmd.pad & 0xF) as u8) => -1,
            Ok(cmd) => pack_rumble(cmd.pad, cmd.low, cmd.high, cmd.backstop_ms),
            Err(_) => -1, // NoFrame (timeout) or Closed — Kotlin loops on its running flag
        }
    })
}

/// `NativeBridge.nativeNextHidout(handle, buf): Int` — block up to ~100 ms for the next DualSense
/// HID-output event, written into the caller's direct ByteBuffer as `[pad][kind][fields…]` (the
/// leading `pad` is the wire pad index the event is addressed to, so Kotlin routes it to that
/// controller — multi-pad HID feedback):
///   Led        → `[pad][0x01][r][g][b]`         (len 5)
///   PlayerLeds → `[pad][0x02][bits]`            (len 3)
///   Trigger    → `[pad][0x03][which][effect…]`  (len 3 + effect.len())
///   MicLed     → `[pad][0x07][mode]`            (len 3)
/// Returns the byte count written, or `-1` on timeout / session closed / an event with no
/// Android replay (dropped). A buffer too small for the event is logged.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextHidout(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    buf: JByteBuffer,
) -> jint {
    // Runs on a Kotlin poll thread, so a panic here would abort the process; guard the boundary.
    //
    // Deliberately `with_env_no_catch` INSIDE `jni_guard`, not the usual `with_env`: every error
    // policy resolves a failure to `T::default()`, and `jint::default()` is 0 — a *valid* byte
    // count — whereas this method's contract says -1. Letting the panic travel out to `jni_guard`
    // keeps the -1 sentinel exact. Every non-panic failure path below likewise returns `Ok(-1)`
    // rather than `Err`, so the policy's default is unreachable by construction.
    jni_guard(-1, || {
        env.with_env_no_catch(|env| -> jni::errors::Result<jint> {
            let Some(h) = get_session(handle) else {
                return Ok(-1);
            };
            let ev = match h.client.next_hidout(PULL_TIMEOUT) {
                Ok(ev) => ev,
                Err(_) => return Ok(-1), // timeout or closed — Kotlin loops
            };
            // `[pad][tag][head…][tail…]` — the two slices spare a concat per event.
            let (pad, tag, head, tail): (u8, u8, &[u8], &[u8]) = match &ev {
                HidOutput::Led { pad, r, g, b } => (*pad, TAG_LED, &[*r, *g, *b], &[]),
                HidOutput::PlayerLeds { pad, bits } => (*pad, TAG_PLAYER_LEDS, &[*bits], &[]),
                HidOutput::MicLed { pad, mode } => (*pad, TAG_MIC_LED, &[*mode], &[]),
                HidOutput::Trigger { pad, which, effect } => {
                    (*pad, TAG_TRIGGER, &[*which], effect.as_slice())
                }
                // As-is SC2 passthrough: the host's hidraw consumer (Steam) wrote this report to
                // the virtual pad; Kotlin replays it verbatim on the physical controller.
                // `kind` 0 = output report, 1 = feature report.
                HidOutput::HidRaw { pad, kind, data } => {
                    (*pad, TAG_HID_RAW, &[*kind], data.as_slice())
                }
                // Steam Controller trackpad-coil haptics and DS5 pad-audio routing have no
                // Android replay path (motor rumble already rides the universal 0xCA plane).
                HidOutput::TrackpadHaptic { .. } | HidOutput::AudioCtl { .. } => return Ok(-1),
            };
            let n = 2 + head.len() + tail.len();
            // The caller passes a direct ByteBuffer (allocateDirect) so we write its backing
            // store directly. A buffer that cannot take the event is a Kotlin-side sizing bug
            // (reports are ≤ 64 bytes; Kotlin allocates 128), not a transient — say so.
            let cap = env.get_direct_buffer_capacity(&buf).unwrap_or(0);
            let ptr = env
                .get_direct_buffer_address(&buf)
                .unwrap_or(std::ptr::null_mut());
            if ptr.is_null() || cap < n {
                log::warn!(
                    "feedback: HID event tag {tag} needs {n} bytes, buffer holds {cap} — dropped"
                );
                return Ok(-1);
            }
            // SAFETY: `ptr`/`cap` describe the direct ByteBuffer's backing store, valid for this call.
            let out = unsafe { std::slice::from_raw_parts_mut(ptr, cap) };
            out[0] = pad;
            out[1] = tag;
            out[2..2 + head.len()].copy_from_slice(head);
            out[2 + head.len()..n].copy_from_slice(tail);
            Ok(n as jint)
        })
        .resolve::<LogErrorAndDefault>()
    })
}

#[cfg(test)]
mod pack_rumble_tests {
    use super::*;
    use punktfunk_core::input::MAX_PADS;

    /// Kotlin's `unpackRumbleEvent`, transcribed — if these two ever disagree the boundary is
    /// broken, and nothing else in the build would say so.
    fn unpack(ev: jlong) -> (u16, u16, u16, u32) {
        let pad = ((ev >> 49) & 0xF) as u16;
        let backstop = ((ev >> 32) & 0xFFFF) as u32;
        let low = ((ev >> 16) & 0xFFFF) as u16;
        let high = (ev & 0xFFFF) as u16;
        (pad, low, high, backstop)
    }

    #[test]
    fn round_trips_every_field_at_its_extremes() {
        for &(pad, low, high, backstop) in &[
            (0u16, 0u16, 0u16, 0u32),
            (15, 0xFFFF, 0xFFFF, 0xFFFF),
            (1, 0x1234, 0x5678, 500),
            (7, 0, 0xFFFF, 2000),
        ] {
            let ev = pack_rumble(pad, low, high, backstop);
            assert_eq!(unpack(ev), (pad, low, high, backstop), "pad {pad}");
        }
    }

    #[test]
    fn every_representable_pad_survives_the_four_bit_field() {
        for pad in 0..MAX_PADS as u16 {
            let (got, ..) = unpack(pack_rumble(pad, 1, 2, 3));
            assert_eq!(got, pad, "pad {pad} aliased in the packed long");
        }
    }

    #[test]
    fn a_packed_command_is_never_negative() {
        // `-1` is the timeout/closed sentinel; any packed value colliding with it would read as
        // "no command" and the rumble would simply vanish.
        assert!(pack_rumble(15, 0xFFFF, 0xFFFF, 0xFFFF) >= 0);
        assert!(pack_rumble(0, 0, 0, 0) >= 0);
    }

    #[test]
    fn an_oversized_backstop_saturates_instead_of_corrupting_the_pad_field() {
        let ev = pack_rumble(3, 0, 0, u32::MAX);
        let (pad, _, _, backstop) = unpack(ev);
        assert_eq!(pad, 3, "a huge backstop must not bleed into the pad bits");
        assert_eq!(backstop, 0xFFFF);
    }

    #[test]
    fn a_stop_is_distinguishable_from_a_hold() {
        let stop = pack_rumble(2, 0, 0, 0);
        let hold = pack_rumble(2, 0x8000, 0x8000, 500);
        assert_ne!(stop, hold);
        assert_eq!(unpack(stop).1, 0);
        assert_eq!(unpack(stop).2, 0);
    }
}
