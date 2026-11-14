// The Rust core's log lines, routed into `ClientLog` (os_log + the send-to-host ring).
//
// The core logs through `tracing`; `punktfunk_set_log_callback` hands each line to the C callback
// below, which files it under a `core.<crate>` category. Without it the transport warnings, quinn
// connection events and rustls handshake notes never reach a log bundle.

import Foundation
import PunktfunkCore

public enum CoreLog {
    /// Install once at launch. Levels above `maxLevel` (1 = error … 5 = trace) are not even
    /// formatted on the Rust side. Info is the ceiling on purpose: quinn's debug/trace is
    /// per-packet and would churn the ring — the same gate the session client's ring applies.
    /// `PUNKTFUNK_CORE_LOG_LEVEL=4` raises it for a debugging session.
    public static func install() {
        let level = UInt8(ProcessInfo.processInfo.environment["PUNKTFUNK_CORE_LOG_LEVEL"] ?? "") ?? 3
        let status = punktfunk_set_log_callback(level, { level, target, message, _ in
            // Called from whichever Rust thread logged — copy both C strings out before anything
            // else, then hand off to ClientLog, which is cheap (one lock + os_log) and thread-safe.
            let target = target.map { String(cString: $0) } ?? "core"
            let message = message.map { String(cString: $0) } ?? ""
            // The crate (first path segment) becomes the category; the full target stays in the
            // line, so `quinn::connection` reads as `core.quinn quinn::connection …`.
            let crate_ = target.split(separator: ":", maxSplits: 1).first.map(String.init) ?? target
            let log = ClientLog(category: "core.\(crate_)")
            switch level {
            case 1: log.error("\(target, privacy: .public) \(message, privacy: .public)")
            case 2: log.warning("\(target, privacy: .public) \(message, privacy: .public)")
            case 3: log.info("\(target, privacy: .public) \(message, privacy: .public)")
            default: log.debug("\(target, privacy: .public) \(message, privacy: .public)")
            }
        }, nil)
        if status != PUNKTFUNK_STATUS_OK.rawValue {
            ClientLog(category: "core").warning("core log callback not installed: status \(status)")
        }
    }
}
