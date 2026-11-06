// Configurable Home-Screen / Lock-Screen library widget (kind "PunktfunkLibrary"). The user picks
// a saved host in the widget's configuration (long-press → Edit Widget — the picker is
// `HostEntity`'s query over the shared App-Group store, running in this extension process); a tap
// deep-links into that host's game library via `punktfunk://browse/<uuid>` — the app's onOpenURL
// routes it to the same library presentation every internal surface drives. No session starts
// until a title is picked there.
//
// Unconfigured, it follows the most recently connected host (the same order the hosts widget
// leads with). A configured host that no longer exists shows the empty state rather than silently
// following a different host — a widget that says "Studio" must never open someone else's library.
//
// Timeline is a single `.never` entry — the app pushes reloads on store changes (HostStore →
// WidgetCenter.reloadTimelines), exactly like the hosts widget.

import AppIntents
import SwiftUI
import WidgetKit

import PunktfunkShared

// MARK: - Configuration intent

/// The widget's per-instance configuration. Executes in the EXTENSION process — which is why
/// `HostEntity` and its query live in PunktfunkShared, not the app.
struct LibraryWidgetConfigIntent: WidgetConfigurationIntent {
    static let title: LocalizedStringResource = LocalizedStringResource(
        "apple_widget_library_intent_title", defaultValue: "Choose Host")
    static let description = IntentDescription(
        LocalizedStringResource(
            "apple_widget_library_intent_desc",
            defaultValue: "Pick whose game library this widget opens."))

    @Parameter(
        title: LocalizedStringResource("apple_intent_param_host", defaultValue: "Host"),
        description: LocalizedStringResource(
            "apple_widget_library_param_host_desc",
            defaultValue: "Leave empty to follow your most recent host."))
    var host: HostEntity?
}

// MARK: - Timeline

struct LibraryEntry: TimelineEntry {
    let date: Date
    /// The resolved target: the configured host if it still exists, the most recent one when
    /// unconfigured, nil when there's nothing to open (empty store, or a removed configured host).
    let host: StoredHost?
}

struct LibraryProvider: AppIntentTimelineProvider {
    func placeholder(in context: Context) -> LibraryEntry {
        LibraryEntry(date: .now, host: nil)
    }

    func snapshot(for configuration: LibraryWidgetConfigIntent, in context: Context) async
        -> LibraryEntry {
        LibraryEntry(date: .now, host: Self.resolve(configuration.host))
    }

    func timeline(for configuration: LibraryWidgetConfigIntent, in context: Context) async
        -> Timeline<LibraryEntry> {
        // Single entry, never auto-refresh: the app reloads this timeline on every store change.
        Timeline(entries: [LibraryEntry(date: .now, host: Self.resolve(configuration.host))],
                 policy: .never)
    }

    /// The configured host by id — nil (NOT a fallback) when it's gone; most-recent when nothing
    /// was configured.
    static func resolve(_ configured: HostEntity?) -> StoredHost? {
        let hosts = HostsProvider.loadHosts() // shared-suite JSON, most-recent first
        guard let configured else { return hosts.first }
        return hosts.first { $0.id == configured.id }
    }
}

// MARK: - Widget

struct LibraryWidget: Widget {
    var body: some WidgetConfiguration {
        AppIntentConfiguration(
            kind: WidgetKind.library, intent: LibraryWidgetConfigIntent.self,
            provider: LibraryProvider()
        ) { entry in
            LibraryWidgetView(entry: entry)
                .containerBackground(.fill.tertiary, for: .widget)
        }
        .configurationDisplayName(L10n.appleWidgetLibraryName)
        .description(L10n.appleWidgetLibraryDesc)
        .supportedFamilies([.systemSmall, .accessoryCircular, .accessoryRectangular])
    }
}

// MARK: - Views

/// Deep link that opens a stored host's library.
private func browseURL(_ host: StoredHost) -> URL {
    DeepLink.browse(host: host.id).url
}

struct LibraryWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: LibraryEntry

    var body: some View {
        switch family {
        case .accessoryCircular:
            CircularLibraryView(host: entry.host)
        case .accessoryRectangular:
            RectangularLibraryView(host: entry.host)
        default: // systemSmall + fallback
            SmallLibraryView(host: entry.host)
        }
    }
}

private struct SmallLibraryView: View {
    let host: StoredHost?
    var body: some View {
        if let host {
            VStack(alignment: .leading, spacing: 6) {
                Image(systemName: "square.grid.2x2.fill")
                    .font(.title2)
                    .foregroundStyle(Color.brand)
                Spacer(minLength: 0)
                Text(host.displayName)
                    .font(.headline)
                    .lineLimit(2)
                Text(L10n.appleWidgetLibraryName)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .widgetURL(browseURL(host))
        } else {
            EmptyLibraryView()
        }
    }
}

private struct CircularLibraryView: View {
    let host: StoredHost?
    var body: some View {
        ZStack {
            AccessoryWidgetBackground()
            Image(systemName: "square.grid.2x2.fill")
        }
        .widgetURL(host.map(browseURL))
    }
}

private struct RectangularLibraryView: View {
    let host: StoredHost?
    var body: some View {
        HStack {
            Image(systemName: "square.grid.2x2.fill")
            VStack(alignment: .leading) {
                Text(host?.displayName ?? "Punktfunk")
                    .lineLimit(1)
                Text(L10n.appleWidgetLibraryLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .widgetURL(host.map(browseURL))
    }
}

private struct EmptyLibraryView: View {
    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "square.grid.2x2")
                .font(.title2)
                .foregroundStyle(.secondary)
            Text(L10n.appleWidgetLibraryEmpty)
                .font(.caption)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Previews (Xcode canvas)
//
// Same pattern as the hosts widget: `#Preview(as:widget:timeline:)` feeds sample entries directly,
// so the canvas works without a paired device or saved hosts. The small preview's second entry
// shows the empty state one timeline click away.

private let previewHost = StoredHost(
    name: "Studio", address: "192.168.1.20",
    lastConnected: .now.addingTimeInterval(-40 * 60))

#Preview("Small", as: .systemSmall) {
    LibraryWidget()
} timeline: {
    LibraryEntry(date: .now, host: previewHost)
    LibraryEntry(date: .now, host: nil)
}

#Preview("Lock Screen circular", as: .accessoryCircular) {
    LibraryWidget()
} timeline: {
    LibraryEntry(date: .now, host: previewHost)
}

#Preview("Lock Screen rectangular", as: .accessoryRectangular) {
    LibraryWidget()
} timeline: {
    LibraryEntry(date: .now, host: previewHost)
}
