import PunktfunkKit
import SwiftUI

/// Open-source acknowledgements: Punktfunk's own license (MIT OR Apache-2.0) followed by the
/// third-party software notices. Used as a pushed view on iOS/tvOS and a preferences tab on macOS.
struct AcknowledgementsView: View {
    private var version: String? {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
    }

    // TV-legible sizes for the explicitly-sized text; the in-hand sizes elsewhere. (The license
    // walls use relative system styles, which already scale per platform.)
    #if os(tvOS)
    private static let titleFont: CGFloat = 36
    private static let headlineFont: CGFloat = 26
    private static let captionFont: CGFloat = 20
    #else
    private static let titleFont: CGFloat = 22
    private static let headlineFont: CGFloat = 17
    private static let captionFont: CGFloat = 12
    #endif

    var body: some View {
        ScrollView {
            // Top-level LazyVStack so the third-party-notices chunks (Licenses.thirdPartyNoticesChunks,
            // ~885 KB total) load lazily as they scroll into view — a single Text that large overshoots
            // the text-rendering height limit (blank below the limit + very slow). spacing 0 keeps the
            // notice chunks visually continuous; the header block carries its own spacing + bottom pad.
            LazyVStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 18) {
                    // The top's own focus stop: moving up from the first license chunk scrolls
                    // back here instead of leaving the title off screen under the tab bar.
                    VStack(alignment: .leading, spacing: 18) {
                        #if os(tvOS)
                        TVScreenTitle(String(localized: L10n.aboutLicensesTitle))
                        #endif
                        Text("Punktfunk")
                            .font(.geist(Self.titleFont, .bold, relativeTo: .title2))
                        if let version {
                            Text(L10n.aboutVersionLine(version: version))
                                .font(.geist(Self.captionFont, relativeTo: .caption))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .modifier(TVFocusable())
                    LicenseWall(text: Licenses.appLicense)
                        .font(.caption.monospaced())

                    Divider()

                    Text(L10n.aboutBundledFont)
                        .font(.geist(Self.headlineFont, .semibold, relativeTo: .headline))
                    Text(L10n.aboutGeistLicense)
                        .font(.geist(Self.captionFont, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                    if !Licenses.fontLicense.isEmpty {
                        LicenseWall(text: Licenses.fontLicense)
                            .font(.caption2.monospaced())
                    }

                    Divider()

                    Text(L10n.appleAboutSwiftPackages)
                        .font(.geist(Self.headlineFont, .semibold, relativeTo: .headline))
                    Text(Licenses.swiftPackages)
                        .font(.geist(Self.captionFont, relativeTo: .caption))
                        .foregroundStyle(.secondary)

                    Divider()

                    Text(L10n.aboutThirdPartyHeading)
                        .font(.geist(Self.headlineFont, .semibold, relativeTo: .headline))
                    Text(Licenses.thirdPartyIntro)
                    .font(.geist(Self.captionFont, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.bottom, 18)

                ForEach(Licenses.thirdPartyNoticesChunks.indices, id: \.self) { i in
                    Text(Licenses.thirdPartyNoticesChunks[i])
                        .font(.caption2.monospaced())
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .modifier(SelectableText())
                        .modifier(TVFocusable())
                }
            }
            .frame(maxWidth: 900, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding()
            #if os(tvOS)
                .padding(40)
            #endif
        }
        #if !os(tvOS)
        .navigationTitle(L10n.aboutLicensesTitle)
        #endif
    }
}

/// `textSelection(.enabled)` is unavailable on tvOS, so apply it only where it exists.
private struct SelectableText: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS)
            content
        #else
            content.textSelection(.enabled)
        #endif
    }
}

/// Focus IS scrolling on tvOS: with nothing focusable in this pushed screen the license wall
/// couldn't move at all, and a Menu press had nothing inside the NavigationStack to route
/// through — it suspended the whole app instead of popping. Plain (non-interactive) focusability
/// on every license/notice chunk fixes both; a chunk is sized to about two thirds of a screen
/// (see Licenses.chunked), so each focus step reads as a page turn. The chunks must be SMALL
/// focus stops all the way down — one tall focusable block would strand focus at its top and the
/// next stop could sit past the LazyVStack's instantiation window.
struct TVFocusable: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS)
            content.focusable()
        #else
            content
        #endif
    }
}

/// One license wall: a single selectable Text on touch/desktop; on tvOS, focus-page-sized
/// chunks (see TVFocusable). The caller's `.font` cascades into either form.
private struct LicenseWall: View {
    let text: String

    var body: some View {
        #if os(tvOS)
        let chunks = Licenses.chunked(text)
        ForEach(chunks.indices, id: \.self) { i in
            Text(chunks[i])
                .frame(maxWidth: .infinity, alignment: .leading)
                .modifier(TVFocusable())
        }
        #else
        Text(text)
            .modifier(SelectableText())
        #endif
    }
}
