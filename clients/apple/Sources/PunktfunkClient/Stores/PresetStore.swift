// The settings-preset catalog as an observable store — the app-side wrapper around
// `PresetCatalog` (design/client-settings-profiles.md §4.2), matching what `HostStore` is to
// `[StoredHost]`.
//
// The catalog lives in the App Group suite beside the saved hosts, because that is where the
// things that POINT at it live: a binding is `StoredHost.presetID` and a pin is an entry in
// `StoredHost.pinnedPresetIDs`. Nothing here is keyed by host — "Work" applied to three hosts is
// one preset, and the per-host part is only the binding (§4.1).

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
final class PresetStore: ObservableObject {
    /// One catalog for the whole app. The settings surface reaches this from a SEPARATE macOS
    /// `Settings` scene, where no parent can pass it — and two instances would mean editing a
    /// preset in Preferences while the host grid still shows the old one. Same shape as
    /// `HostStore.shared` and `GamepadManager.shared`.
    static let shared = PresetStore()

    @Published private(set) var catalog: PresetCatalog {
        didSet {
            #if DEBUG
            // Shot mode seeds this SINGLETON with mock presets to populate the host cards.
            // Saving would write them into the tester's real catalog — see HostStore.persist().
            if ScreenshotMode.isActive { return }
            #endif
            scheduleSave()
        }
    }

    /// Coalesces the write. A save encodes EVERY preset, and a continuous control in preset
    /// scope writes per drag tick. One run-loop turn is enough to collapse a drag into a single
    /// write while still landing long before the app can be killed.
    private var saveTask: Task<Void, Never>?

    private func scheduleSave() {
        saveTask?.cancel()
        saveTask = Task { [weak self] in
            await Task.yield()
            guard let self, !Task.isCancelled else { return }
            self.saveTask = nil
            self.catalog.save()
        }
    }

    /// Write now, for a caller that cannot wait for the coalesced turn (app teardown).
    func flush() {
        saveTask?.cancel()
        saveTask = nil
        #if DEBUG
        if ScreenshotMode.isActive { return }
        #endif
        catalog.save()
    }

    var presets: [StreamPreset] { catalog.presets }

    init(catalog: PresetCatalog? = nil) {
        self.catalog = catalog ?? PresetCatalog.load()
    }

    func preset(id: String?) -> StreamPreset? {
        id.flatMap { catalog.preset(id: $0) }
    }

    #if DEBUG
    /// Shot-mode seed: replace the catalog outright so a capture shows a known set of presets
    /// rather than the tester's. Safe because `didSet` suppresses the write-back in shot mode.
    func debugSet(_ presets: [StreamPreset]) {
        catalog = PresetCatalog(presets: presets)
    }
    #endif

    /// This host's default preset, dangling ids dropped — a deleted preset resolves as "Default
    /// settings", never an error (§4.4).
    func binding(for host: StoredHost) -> StreamPreset? { catalog.binding(for: host) }

    /// This host's pinned presets in card order, duplicates and dangling ids dropped.
    func pinned(for host: StoredHost) -> [StreamPreset] { catalog.pinned(for: host) }

    func nameTaken(_ name: String, except: String? = nil) -> Bool {
        catalog.nameTaken(name, except: except)
    }

    // MARK: - Catalog management (the scope menu's Rename / Duplicate / Delete)

    /// Add a preset the editor built. A blank one inherits everything — the right creation
    /// default under inherit-by-exception; a duplicate arrives carrying the source's overrides,
    /// which is what duplicating is for.
    func add(_ preset: StreamPreset) {
        catalog.presets.append(preset)
    }

    /// Insert or replace by id: the console's editor saves a preset whole.
    func put(_ preset: StreamPreset) {
        if let i = catalog.presets.firstIndex(where: { $0.id == preset.id }) {
            catalog.presets[i] = preset
        } else {
            catalog.presets.append(preset)
        }
    }

    func rename(_ id: String, to name: String) {
        guard let i = catalog.presets.firstIndex(where: { $0.id == id }) else { return }
        catalog.presets[i].name = name
    }

    func setAccent(_ id: String, to accent: String?) {
        guard let i = catalog.presets.firstIndex(where: { $0.id == id }) else { return }
        catalog.presets[i].accent = accent
    }

    /// Delete a preset. Bindings and pins pointing at it are left alone deliberately: they
    /// degrade to "Default settings" / a dropped card at read time (§6), so a delete never has to
    /// walk the host store — and a host record saved by an older build can't resurrect a stale id.
    func delete(_ id: String) {
        catalog.presets.removeAll { $0.id == id }
    }

    /// How the delete warning counts what it is about to change: hosts bound to this preset and
    /// pinned cards that will disappear.
    func usage(of id: String) -> (bound: Int, pinned: Int) {
        let hosts = Self.savedHosts()
        return (
            hosts.filter { $0.presetID == id }.count,
            hosts.filter { ($0.pinnedPresetIDs ?? []).contains(id) }.count
        )
    }

    /// Saved hosts straight from the shared store. The settings surface owns no `HostStore` — it
    /// only needs to COUNT what a delete is about to change, and reading the same App-Group blob
    /// the widget reads beats threading a store through a separate macOS Settings scene.
    static func savedHosts() -> [StoredHost] { StoredHost.loadAll(recentFirst: false) }

    // MARK: - Overrides

    /// Record an override, always by explicit write — never by comparing the new value against
    /// today's global. A value that happens to equal the global is a legitimate PIN: the preset
    /// keeps it when the global later moves, and that is the whole difference between this feature
    /// and "copy the settings" (§4.1).
    func setOverride<Value>(
        _ id: String, _ keyPath: WritableKeyPath<SettingsOverlay, Value?>, _ value: Value
    ) {
        guard let i = catalog.presets.firstIndex(where: { $0.id == id }) else { return }
        catalog.presets[i].overrides[keyPath: keyPath] = value
    }

    /// The only way back to inheriting: an explicit per-row reset. `field` is the overlay's own
    /// serialized name, with `resolution` covering the width/height/match-window tri-state.
    func clearOverride(_ id: String, field: String) {
        guard let i = catalog.presets.firstIndex(where: { $0.id == id }) else { return }
        OverlayField.clear(field, in: &catalog.presets[i].overrides)
    }
}
