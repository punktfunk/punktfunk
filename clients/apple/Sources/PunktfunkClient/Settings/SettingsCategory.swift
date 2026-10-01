// SettingsView's navigation and presentation helpers: the settings categories (the iPhone, iPad
// and Apple TV sidebars) and the iPad sheet sizing.

import SwiftUI

#if os(iOS) || os(visionOS) || os(tvOS)
/// The settings groups, mirroring the macOS preference tabs. On iPad each is a sidebar row that
/// drives the detail pane; on iPhone the same list collapses to pushed sub-pages; on a TV the rows
/// head the settings screen and focus picks one. Internal (not private) so the screenshot harness
/// can open SettingsView on a specific category.
enum SettingsCategory: String, CaseIterable, Identifiable {
    // General = session/app behavior, Display = everything about the picture (resolution,
    // quality, presentation, host output), Input = touch/keyboard/mouse, which a TV has none of.
    case general, display
    #if os(iOS) || os(visionOS)
    case input
    #endif
    case audio, controllers
    #if os(tvOS)
    case quickActions
    #endif
    case about

    var id: Self { self }

    var title: String {
        switch self {
        case .general: return "General"
        case .display: return "Display"
        #if os(iOS) || os(visionOS)
        case .input: return "Input"
        #endif
        case .audio: return "Audio"
        case .controllers: return "Controllers"
        #if os(tvOS)
        case .quickActions: return "Quick Actions"
        #endif
        case .about: return "About"
        }
    }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .display: return "display"
        #if os(iOS) || os(visionOS)
        case .input: return "keyboard"
        #endif
        case .audio: return "speaker.wave.2"
        case .controllers: return "gamecontroller"
        #if os(tvOS)
        case .quickActions: return "dial.medium"
        #endif
        case .about: return "info.circle"
        }
    }
}
#endif

#if os(iOS) || os(visionOS)
extension View {
    /// Present the settings sheet large on iPad and Vision Pro so the NavigationSplitView has room
    /// for its sidebar + detail — a default form sheet is too narrow and the split view would
    /// collapse to the iPhone push list. No-op on iPhone (the standard sheet is already right) and
    /// on iOS 17 (no `presentationSizing` — it falls back to the default sheet, which still
    /// degrades cleanly to the push list).
    @ViewBuilder
    func settingsSheetSizing() -> some View {
        let idiom = UIDevice.current.userInterfaceIdiom
        if idiom == .pad || idiom == .vision, #available(iOS 18, *) {
            presentationSizing(.page)
        } else {
            self
        }
    }
}
#endif
