// Home-Screen / Lock-Screen quick-launch widget (kind "PunktfunkHosts"). Reads the saved-host
// store from the shared App-Group suite, sorts most-recent-first, and deep-links each host into a
// session via `punktfunk://connect/<uuid>` — the app's onOpenURL routes it through the normal
// connect path (trust policy / WoL / approval all apply).
//
// No reachability probing (a UDP check has no place in a timeline build; WoL handles offline
// hosts on tap). The timeline steps the "2 hr. ago" line over an hour of entries, then reloads;
// the app also pushes reloads on store changes (HostStore → WidgetCenter.reloadTimelines).

import SwiftUI
import WidgetKit

import PunktfunkShared

// MARK: - Timeline

struct HostsEntry: TimelineEntry {
    let date: Date
    let hosts: [StoredHost]
}

struct HostsProvider: TimelineProvider {
    func placeholder(in context: Context) -> HostsEntry {
        HostsEntry(date: .now, hosts: [])
    }

    func getSnapshot(in context: Context, completion: @escaping (HostsEntry) -> Void) {
        completion(HostsEntry(date: .now, hosts: Self.loadHosts()))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<HostsEntry>) -> Void) {
        // Same hosts at growing gaps: each entry's date re-words "2 hr. ago", finely while minutes
        // matter. The app also reloads when the store changes (a new host, a fresh connect).
        let now = Date.now
        let hosts = Self.loadHosts()
        let entries = Self.refreshMinutes.map {
            HostsEntry(date: now.addingTimeInterval(Double($0) * 60), hosts: hosts)
        }
        completion(Timeline(entries: entries, policy: .after(now.addingTimeInterval(3600))))
    }

    /// Minutes past the reload at which the relative times are worded again.
    static let refreshMinutes = [0, 1, 2, 5, 10, 15, 30, 45, 60]

    /// The shared-suite host JSON, most-recent first — see `StoredHost.loadAll`.
    static func loadHosts() -> [StoredHost] { StoredHost.loadAll(recentFirst: true) }
}

// MARK: - Widget

struct HostsWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: WidgetKind.hosts, provider: HostsProvider()) { entry in
            HostsWidgetView(entry: entry)
                .containerBackground(.fill.tertiary, for: .widget)
        }
        .configurationDisplayName(L10n.appleWidgetHostsName)
        .description(L10n.appleWidgetHostsDesc)
        .supportedFamilies([
            .systemSmall, .systemMedium, .accessoryCircular, .accessoryRectangular,
        ])
    }
}

// MARK: - Views

struct HostsWidgetView: View {
    @Environment(\.widgetFamily) private var family
    let entry: HostsEntry

    var body: some View {
        switch family {
        case .systemMedium:
            MediumHostsView(hosts: entry.hosts, now: entry.date)
        case .accessoryCircular:
            CircularHostView(host: entry.hosts.first)
        case .accessoryRectangular:
            RectangularHostView(host: entry.hosts.first)
        default: // systemSmall + fallback
            SmallHostView(host: entry.hosts.first, now: entry.date)
        }
    }
}

/// "2 hr. ago" in the system language, counted to the entry's own date.
private func sinceLabel(_ date: Date, now: Date) -> String {
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .short
    return formatter.localizedString(for: date, relativeTo: now)
}

/// Deep link that connects to a stored host.
private func connectURL(_ host: StoredHost) -> URL {
    DeepLink.connect(host: host.id, launchID: nil).url
}

private struct SmallHostView: View {
    let host: StoredHost?
    let now: Date
    var body: some View {
        if let host {
            VStack(alignment: .leading, spacing: 6) {
                Image(systemName: "play.tv.fill")
                    .font(.title2)
                    .foregroundStyle(Color.brand)
                Spacer(minLength: 0)
                Text(host.displayName)
                    .font(.headline)
                    .lineLimit(2)
                if let last = host.lastConnected {
                    Text(sinceLabel(last, now: now))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .widgetURL(connectURL(host))
        } else {
            EmptyHostView()
        }
    }
}

private struct MediumHostsView: View {
    let hosts: [StoredHost]
    let now: Date
    var body: some View {
        if hosts.isEmpty {
            EmptyHostView()
        } else {
            VStack(alignment: .leading, spacing: 8) {
                Text("Punktfunk")
                    .font(.caption).bold()
                    .foregroundStyle(Color.brand)
                ForEach(hosts.prefix(4)) { host in
                    Link(destination: connectURL(host)) {
                        HStack {
                            Image(systemName: "play.tv.fill")
                                .foregroundStyle(Color.brand)
                            Text(host.displayName)
                                .font(.subheadline)
                                .lineLimit(1)
                            Spacer()
                            if let last = host.lastConnected {
                                Text(sinceLabel(last, now: now))
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
                Spacer(minLength: 0)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
    }
}

private struct CircularHostView: View {
    let host: StoredHost?
    var body: some View {
        ZStack {
            AccessoryWidgetBackground()
            Image(systemName: "play.tv.fill")
        }
        .widgetURL(host.map(connectURL))
    }
}

private struct RectangularHostView: View {
    let host: StoredHost?
    var body: some View {
        HStack {
            Image(systemName: "play.tv.fill")
            Text(host?.displayName ?? "Punktfunk")
                .lineLimit(1)
        }
        .widgetURL(host.map(connectURL))
    }
}

private struct EmptyHostView: View {
    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "play.tv")
                .font(.title2)
                .foregroundStyle(.secondary)
            Text(L10n.appleWidgetHostsEmpty)
                .font(.caption)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Previews (Xcode canvas)
//
// Select the PunktfunkWidgetsExtension scheme and open the canvas (⌥⌘↩). The widget
// `#Preview(as:widget:timeline:)` form feeds sample entries directly — the App-Group store is
// never read, so the canvas works without a paired device or saved hosts. The small preview's
// second entry shows the empty state one timeline click away.

private let previewHosts: [StoredHost] = [
    StoredHost(
        name: "Studio", address: "192.168.1.20",
        lastConnected: .now.addingTimeInterval(-40 * 60)),
    StoredHost(
        name: "Living Room", address: "192.168.1.30",
        lastConnected: .now.addingTimeInterval(-26 * 3600)),
    StoredHost(
        name: "Workstation", address: "10.0.0.5",
        lastConnected: .now.addingTimeInterval(-6 * 86400)),
]

#Preview("Small", as: .systemSmall) {
    HostsWidget()
} timeline: {
    HostsEntry(date: .now, hosts: previewHosts)
    HostsEntry(date: .now, hosts: [])
}

#Preview("Medium", as: .systemMedium) {
    HostsWidget()
} timeline: {
    HostsEntry(date: .now, hosts: previewHosts)
}

#Preview("Lock Screen circular", as: .accessoryCircular) {
    HostsWidget()
} timeline: {
    HostsEntry(date: .now, hosts: previewHosts)
}

#Preview("Lock Screen rectangular", as: .accessoryRectangular) {
    HostsWidget()
} timeline: {
    HostsEntry(date: .now, hosts: previewHosts)
}
