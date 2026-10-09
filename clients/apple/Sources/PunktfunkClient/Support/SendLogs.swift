// "Send logs to host" — the one action behind the host card's menu item and the gamepad options
// row. Posts `ClientLogRing` to the PAIRED host (`LibraryClient.sendLogs`), where the web
// console's Logs page shows it next to the host's own log. The Apple port of the Gaming Mode
// console's `ConsoleCmd::SendLogs` (clients/session/src/console.rs), same wording on success.

import Foundation
import PunktfunkKit

private let log = ClientLog(category: "logs")

enum SendLogs {
    /// Upload this device's recent log to `host`, a paired one only: the bundle carries the
    /// device's diagnostics. Never throws: the caller shows `message` either way, and the outcome
    /// is itself the last line of the NEXT bundle.
    static func toHost(_ host: StoredHost) async -> (ok: Bool, message: String) {
        do {
            let id = try await LibraryClient.sendLogs(to: MgmtTarget.make(host: host).get())
            log.info("client logs uploaded to \(host.displayName, privacy: .public) id=\(id, privacy: .public)")
            return (true, "Logs sent to \(host.displayName) — download them from its web console's "
                + "Logs page.")
        } catch let missing as MgmtTargetMissing {
            return (false, missing.sentence)
        } catch {
            let why = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            log.warning("client log upload to \(host.displayName, privacy: .public) failed: \(why, privacy: .public)")
            return (false, "Couldn't send logs — \(why)")
        }
    }
}
