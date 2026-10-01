// The About page — the app's identity card, on the shape every Mac user already knows from an
// About panel: the icon, the name, the version, and then the ways out (documentation, community,
// source, licenses). It replaced a bare header over an 885 KB wall of license text, which answered
// the one question nobody opens About to ask.
//
// The license wall itself is still `AcknowledgementsView`; About opens it as a sheet — pushed
// only on tvOS, where this page really is inside a navigation stack. The iPad settings detail
// column deliberately isn't one, so a push there had no way back.
//
// Wording note: this page says the SOURCE is open, never that the app is "free software". The
// term means liberty, but it is read as price — and this app is sold on the App Store, so to the
// person most likely to read this line it would be a claim about what they just paid for.

import PunktfunkKit
import SwiftUI

struct AboutView: View {
    /// Where the product actually lives. Kept here rather than scattered through the view so the
    /// three of them can be checked against the README in one glance.
    private enum Destination {
        static let docs = URL(string: "https://docs.punktfunk.unom.io")!
        static let community = URL(string: "https://discord.gg/wzEGg9y45z")!
        static let source = URL(string: "https://git.unom.io/unom/punktfunk")!
    }

    private static let tagline =
        "Low-latency desktop and game streaming with first-class Linux and Windows hosts."

    #if !os(tvOS)
    @State private var showAcknowledgements = false
    /// The in-session controls. They used to announce themselves in a 6-second banner at the start
    /// of every stream; that banner is gone, so this page is where they live now — including for
    /// touch users on a Mac, who saw it too.
    @State private var showShortcuts = false
    #endif

