// What a CJK keyboard types into a host address. It writes full-width forms by default
// (`１９２．１６８．１．５`, `ｎｕｃ：９７７７`) and the ideographic full stop `。` for a dot.

import Foundation

extension String {
    /// A host address with each full-width ASCII form (`！`..`～`) as its ASCII character and `。`
    /// as `.`. Every other scalar stays, so an IDN host passes through. For an address field only;
    /// a name keeps what the user typed.
    public var withASCIIAddress: String {
        var mapped = String.UnicodeScalarView()
        for scalar in unicodeScalars {
            switch scalar.value {
            case 0xFF01...0xFF5E: mapped.append(Unicode.Scalar(scalar.value - 0xFEE0) ?? scalar)
            case 0x3002: mapped.append(".")
            default: mapped.append(scalar)
            }
        }
        return String(mapped)
    }
}
