// Wake a sleeping host and WAIT for it to come back before proceeding.
//
// A magic packet is fire-and-forget, and a cold box can take 20–60 s to POST, boot, and start
// answering again — far longer than a connect attempt will sit. The old path fired a packet and
// immediately dialed, so a genuinely-asleep host just failed. This drives a visible "Waking…"
// state instead: it polls `isOnline` about once a second, (re-)sends the packet while that stays
// false, and on success runs `onOnline` (the real connect for a Wake-&-Connect, or nothing for an
// explicit wake-only); on timeout it parks in a retry/cancel state. One wake at a time.
//
// `isOnline` is async because the only trustworthy answer costs a round trip: mDNS presence is a
// cache a sleeping host keeps warm for up to 75 minutes, so waiting on it both returned true for a
// host that never woke and never returned true for a routed host that does not advertise at all.

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
final class HostWaker: ObservableObject {
    struct Waking: Equatable {
        let hostID: UUID
        let hostName: String
        /// Whether coming online chains into a connect (Wake & Connect) vs. just stopping.
        let connectsAfter: Bool
        var seconds = 0
        var timedOut = false
    }

    /// nil = idle; non-nil drives the "Waking…" phase of `ConnectOverlay`.
    @Published private(set) var waking: Waking?

    /// How long to wait for the host to reappear before giving up. Generous — a cold boot + service
    /// start can be a minute-plus.
    private let timeoutSeconds = 90
    /// How often the packet is re-sent while the host stays down.
    private let resendEverySeconds = 6

    private var loop: Task<Void, Never>?
    /// Captured so "Try Again" replays the exact same wait.
    private var replay: (() -> Void)?

    /// Wake `host` and wait for `isOnline()` to go true, then run `onOnline`. `macs`/`lastIP` target
    /// the magic packet. No-ops straight to `onOnline` when there's nothing to wake with; a host
    /// that turns out to be up already (a race with the caller's check) falls out of the loop's
    /// first pass, before any packet is sent.
    func start(
        host: StoredHost, connectsAfter: Bool,
        macs: [String], lastIP: String?,
        isOnline: @escaping () async -> Bool, onOnline: @escaping () -> Void
    ) {
        guard !macs.isEmpty else {
            cancel()
            onOnline()
            return
        }
        replay = { [weak self] in
            self?.run(host: host, connectsAfter: connectsAfter, macs: macs, lastIP: lastIP,
                      isOnline: isOnline, onOnline: onOnline)
        }
        replay?()
    }

    /// Stop waiting and dismiss the overlay (B / Cancel).
    func cancel() {
        loop?.cancel()
        loop = nil
        replay = nil
        waking = nil
    }

    /// Restart the wait after a timeout (A / Try Again).
    func retry() { replay?() }

    private func run(
        host: StoredHost, connectsAfter: Bool, macs: [String], lastIP: String?,
        isOnline: @escaping () async -> Bool, onOnline: @escaping () -> Void
    ) {
        loop?.cancel()
        waking = Waking(hostID: host.id, hostName: host.displayName, connectsAfter: connectsAfter)
        let timeout = timeoutSeconds
        let resend = resendEverySeconds
        loop = Task { [weak self] in
            // Wall-clock, not a lap count: one `isOnline` costs a probe round trip, so laps are
            // longer than the sleep and a counted one would stretch both the timeout and the
            // seconds this shows.
            let started = Date()
            var sentAt: Int?
            while !Task.isCancelled {
                let elapsed = Int(Date().timeIntervalSince(started))
                if await isOnline() {
                    guard let self, !Task.isCancelled else { return }
                    self.waking = nil
                    self.loop = nil
                    // The wait is over, so "Try Again" has nothing to replay: leaving it armed
                    // lets a stray retry wake a host the user has already moved on from.
                    self.replay = nil
                    onOnline()
                    return
                }
                // A probe cancelled mid-flight answers false: no packet for a wait that ended.
                guard !Task.isCancelled else { return }
                if elapsed >= timeout {
                    self?.waking?.timedOut = true
                    self?.loop = nil
                    return
                }
                // Checked before sent, so a host that is already up never gets a packet. Re-sent
                // on a cadence because a single one can be missed, and some NICs only wake on a
                // fresh packet after dropping into a deeper sleep state.
                if sentAt.map({ elapsed - $0 >= resend }) ?? true {
                    sentAt = elapsed
                    Self.sendPacket(macs: macs, lastIP: lastIP)
                }
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                guard !Task.isCancelled else { return }
                self?.waking?.seconds = Int(Date().timeIntervalSince(started))
            }
        }
    }

    /// Blocking sends (see PunktfunkConnection.wakeOnLAN) — off the main thread.
    private static func sendPacket(macs: [String], lastIP: String?) {
        DispatchQueue.global(qos: .userInitiated).async {
            PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: lastIP)
        }
    }

    #if DEBUG
    /// Force a static waking state for the screenshot harness (no timers, no packets).
    func debugSet(_ w: Waking) { waking = w }
    #endif
}
