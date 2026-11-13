// App Intents that must compile into BOTH the app and the widget extension live here in the shared
// module. Today that's `EndStreamIntent` — the Live Activity's "End stream" button (a
// LiveActivityIntent runs in the APP's process) which M4 also surfaces to Siri/Shortcuts.
//
// Gated on os(iOS): LiveActivityIntent is part of ActivityKit's world (iPhone/iPad only). The M4
// Connect/Wake intents that need the app's router live in the app target, not here.

#if os(iOS)
import AppIntents
import Foundation

/// Ends the active streaming session. Backs the Live Activity's End button and the Shortcuts /
/// Siri "End the Punktfunk stream" phrase. `perform()` runs in the app's process (LiveActivityIntent)
/// — it posts `.punktfunkEndActiveSession`, which the app's SessionModel owner observes and turns
/// into `disconnect(deliberate: true)` (the user explicitly ended it → quit-close the host).
@available(iOS 17.0, *)
public struct EndStreamIntent: LiveActivityIntent {
    public static let title: LocalizedStringResource = LocalizedStringResource(
        "apple_intent_end_stream_title", defaultValue: "End Punktfunk Stream")
    public static let description = IntentDescription(
        LocalizedStringResource(
            "apple_intent_end_stream_desc",
            defaultValue: "Ends the active Punktfunk streaming session."))

    public init() {}

    public func perform() async throws -> some IntentResult {
        await MainActor.run {
            NotificationCenter.default.post(name: .punktfunkEndActiveSession, object: nil)
        }
        return .result()
    }
}
#endif
