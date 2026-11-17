// Keeps the local display awake for the duration of a streaming session.
//
// A stream is not "user activity" to the OS: the pixels arrive over the network and the input that
// drives them is often a game controller, and Apple requires a game driven by one to disable the
// idle timer itself (iOS, tvOS). So a controller-only session can idle the panel out from under
// the user — the same reason the Android client holds FLAG_KEEP_SCREEN_ON while streaming
// (StreamWindow.kt).
//
// Held by SessionModel from `beginStreaming` to `disconnect`, so it is scoped to the session and
// never leaks past it (including a host-ended or timed-out background session, which both land in
// `disconnect`). With "Keep screen awake" off, the session also drops it while the player's
// input is quiet and takes it back on their next one.

import Foundation

#if os(macOS)
    import IOKit.pwr_mgt
#else
    import UIKit
#endif

@MainActor
final class DisplaySleepGuard {
    #if os(macOS)
        /// The `beginActivity` token; non-nil exactly while held.
        private var activity: NSObjectProtocol?
        /// Re-used across heartbeats so the whole session shares one assertion instead of
        /// accumulating one per tick.
        private var userActivityAssertion: IOPMAssertionID = IOPMAssertionID(0)
        private var heartbeat: Timer?

        /// The power assertion defers DISPLAY SLEEP but not the screen saver — that runs off the
        /// HID idle timer, which a controller-only session never touches. Declaring user activity
        /// on an interval well under the shortest selectable screen-saver delay (1 minute) keeps
        /// that timer from ever reaching it. Side effect, and the intended one: an idle-lock
        /// configured to follow the screen saver is deferred too, for the session only.
        private static let heartbeatInterval: TimeInterval = 30
    #endif

    private(set) var isHeld = false

    /// Idempotent — a second acquire while held is a no-op.
    func acquire() {
        guard !isHeld else { return }
        isHeld = true
        #if os(macOS)
            // The high-level Foundation API over IOKit power assertions: `.idleDisplaySleepDisabled`
            // is the panel, `.userInitiated` also holds off idle SYSTEM sleep and sudden termination
            // for a session the user is watching in real time.
            activity = ProcessInfo.processInfo.beginActivity(
                options: [.userInitiated, .idleDisplaySleepDisabled],
                reason: "Punktfunk streaming session")
            declareUserActivity()
            let timer = Timer.scheduledTimer(withTimeInterval: Self.heartbeatInterval, repeats: true) {
                [weak self] _ in
                MainActor.assumeIsolated { self?.declareUserActivity() }
            }
            // The stream runs under a tracking run-loop mode while a menu or a window resize is up;
            // .common keeps the heartbeat ticking through those.
            RunLoop.main.add(timer, forMode: .common)
            heartbeat = timer
        #else
            // iOS/iPadOS/tvOS/visionOS: app-wide, and ignored while backgrounded — the background
            // keep-alive (audio-only, video dropped) lets the device sleep without touching this.
            Self.holders += 1
            UIApplication.shared.isIdleTimerDisabled = true
        #endif
    }

    #if !os(macOS)
        /// Guards holding the app-wide idle timer off. An iPad runs a session per window, and
        /// one window's disconnect must not re-arm the timer under another's stream.
        private static var holders = 0
    #endif

    /// Idempotent — safe to call when not held (`disconnect` runs on paths that never streamed).
    func release() {
        guard isHeld else { return }
        isHeld = false
        #if os(macOS)
            heartbeat?.invalidate()
            heartbeat = nil
            if let activity {
                ProcessInfo.processInfo.endActivity(activity)
                self.activity = nil
            }
            if userActivityAssertion != IOPMAssertionID(0) {
                IOPMAssertionRelease(userActivityAssertion)
                userActivityAssertion = IOPMAssertionID(0)
            }
        #else
            Self.holders -= 1
            UIApplication.shared.isIdleTimerDisabled = Self.holders > 0
        #endif
    }

    #if os(macOS)
        /// Resets the HID idle timer (see `heartbeatInterval`). `kIOPMUserActiveLocal` = activity at
        /// this Mac's own display, which is what a stream being watched here is.
        private func declareUserActivity() {
            IOPMAssertionDeclareUserActivity(
                "Punktfunk streaming session" as CFString,
                kIOPMUserActiveLocal,
                &userActivityAssertion)
        }
    #endif
}
