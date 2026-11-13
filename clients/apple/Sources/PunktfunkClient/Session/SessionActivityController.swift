// Owns the ActivityKit Live Activity lifecycle for a streaming session (iPhone/iPad only). Driven
// by ContentView from the session model's published state (phase / isBackgrounded / deadline) so
// none of this leaks into the cross-platform SessionModel. Local updates only (`pushType: nil`) —
// the app process is alive whenever there's a session to report, so there's no push token plumbing.
//
// Gated os(iOS): ActivityKit is iPhone/iPad only. Minimum deployment is iOS 17, so no @available
// guards are needed (Activity has existed since 16.1).

#if os(iOS)
import ActivityKit
import Foundation
// PunktfunkKit re-exports PunktfunkShared (@_exported), so the app target sees PunktfunkSessionAttributes
// without linking the Shared product directly — same pattern as StoredHost in HostStore.
import PunktfunkKit

@MainActor
final class SessionActivityController {
    private var activity: Activity<PunktfunkSessionAttributes>?
    /// The last pushed state, so an update can mutate one field and keep the rest (notably
    /// `startedAt`, which the Lock-Screen timer ticks from).
    private var state: PunktfunkSessionAttributes.ContentState?

    /// How far past the next expected update to mark the content stale — a frozen opt-out session
    /// then greys out instead of showing a lying clock.
    private static let staleWindow: TimeInterval = 90
    private static var swept = false

    /// End any Activity left over from a previous launch that was killed mid-session. Runs once
    /// per process: every window's appearance calls it, and a later window's sweep would end
    /// the Activity of a session live in the first.
    static func sweepOrphans() {
        guard !swept else { return }
        swept = true
        Task {
            for activity in Activity<PunktfunkSessionAttributes>.activities {
                await activity.end(nil, dismissalPolicy: .immediate)
            }
        }
    }

    /// Start the Live Activity for a freshly-streaming session. No-op if the user disabled Live
    /// Activities for the app, or one is already up.
    func begin(hostID: UUID, hostName: String, modeLine: String, startedAt: Date) {
        guard ActivityAuthorizationInfo().areActivitiesEnabled, activity == nil else { return }
        let attributes = PunktfunkSessionAttributes(hostID: hostID, hostName: hostName)
        let initial = PunktfunkSessionAttributes.ContentState(
            stage: .streaming, startedAt: startedAt, modeLine: modeLine)
        state = initial
        do {
            activity = try Activity.request(
                attributes: attributes,
                content: content(initial),
                pushType: nil)
        } catch {
            activity = nil
            state = nil
        }
    }

    /// Coalesced update: mutate the running state in place (keeps `startedAt` etc.) and push once.
    /// No-op when there's no live Activity.
    func update(_ mutate: (inout PunktfunkSessionAttributes.ContentState) -> Void) {
        guard let activity, var next = state else { return }
        mutate(&next)
        state = next
        Task { await activity.update(content(next)) }
    }

    /// End with a final "ended" state, dismissed a few seconds later.
    func end() {
        guard let activity, var final = state else {
            self.activity = nil
            state = nil
            return
        }
        self.activity = nil
        state = nil
        final.stage = .ending
        final.backgroundDeadline = nil
        Task {
            await activity.end(content(final), dismissalPolicy: .after(.now + 4))
        }
    }

    private func content(_ s: PunktfunkSessionAttributes.ContentState)
        -> ActivityContent<PunktfunkSessionAttributes.ContentState> {
        ActivityContent(state: s, staleDate: Date().addingTimeInterval(Self.staleWindow))
    }
}
#endif
