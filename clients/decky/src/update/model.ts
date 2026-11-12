// What a pending update means for the panel. Pure, so scripts/test-update.ts and
// scripts/test-panel.ts run it under node.
import type { UpdateInfo } from "../backend";

/** True when EITHER the plugin or the client has a pending update. */
export function hasUpdate(info: UpdateInfo | null | undefined): boolean {
  return !!info && (info.update_available || info.client_update_available);
}

/**
 * Can this Deck actually INSTALL the pending client update, or only tell you how?
 *
 * A flatpak and a one-tap-capable native install (the packaged root helper + the operator's
 * group opt-in) get a button; a sysext, a nix profile, a source build or a box that hasn't
 * opted in gets the command. Offering a button that can only fail is worse than saying so.
 */
export function clientUpdateIsOneTap(info: UpdateInfo | null | undefined): boolean {
  return (
    !!info &&
    info.client_update_available &&
    (info.client_applier === "flatpak" || info.client_applier === "helper")
  );
}

/** True when the only pending update is one this Deck can't apply itself. */
export function clientUpdateIsManualOnly(info: UpdateInfo | null | undefined): boolean {
  return !!info && info.client_update_available && !clientUpdateIsOneTap(info);
}

/**
 * The panel's update row. A client this Deck can't install gets its command, not a button: a tap
 * that can only fail is worse than one line to read. A pending plugin update still gets the
 * button, and promises "+ client" only when the same tap really installs it.
 */
export type UpdateBanner = "none" | "manual-client" | "plugin-and-client" | "plugin" | "client";

export function updateBanner(info: UpdateInfo | null | undefined): UpdateBanner {
  if (!info || !hasUpdate(info)) return "none";
  if (!info.update_available) return clientUpdateIsManualOnly(info) ? "manual-client" : "client";
  return clientUpdateIsOneTap(info) ? "plugin-and-client" : "plugin";
}

/** What "Check for updates" found. `-unknown`: the client's new version number is missing. */
export type CheckKind =
  | "unreachable"
  | "both"
  | "both-unknown"
  | "plugin"
  | "client"
  | "client-unknown"
  | "client-outdated"
  | "client-check-failed"
  | "dev-build"
  | "up-to-date";

/**
 * The outcome of an explicit check, and whether the client part needs a hand install. A failed
 * client check is never `up-to-date`: that is the one wrong answer.
 */
export function checkOutcome(res: UpdateInfo | null): { kind: CheckKind; manual: boolean } {
  if (!res || res.error === "fetch-failed") return { kind: "unreachable", manual: false };
  if (hasUpdate(res)) {
    const known = !!res.client_latest;
    const kind: CheckKind =
      res.update_available && res.client_update_available
        ? known ? "both" : "both-unknown"
        : res.update_available
          ? "plugin"
          : known ? "client" : "client-unknown";
    return { kind, manual: clientUpdateIsManualOnly(res) };
  }
  if (res.client_error) {
    const kind = res.client_error === "client-outdated" ? "client-outdated" : "client-check-failed";
    return { kind, manual: false };
  }
  if (res.error === "update-channel-unknown") return { kind: "dev-build", manual: false };
  return { kind: "up-to-date", manual: false };
}
