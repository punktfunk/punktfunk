import Foundation

/// A sentence the host sent by the id of its message, worded in the app's language.
///
/// The host names the message beside its English: an API error's code and params, a seat line's
/// `detail_id`, a launch verdict's id, a close code's `reject_<token>`. An id this build has no
/// message for, such as a newer host's, reads as the host's own English.
/// `clients/shared/host-text-vectors.json` pins the sentences for every client.
public enum HostText {
    /// What a host may ask this app to say. Any other message is the app's own.
    private static let ids = ["apierr_", "launch_", "seatline_", "reject_"]

    /// The text a lookup returns for a key the catalog lacks.
    private static let missing = "\u{1}"

    /// The message `id` names, filled from `params`; `english` when this build has none.
    public static func word(_ id: String, params: [String: String] = [:], english: String) -> String {
        guard ids.contains(where: id.hasPrefix) else { return english }
        if let plural = plurals[id] {
            return params["count"].flatMap(Int.init).map(plural) ?? english
        }
        if let several = several[id] {
            return several(params) ?? english
        }
        let format = Bundle.module.localizedString(forKey: id, value: missing, table: nil)
        guard format != missing else { return english }
        if !format.contains("%") { return format }
        // One placeholder takes the one value. A host that sends more than this message holds is
        // newer than this build.
        guard params.count == 1, let value = params.values.first else { return english }
        return String(format: format, value)
    }

    /// `word` for an API error code, whose message is `apierr_<code>`.
    public static func word(code: String, params: [String: String] = [:], english: String) -> String {
        word("apierr_" + code, params: params, english: english)
    }

    /// The sentence of an `ApiError` body (`{"error", "code", "params"}`) in the app's language
    /// when it names a message this build has, else the host's English. nil for a body that is
    /// not one or says nothing.
    public static func apiError(_ body: Data) -> String? {
        guard let json = try? JSONSerialization.jsonObject(with: body) as? [String: Any] else { return nil }
        let english = json["error"] as? String ?? ""
        let code = (json["code"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        let said: String
        if let code {
            let params = (json["params"] as? [String: Any])?.compactMapValues { $0 as? String } ?? [:]
            said = word(code: code, params: params, english: english)
        } else {
            said = english
        }
        return said.isEmpty ? nil : said
    }

    /// A launch verdict as `punktfunk_connection_launch_message` writes it: the message id, the
    /// host's English, then `name<TAB>value` for each placeholder, a line each. nil for none.
    public static func launch(lines: String) -> String? {
        let rows = lines.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        guard rows.count >= 2 else { return nil }
        var params: [String: String] = [:]
        for row in rows.dropFirst(2) {
            let field = row.split(separator: "\t", maxSplits: 1, omittingEmptySubsequences: false)
            if field.count == 2 { params[String(field[0])] = String(field[1]) }
        }
        let said = word(rows[0], params: params, english: rows[1])
        return said.isEmpty ? nil : said
    }

    /// The plural messages, picked by their `count`. The accessor holds every language's plural
    /// rules, so each is spelled out.
    private static let plurals: [String: (Int) -> String] = [
        "apierr_profile_limit": { L10n.apierrProfileLimit(count: $0) },
        "apierr_unlock_locked_out_device_minutes": { L10n.apierrUnlockLockedOutDeviceMinutes(count: $0) },
        "apierr_unlock_locked_out_device_seconds": { L10n.apierrUnlockLockedOutDeviceSeconds(count: $0) },
        "apierr_unlock_locked_out_password_minutes": { L10n.apierrUnlockLockedOutPasswordMinutes(count: $0) },
        "apierr_unlock_locked_out_password_seconds": { L10n.apierrUnlockLockedOutPasswordSeconds(count: $0) },
    ]

    /// The messages with two placeholders. The accessor takes them in the order the English shows;
    /// nil when the host's values are not exactly the two this build knows.
    private static let several: [String: ([String: String]) -> String?] = [
        "launch_closed_at_once": { p in
            pair(p, "title", "seconds").map { L10n.launchClosedAtOnce(title: $0, seconds: $1) }
        },
        "launch_download_failed_reason": { p in
            pair(p, "title", "reason").map { L10n.launchDownloadFailedReason(title: $0, reason: $1) }
        },
        "launch_download_refused": { p in
            pair(p, "title", "reason").map { L10n.launchDownloadRefused(title: $0, reason: $1) }
        },
        "seatline_stage_display_try": { p in
            pair(p, "attempt", "total").map { L10n.seatlineStageDisplayTry(attempt: $0, total: $1) }
        },
        "seatline_stage_host_try": { p in
            pair(p, "attempt", "total").map { L10n.seatlineStageHostTry(attempt: $0, total: $1) }
        },
        "seatline_stage_sign_in_try": { p in
            pair(p, "attempt", "total").map { L10n.seatlineStageSignInTry(attempt: $0, total: $1) }
        },
    ]

    private static func pair(_ params: [String: String], _ a: String, _ b: String) -> (String, String)? {
        guard params.count == 2, let first = params[a], let second = params[b] else { return nil }
        return (first, second)
    }
}