    var body: some View {
        #if os(tvOS)
        tvBody
        #else
        Form {
            Section {
                identity
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
                    // The header is a statement, not a control: no separator, no row insets
                    // fighting the centring.
                    .listRowInsets(EdgeInsets())
                    .listRowBackground(Color.clear)
            }
            Section {
                shortcutsRow
            }
            Section {
                linkRow("Documentation", systemImage: "book", url: Destination.docs)
                linkRow("Community", systemImage: "bubble.left.and.bubble.right",
                        url: Destination.community)
                linkRow("Source Code", systemImage: "chevron.left.forwardslash.chevron.right",
                        url: Destination.source)
            }
            Section {
                acknowledgementsRow
            } footer: {
                Text("Punktfunk's source is open under MIT or Apache-2.0.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        // A SHEET, not a push — on iPad the settings detail column is deliberately not a
        // NavigationStack (an inner one doubles the title bar), so a NavigationLink from here
        // pushed into a context with no back button and stranded the licenses on screen.
        // A sheet for the same reason Acknowledgements is one — see that modifier's note on the
        // iPad detail column not being a NavigationStack.
        .sheet(isPresented: $showShortcuts) {
            NavigationStack {
                ShortcutsView(micAvailable: ShortcutsCatalog.micPlausible)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { showShortcuts = false }
                        }
                    }
            }
            #if os(macOS)
            .frame(width: 560, height: 460)
            #endif
        }
        .sheet(isPresented: $showAcknowledgements) {
            NavigationStack {
                AcknowledgementsView()
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { showAcknowledgements = false }
                        }
                    }
            }
            #if os(macOS)
            .frame(width: 640, height: 560)
            #endif
        }
        #endif
    }

    // MARK: - Identity

    /// Icon, name, version, tagline — centred, in that order, at the sizes an About panel uses.
    private var identity: some View {
        VStack(spacing: 10) {
            AppIconView(side: Self.iconSide)
            Text("Punktfunk")
                .font(.geist(Self.nameFont, .bold, relativeTo: .largeTitle))
            Text(Self.versionLine)
                .font(.geist(Self.versionFont, relativeTo: .callout))
                .monospacedDigit()
                .foregroundStyle(.secondary)
                .modifier(SelectableIfPossible())
            Text(Self.tagline)
                .font(.geist(Self.taglineFont, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 320)
                .padding(.top, 2)
        }
    }

    /// "Version 0.21.0 (1)" — the build number only when it says something the version doesn't.
    /// A bug report is worth more with it, which is why it's selectable above.
    private static var versionLine: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "—"
        let build = info?["CFBundleVersion"] as? String
        guard let build, !build.isEmpty, build != short else { return "Version \(short)" }
        return "Version \(short) (\(build))"
    }

    // MARK: - Rows

    #if !os(tvOS)
    private func linkRow(_ title: String, systemImage: String, url: URL) -> some View {
        Link(destination: url) {
            HStack {
                Label(title, systemImage: systemImage)
                Spacer(minLength: 8)
                Image(systemName: "arrow.up.right")
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    // The row already announces itself as a link.
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        #if os(macOS)
        // A Link in a grouped Form renders as raw blue body text on macOS; the row is the button.
        .buttonStyle(.plain)
        #endif
        .foregroundStyle(.primary)
    }

    private var shortcutsRow: some View {
        Button {
            showShortcuts = true
        } label: {
            HStack {
                Label("Shortcuts", systemImage: "command")
                Spacer(minLength: 8)
                Image(systemName: "chevron.right")
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(.primary)
    }

    private var acknowledgementsRow: some View {
        Button {
            showAcknowledgements = true
        } label: {
            HStack {
                Label("Acknowledgements", systemImage: "text.document")
                Spacer(minLength: 8)
                Image(systemName: "chevron.right")
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(.primary)
    }
    #endif

    // MARK: - tvOS

    #if os(tvOS)
    /// tvOS has no browser and no `openURL`, so the addresses are text to read off the screen
    /// rather than links to nowhere. Acknowledgements still pushes — that one is in the app.
    private var tvBody: some View {
        ScrollView {
            VStack(spacing: 28) {
                identity
                VStack(spacing: 10) {
                    tvAddress("Documentation", Destination.docs)
                    tvAddress("Community", Destination.community)
                    tvAddress("Source code", Destination.source)
                }
                // Both push here: this page really is inside a navigation stack on tvOS, which is
                // the case the sheets above exist to work around elsewhere.
                NavigationLink("Shortcuts") {
                    ShortcutsView(micAvailable: false) // tvOS has no app-accessible mic
                }
                NavigationLink("Acknowledgements") { AcknowledgementsView() }
                Text("Punktfunk's source is open under MIT or Apache-2.0.")
                    .font(.geist(20, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: 1000)
            .frame(maxWidth: .infinity)
            .padding(60)
        }
    }

    private func tvAddress(_ title: String, _ url: URL) -> some View {
        VStack(spacing: 2) {
            Text(title)
                .font(.geist(22, .semibold, relativeTo: .headline))
            Text(url.absoluteString.replacingOccurrences(of: "https://", with: ""))
                .font(.geist(20, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }
    #endif

    // Panel-sized on the Mac, a touch smaller in hand, and read-from-the-couch on TV.
    #if os(tvOS)
    private static let iconSide: CGFloat = 160
    private static let nameFont: CGFloat = 44
    private static let versionFont: CGFloat = 24
    private static let taglineFont: CGFloat = 22
    #else
    private static let iconSide: CGFloat = 96
    private static let nameFont: CGFloat = 28
    private static let versionFont: CGFloat = 14
    private static let taglineFont: CGFloat = 13
    #endif
}

/// The app's own icon, at whatever size About asks for.
///
/// Every platform hides it somewhere different, and one of them can't hand it over at all: macOS
/// has the running app's icon; iOS keeps the primary icon's file names in `CFBundleIcons`; tvOS
/// and visionOS icons are layered assets with no single image to load, and an unbundled
/// `swift run` build has no icon in the first place. So there is a drawn fallback, built from
/// the same brand gradient and Geist monogram as the host cards — where the real icon is
/// unavailable this reads as a mark, not as a missing image.
struct AppIconView: View {
    let side: CGFloat

    var body: some View {
        Group {
            if let icon = Self.bundleIcon {
                // Masked where the OS masks at draw time, the iOS and TV home screens. macOS bakes
                // its shape and margins into the image, and a clip would cut into them.
                if icon.needsMask {
                    icon.image
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .clipShape(RoundedRectangle(
                            cornerRadius: side * Self.cornerRatio, style: .continuous))
                } else {
                    icon.image
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                }
            } else {
                monogram
            }
        }
        // tvOS's icon is a 400×240 rectangle, not a squircle — framing it square would letterbox
        // it inside a box two thirds empty. `side` means HEIGHT there, and the width follows the
        // real 5:3 art. A MAX frame rather than a fixed one: with a fixed width the image cannot
        // shrink when its row is tight, so it overflows and is clipped by whatever is above it
        // instead — `.fit` inside a max frame gives back the whole icon, just smaller.
        #if os(tvOS)
        .frame(maxWidth: side * (400.0 / 240.0), maxHeight: side)
        #else
        .frame(width: side, height: side)
        #endif
        .accessibilityHidden(true) // the app's name is the next line
    }

    /// The mask's corner radius as a fraction of the icon's height: the iOS squircle's ~22.37%,
    /// and the 24 pt a TV home screen gives its 150 pt tall icons.
    #if os(tvOS)
    private static let cornerRatio: CGFloat = 0.16
    #elseif os(visionOS)
    private static let cornerRatio: CGFloat = 0.5 // a circle, as the home view draws it
    #else
    private static let cornerRatio: CGFloat = 0.2237
    #endif

    private var monogram: some View {
        let shape = RoundedRectangle(cornerRadius: side * 0.22, style: .continuous)
        return shape
            .fill(LinearGradient(
                colors: [Color.brand, Color.brand.opacity(0.7)],
                startPoint: .top, endPoint: .bottom))
            .overlay {
                Text("P")
                    .font(.geistFixed(side * 0.5, .bold))
                    .foregroundStyle(.white)
            }
    }

    /// `needsMask` = the platform hands over a bare square that the OS normally rounds for us.
    private static var bundleIcon: (image: Image, needsMask: Bool)? {
        #if os(macOS)
        return (Image(nsImage: NSApplication.shared.applicationIconImage), false)
        #elseif os(iOS)
        // The last entry is the largest — `CFBundleIconFiles` is ordered small to large.
        guard let icons = Bundle.main.infoDictionary?["CFBundleIcons"] as? [String: Any],
              let primary = icons["CFBundlePrimaryIcon"] as? [String: Any],
              let files = primary["CFBundleIconFiles"] as? [String],
              let name = files.last,
              let image = UIImage(named: name)
        else { return nil }
        return (Image(uiImage: image), true)
        #else
        // tvOS and visionOS icons are layered stacks with no single image to load. `AboutAppIcon`
        // is the stack flattened, unmasked as the stack is (the home screen masks it at draw
        // time): tvOS's by scripts/render-tvos-icon.swift, visionOS's from its three layers.
        guard let image = UIImage(named: "AboutAppIcon") else { return nil }
        return (Image(uiImage: image), true)
        #endif
    }
}

/// `textSelection(.enabled)` doesn't exist on tvOS, and a version string is the one thing here
/// worth copying into a bug report.
private struct SelectableIfPossible: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS)
        content
        #else
        content.textSelection(.enabled)
        #endif
    }
}
