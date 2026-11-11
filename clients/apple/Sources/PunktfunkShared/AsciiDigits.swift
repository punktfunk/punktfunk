// Digits a CJK keyboard types. Its number row writes full-width digits (`０`..`９`) by default,
// and a PIN or a port the host reads has to be ASCII.

import Foundation

extension Character {
    /// This character as an ASCII digit, when it is a decimal digit of any script. `一` and `②`
    /// have a number value too but are not digits, so they stay out.
    fileprivate var asciiDigit: Character? {
        guard let scalar = unicodeScalars.first, unicodeScalars.count == 1,
              scalar.properties.numericType == .decimal,
              let value = wholeNumberValue
        else { return nil }
        return Character(String(value))
    }
}

extension String {
    /// Each decimal digit as its ASCII digit, every other character as it was. For a field that
    /// still has to refuse what is not a number.
    public var withASCIIDigits: String { String(map { $0.asciiDigit ?? $0 }) }

    /// The decimal digits as ASCII, every other character dropped. For a PIN.
    public var asciiDigits: String { String(compactMap(\.asciiDigit)) }
}
