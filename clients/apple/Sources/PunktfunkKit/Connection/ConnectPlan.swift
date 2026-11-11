// What a tap on a host decides before its dial. `ConnectFlow` gathers the facts, calls these and
// performs the result. Pure, so `swift test` checks every row (`ConnectPlanTests`).

import Foundation

public enum ConnectPlan {
    /// The way a tap goes in.
    public enum Door: Equatable {
        /// Dial. `allowTofu`: an unpinned host whose trust prompt may pin it (rule 3a).
        /// `asksProfile`: the paired host is asked who is playing first.
        case dial(allowTofu: Bool, asksProfile: Bool)
        /// Request access or PIN pairing, never a silent connect (rule 3b). `resumes`: a link's
        /// connect runs again once the host is pinned.
        case approvalChoice(advertisedFingerprint: Data?, resumes: Bool)
    }

    /// A pinned host dials on its stored fingerprint. An unpinned one may trust on first use when
    /// the caller's `allowTofu` says so, or, without one, when a live advert says `pair=optional`
    /// (`advertsTofu`). Only a pinned host other than the demo, which has no management API, is
    /// asked for its profiles.
    public static func door(
        pinned: Bool, allowTofu: Bool?, advertsTofu: Bool, advertisedFingerprint: Data?,
        fromLink: Bool, isDemo: Bool
    ) -> Door {
        if pinned { return .dial(allowTofu: false, asksProfile: !isDemo) }
        guard allowTofu ?? advertsTofu else {
            return .approvalChoice(advertisedFingerprint: advertisedFingerprint, resumes: fromLink)
        }
        return .dial(allowTofu: true, asksProfile: false)
    }

    /// What follows the host's profile list.
    public enum AfterProfiles: Equatable {
        /// No answer: dial `id`, named when it is the saved pick.
        case dial(id: String?, name: String?)
        /// No picker: save `remember`, then settle `id`'s seat and dial.
        case settle(id: String?, remember: ProfilePick?)
        /// Show the picker. `saved` is the remembered pick it still lists, `gone` the name of one
        /// it dropped.
        case pick([ListedProfile], saved: String?, gone: String?)
    }

    /// `rows` is the list, nil for a box without profiles; `failed`: no answer in time. `link` is
    /// a link's `as=`, which wins for this connect. A failed answer dials as before: the link's
    /// profile, else the saved pick.
    public static func afterProfiles(
        rows: [ListedProfile]?, failed: Bool, saved: ProfilePick?, link: String?
    ) -> AfterProfiles {
        if failed {
            let id = link ?? saved?.id
            return .dial(id: id, name: id == saved?.id ? saved?.displayName : nil)
        }
        let d = HostProfiles.pickerDecision(listed: rows, remembered: saved, link: link)
        guard d.picker, let rows else { return .settle(id: d.send, remember: d.remember) }
        return .pick(rows, saved: rows.first { $0.id == saved?.id }?.id, gone: d.gone)
    }
}
