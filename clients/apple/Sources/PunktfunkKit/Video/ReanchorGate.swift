// Swift wrapper around the punktfunk-core C ABI's post-loss re-anchor gate
// (`punktfunk_reanchor_gate_*`, ABI v6). The shared Rust gate (crates/core/punktfunk-core/src/reanchor.rs)
// is what the Linux/Windows desktop pump and the Android client use directly; the Swift clients reach
// it across the C ABI so the freeze-until-reanchor policy is defined ONCE for every platform.
//
// Why a freeze at all: after unrecoverable loss the host keeps sending delta frames that reference a
// picture the client never got. Hardware decoders (VideoToolbox included) don't reliably error on
// that — they CONCEAL, returning a gray/garbage frame with a success status. Presenting those is the
// visible "gray flash with motion" of the loss reports. The gate withholds concealed frames and holds
// the last good picture on glass until a PROVEN clean re-anchor lands — an IDR (wire `FLAG_SOF`), an
// RFI recovery anchor (`USER_FLAG_RECOVERY_ANCHOR`), or the 2nd of two intra-refresh recovery marks
// (`USER_FLAG_RECOVERY_POINT`) — with a bounded backstop so a lost re-anchor can never freeze forever.
// See punktfunk-planning design/client-reanchor-freeze-parity.md.
//
// Threading: one gate per session. Its calls arrive from two threads — the pump thread (`arm` on a
// frame-index gap / a submit failure, `poll` per iteration) and a VideoToolbox decode thread
// (`onDecoded` per decoded frame, `onNoOutput` on a decode error). The raw Rust gate is a plain
// struct behind an opaque pointer with no internal synchronization, so every call is serialized under
// `lock` here — the calls are cheap field updates, so contention is negligible. `@unchecked Sendable`:
// the lock enforces the contract.

import Foundation
import PunktfunkCore

final class ReanchorGate: @unchecked Sendable {
    private let lock = NSLock()
    /// The opaque `ReanchorGate *`. `var` so `reseed` can swap it at session start. Never NULL
    /// (`punktfunk_reanchor_gate_new` never returns NULL).
    private var ptr: OpaquePointer

    /// Seed the baseline with the connection's current `framesDropped` so the first `poll` doesn't
    /// read the session's starting drop count as a fresh loss.
    init(framesDropped: UInt64) {
        ptr = punktfunk_reanchor_gate_new(framesDropped)
    }

    deinit { punktfunk_reanchor_gate_free(ptr) }

    /// Re-anchor the drop-count baseline to `framesDropped` for a (re)started session. The gate is
    /// created in the pipeline's init (before a connection exists, seeded 0); `start` calls this once
    /// the live connection's count is known so a mid-life connection's non-zero baseline isn't
    /// mistaken for loss on the first poll.
    func reseed(framesDropped: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        punktfunk_reanchor_gate_free(ptr)
        ptr = punktfunk_reanchor_gate_new(framesDropped)
    }

    /// Arm the freeze: a loss was detected (a frame-index gap, or a decoder wedge). Zeroes the
    /// recovery-mark count and (re)sets the backstop deadline.
    func arm() {
        lock.lock()
        punktfunk_reanchor_gate_arm(ptr)
        lock.unlock()
    }

    /// `arm()` for a loss detected as a frame-index gap of a known width
    /// (`PunktfunkConnection.noteFrameIndexGapWidth`). Pre-credits the reassembler's later
    /// `framesDropped` climb for the same lost frames, so `poll` doesn't re-freeze a stream an
    /// RFI anchor already healed (the double-arm race — the Rust gate's docs tell the story).
    func arm(expectingDrops: UInt64) {
        lock.lock()
        punktfunk_reanchor_gate_arm_expecting_drops(ptr, expectingDrops)
        lock.unlock()
    }

    /// Fold one decoded frame. `flags` is the AU's wire `user_flags`. Returns true to PRESENT the
    /// frame, false to WITHHOLD it as a post-loss concealment (hold the last good picture). Pass
    /// `decoderKeyframe: false` — VideoToolbox doesn't flag IDRs, so the wire `FLAG_SOF` covers it.
    func onDecoded(flags: UInt32, decoderKeyframe: Bool = false) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        var present = false
        _ = punktfunk_reanchor_gate_on_decoded(ptr, flags, decoderKeyframe, &present)
        return present
    }

    /// A received AU produced no decoded frame (a VideoToolbox decode error). Returns true when the
    /// no-output streak has tripped (the gate armed the freeze) and the caller should — throttled —
    /// request a keyframe.
    func onNoOutput() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        var requestKf = false
        _ = punktfunk_reanchor_gate_on_no_output(ptr, &requestKf)
        return requestKf
    }

    /// Periodic fold of the session's `framesDropped` plus the overdue backstop. Returns true when the
    /// caller should — throttled — request a keyframe (a drop-count climb armed a fresh freeze, or the
    /// freeze is overdue and re-asks while it keeps holding).
    func poll(framesDropped: UInt64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        var requestKf = false
        _ = punktfunk_reanchor_gate_poll(ptr, framesDropped, &requestKf)
        return requestKf
    }
}
