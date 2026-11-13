// Siri / Shortcuts / Spotlight surface (design §M4, extended by client-deep-links.md §6).
// Deliberately thin: every action already has an internal entry point — the deep-link router
// (connect / connect-and-launch / connect-with-a-preset, and the `browse` route into a host's
// library), the in-process end-session hook, and the existing Wake-on-LAN path — so these
// intents only wrap them.
//
// Connect and Wake compile on macOS and tvOS too: AppIntents is genuinely available there
// (macOS 13+ / tvOS 16+), and "Stream Desktop with Work" from Spotlight on a Mac is part of the
// ask. Only the AppShortcutsProvider stays iOS-gated, because it bundles `EndStreamIntent` — a
// LiveActivityIntent, and ActivityKit is iPhone/iPad only.
//
// `HostEntity` and `ProfileEntity` (the parameter types) live in PunktfunkShared so a widget's
// configuration intent, which executes in the EXTENSION process, can share them.

#if canImport(AppIntents)
import AppIntents
import Foundation
import PunktfunkKit

/// Load a full saved host (MACs, address) from the shared App-Group store by id — HostEntity only
/// carries id + name.
private func loadStoredHost(_ id: UUID) -> StoredHost? {
    StoredHost.loadAll(recentFirst: false).first { $0.id == id }
}

/// Start a session with a stored host (optionally launching a title, optionally with a settings
/// preset). Foregrounds the app and routes through the SAME `.onOpenURL` path a widget tap uses —
/// trust policy, WoL and the approval sheet all apply, and its guards (unknown host, ambiguous or
/// unknown preset, already-streaming) hold. One router, not a second connect path.
struct ConnectToHostIntent: AppIntent {
    static let title: LocalizedStringResource = LocalizedStringResource("apple_intent_connect_title", defaultValue: "Connect to Host")
    static let description = IntentDescription(
        LocalizedStringResource("apple_intent_connect_desc", defaultValue: "Start a Punktfunk streaming session with a host."))
    static let openAppWhenRun = true

    @Parameter(title: LocalizedStringResource("apple_intent_param_host", defaultValue: "Host")) var host: HostEntity
    @Parameter(
        title: LocalizedStringResource("apple_intent_param_game_id", defaultValue: "Game ID"),
        description: LocalizedStringResource("apple_intent_param_game_id_desc", defaultValue: "Optional store id like steam:570"))
    var launchID: String?
    @Parameter(
        title: LocalizedStringResource("apple_intent_param_preset", defaultValue: "Preset"),
        description: LocalizedStringResource(
            "apple_intent_param_preset_desc",
            defaultValue: "Which settings preset to use — leave empty for the host's default"))
    var profile: ProfileEntity?

    func perform() async throws -> some IntentResult {
        let url = DeepLink.connect(host: host.id, launchID: launchID, preset: profile?.id).url
        await MainActor.run { DeepLinkInbox.post(url) }
        return .result()
    }
}

/// Jump straight into a host's game library — no session. Foregrounds the app and routes the
/// `browse` route through the same `.onOpenURL` path a widget tap uses, which drives the one
/// `libraryTarget` every surface shares — so the shortcut lands in whichever library presentation
/// the current mode owns: the console's shelf when the console is up, the touch/desktop library
/// otherwise. A session starts only when a title is picked there.
struct OpenLibraryIntent: AppIntent {
    static let title: LocalizedStringResource = LocalizedStringResource("apple_intent_open_library_title", defaultValue: "Open Game Library")
    static let description = IntentDescription(
        LocalizedStringResource(
            "apple_intent_open_library_desc",
            defaultValue: "Open a host's game library in Punktfunk, without starting a stream."))
    static let openAppWhenRun = true

    @Parameter(title: LocalizedStringResource("apple_intent_param_host", defaultValue: "Host")) var host: HostEntity

    func perform() async throws -> some IntentResult {
        let url = DeepLink.browse(host: host.id).url
        await MainActor.run { DeepLinkInbox.post(url) }
        return .result()
    }
}

/// Wake a sleeping host (magic packet). No `openAppWhenRun` — usable in automations ("when I get
/// home, wake the tower") without foregrounding the app.
struct WakeHostIntent: AppIntent {
    static let title: LocalizedStringResource = LocalizedStringResource("apple_intent_wake_title", defaultValue: "Wake Host")
    static let description = IntentDescription(
        LocalizedStringResource("apple_intent_wake_desc", defaultValue: "Send a Wake-on-LAN magic packet to a host."))

    @Parameter(title: LocalizedStringResource("apple_intent_param_host", defaultValue: "Host")) var host: HostEntity

    func perform() async throws -> some IntentResult {
        guard let stored = loadStoredHost(host.id), WakePolicy.canWake(stored.wakeMacs) else {
            throw IntentError.noWakeAddress
        }
        PunktfunkConnection.wakeOnLAN(macs: stored.wakeMacs, lastKnownIP: stored.address)
        return .result()
    }
}

/// Errors surfaced to Siri/Shortcuts. `CustomLocalizedStringResourceConvertible` makes the message
/// show as the intent's failure text.
enum IntentError: Error, CustomLocalizedStringResourceConvertible {
    case noWakeAddress

    var localizedStringResource: LocalizedStringResource {
        switch self {
        case .noWakeAddress:
            return L10n.appleIntentErrorNoWake
        }
    }
}

#if os(iOS)
/// Zero-setup Siri / Spotlight phrases. Parameterized phrases resolve a `HostEntity` by name; stays
/// well under the 10-shortcut cap. Phrases stay host-parameterized — App Shortcut phrases can't
/// embed a second arbitrary `AppEntity`, so the preset is picked in the Shortcuts editor.
///
/// A phrase must be a string literal. `AppShortcuts.xcstrings`, which `pf-l10n` writes from the
/// `apple_shortcut_*_phrase` messages, is keyed by these exact English phrases.
struct PunktfunkShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: ConnectToHostIntent(),
            phrases: [
                "Connect to \(\.$host) in \(.applicationName)",
                "Stream \(\.$host) with \(.applicationName)",
            ],
            shortTitle: LocalizedStringResource("apple_shortcut_connect_short", defaultValue: "Connect"), systemImageName: "play.tv.fill")
        AppShortcut(
            intent: OpenLibraryIntent(),
            phrases: [
                "Open \(\.$host) library in \(.applicationName)",
                "Show \(\.$host) games in \(.applicationName)",
            ],
            shortTitle: LocalizedStringResource("apple_shortcut_library_short", defaultValue: "Game Library"), systemImageName: "square.grid.2x2.fill")
        AppShortcut(
            intent: WakeHostIntent(),
            phrases: [
                "Wake \(\.$host) with \(.applicationName)",
            ],
            shortTitle: LocalizedStringResource("apple_shortcut_wake_short", defaultValue: "Wake Host"), systemImageName: "power")
        AppShortcut(
            intent: EndStreamIntent(),
            phrases: [
                "End the \(.applicationName) stream",
            ],
            shortTitle: LocalizedStringResource("apple_shortcut_end_short", defaultValue: "End Stream"), systemImageName: "stop.fill")
    }
}
#endif
#endif
