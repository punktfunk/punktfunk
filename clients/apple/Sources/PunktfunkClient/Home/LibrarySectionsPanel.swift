// The Library's Customize panel (design/apple-touch-ui-overhaul.md §2.5): every section with a
// switch, in the order the tab draws them. It writes `punktfunk.librarySections`, which the tab
// reads; a switched-off section stays listed. A sheet on the iPhone and the TV (a TV's rows move
// from their context menu), a plain popover on the Mac, where each row carries a drag grip.

import PunktfunkKit
import SwiftUI

struct LibrarySectionsPanel: View {
    @AppStorage(DefaultsKey.librarySections) private var storedLayout =
        SettingDefault.librarySections
    #if DEBUG
    /// Shot harness: a layout that never touches the device's own.
    var shotLayout: String?
    #if os(tvOS)
    /// Shot harness: focus moves to the second row once the sheet is up.
    var shotMovesFocus = false
    @FocusState private var shotFocus: LibrarySection?
    #endif
    #endif
    @Environment(\.dismiss) private var dismiss

    private var note: String {
        #if os(tvOS)
        "Hold a section to move it. A section with nothing to show stays hidden until it has "
            + "something."
        #else
        "Drag to reorder. A section with nothing to show stays hidden until it has something."
        #endif
    }

    private var layout: LibrarySectionLayout {
        #if DEBUG
        if let shotLayout { return LibrarySectionLayout(stored: shotLayout) }
        #endif
        return LibrarySectionLayout(stored: storedLayout)
    }

    var body: some View {
        #if os(macOS)
        VStack(alignment: .leading, spacing: 10) {
            Text("Customize Library")
                .font(.headline)
            // A Mac list paints its own fill and insets its rows. Without both gone the rows sat in
            // a dark slab, indented past the popover's own padding.
            List { rows }
                .listStyle(.plain)
                .scrollContentBackground(.hidden)
                .contentMargins(0, for: .scrollContent)
                .padding(.horizontal, -8) // the plain list's own row indent, so rows meet the title
            Text(note)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            restoreButton
        }
        .padding(16)
        #else
        NavigationStack {
            List {
                #if os(tvOS)
                TVScreenTitle("Customize Library")
                    .listRowBackground(Color.clear)
                #endif
                Section {
                    rows
                } footer: {
                    Text(note)
                }
                Section { restoreButton }
            }
            #if os(iOS) || os(visionOS)
            .environment(\.editMode, .constant(.active))
            .navigationBarTitleDisplayMode(.inline)
            .navigationTitle("Customize Library")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
            #else
            // A TV presents this as a card: its content keeps off every edge, with room inside the
            // list for a focused row to grow.
            .safeAreaPadding(40)
            #if DEBUG
            .task {
                guard shotMovesFocus else { return }
                try? await Task.sleep(for: .seconds(1.5))
                shotFocus = layout.entries.dropFirst().first?.section
            }
            #endif
            #endif
        }
        #endif
    }

    private var rows: some View {
        ForEach(layout.entries) { entry in
            HStack {
                Toggle(isOn: isOn(entry.section)) {
                    #if os(tvOS)
                    TVRowLabel(symbol: entry.section.symbol, text: entry.section.label)
                    #else
                    Label {
                        Text(entry.section.label)
                    } icon: {
                        Image(systemName: entry.section.symbol)
                            #if os(macOS)
                            .frame(width: 18) // one icon width, so the names line up
                            #endif
                    }
                    #endif
                }
                #if DEBUG && os(tvOS)
                .focused($shotFocus, equals: entry.section)
                #endif
                #if os(macOS)
                Spacer()
                // A Mac list draws no handle of its own; the whole row drags.
                Image(systemName: "line.3.horizontal")
                    .foregroundStyle(.tertiary)
                    .help("Drag to reorder")
                #endif
            }
            #if os(macOS)
            .listRowInsets(EdgeInsets(top: 3, leading: 0, bottom: 3, trailing: 0))
            #endif
            #if os(tvOS)
            .contextMenu {
                Button("Move Up", systemImage: "arrow.up") { step(entry.section, by: -1) }
                Button("Move Down", systemImage: "arrow.down") { step(entry.section, by: 1) }
            }
            #endif
        }
        .onMove(perform: move)
    }

    private var restoreButton: some View {
        Button("Restore Default Order") { storedLayout = "" }
            .disabled(storedLayout.isEmpty)
    }

    private func isOn(_ section: LibrarySection) -> Binding<Bool> {
        Binding(
            get: { layout.entries.first { $0.section == section }?.isOn ?? true },
            set: { on in
                var next = layout
                if let at = next.entries.firstIndex(where: { $0.section == section }) {
                    next.entries[at].isOn = on
                }
                storedLayout = next.stored
            })
    }

    private func move(from source: IndexSet, to destination: Int) {
        var next = layout
        next.entries.move(fromOffsets: source, toOffset: destination)
        storedLayout = next.stored
    }

    #if os(tvOS)
    /// A row's icon column and name: a TV Label sets the name against its icon. Black while the
    /// row has focus, whatever ink it inherits, so it never sits white on the white platter.
    private struct TVRowLabel: View {
        let symbol: String
        let text: String
        @Environment(\.isFocused) private var focused

        var body: some View {
            HStack(spacing: 20) {
                Image(systemName: symbol)
                    .frame(width: 44)
                Text(text)
            }
            .foregroundStyle(focused ? AnyShapeStyle(Color.black) : AnyShapeStyle(.primary))
        }
    }

    /// One place up or down: a remote's move, since it cannot drag.
    private func step(_ section: LibrarySection, by delta: Int) {
        var next = layout
        guard let at = next.entries.firstIndex(where: { $0.section == section }),
              next.entries.indices.contains(at + delta) else { return }
        next.entries.swapAt(at, at + delta)
        storedLayout = next.stored
    }
    #endif
}
