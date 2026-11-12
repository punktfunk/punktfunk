// What a pending update means for the panel. Pure, so scripts/test-update.ts runs it under node.
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
