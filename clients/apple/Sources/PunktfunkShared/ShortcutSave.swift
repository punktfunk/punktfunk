// What the Shortcuts page's Save does with an edit: store it, ask to replace the binding with the
// same trigger, or stay off with a reason. `ShortcutSaveTests` pins it.

import Foundation

extension Shortcuts {
    public enum SaveDecision: Equatable, Sendable {
        case ok
        /// The same trigger is bound at this index of `without(editing)`: ask, then replace it.
        case replace(Int)
        /// Save stays off; the reason is never `.identical`.
        case blocked(Conflict)
    }

    /// The bindings without the one at `index`, which an edit is checked against; all of them
    /// for a new binding.
    public func without(_ index: Int?) -> Shortcuts {
        var s = self
        if let index { s.bindings.remove(at: index) }
        return s
    }

    /// Whether `trigger`, for `action`, may be saved as the binding at `index` (nil: a new one).
    public func saveDecision(_ trigger: Trigger, action: Action, editing index: Int?) -> SaveDecision {
        switch without(index).validate(Binding(trigger: trigger, action: action)) {
        case nil: return .ok
        case .identical(let i): return .replace(i)
        case let conflict?: return .blocked(conflict)
        }
    }

    /// The bindings with `binding` stored at `index` (nil: appended), less the one at
    /// `replacing`, an index into `without(index)`.
    public func applying(binding: Binding, at index: Int?, replacing: Int?) -> Shortcuts {
        var s = self
        if let index { s.bindings[index] = binding } else { s.bindings.append(binding) }
        if let r = replacing { s.bindings.remove(at: index.map { r >= $0 ? r + 1 : r } ?? r) }
        return s
    }
}
