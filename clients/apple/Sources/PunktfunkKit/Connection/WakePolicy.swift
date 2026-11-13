// When this app sends Wake-on-LAN. The touch UI, the console (through `LibraryLoad` and
// `ConsoleJSON`) and the Wake Host intent all decide through `WakePolicy`, and every view reads
// the player's switch through `@AutoWake`. Pure, so `swift test` checks it (`WakePolicyTests`).

import PunktfunkShared
import SwiftUI

public enum WakePolicy {
    /// A wake can reach this host: its MACs are known (iOS and tvOS broadcast through the
    /// multicast entitlement in `Config/Punktfunk.entitlements`). Gates the explicit Wake Host
    /// action.
    public static func canWake(_ macs: [String]) -> Bool {
        !macs.isEmpty
    }

    /// With Auto-wake on, a connect or a library open wakes this host before it asks.
    public static func autoWakes(_ macs: [String], autoWake: Bool) -> Bool {
        autoWake && canWake(macs)
    }

    /// A connect sends Auto-wake's packet before it dials, unless the last probe reached the host.
    public static func wakesFirst(_ macs: [String], autoWake: Bool, probedOnline: Bool) -> Bool {
        autoWakes(macs, autoWake: autoWake) && !probedOnline
    }
}

/// The player's Auto-wake on connect switch (Settings → General), read live by a view.
@propertyWrapper
public struct AutoWake: DynamicProperty {
    @AppStorage(DefaultsKey.autoWake) private var value = SettingDefault.autoWake

    public init() {}

    public var wrappedValue: Bool {
        get { value }
        nonmutating set { value = newValue }
    }

    public var projectedValue: Binding<Bool> { $value }
}
