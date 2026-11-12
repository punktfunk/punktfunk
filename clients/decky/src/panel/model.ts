// What the Quick Access panel decides about its rows. Pure, so scripts/test-panel.ts runs it
// under plain node. Each function returns a kind; the sections turn kinds into words, which keeps
// every `t("…")` literal where the l10n test reads it. The host split lives beside `needsPair` in
// hosts/model.ts and the update rows beside the update predicates in update/model.ts.
import type { HostView } from "../hosts/model";

export type RowStatus = "online" | "offline" | "offline-wakes";

/** Whether a host answers now. An offline host with a MAC wakes when a launch dials it. */
export function rowStatus(v: HostView): RowStatus {
  if (v.online) return "online";
  return v.wakeable ? "offline-wakes" : "offline";
}

/** The names two or more rows share. Those rows show their address; no other row does. */
export function sharedNames(views: HostView[]): Set<string> {
  const seen = new Set<string>();
  const shared = new Set<string>();
  for (const v of views) {
    (seen.has(v.name) ? shared : seen).add(v.name);
  }
  return shared;
}

/**
 * What a host row's ▾ menu lists, or null for a row without one. Profiles count from two: a host
 * with a single profile always streams as it. `power` counts Wake and the host's own actions.
 */
export function rowMenu(
  presets: number,
  profiles: number,
  power: number,
): { profiles: boolean; presets: boolean; power: boolean } | null {
  const menu = { profiles: profiles >= 2, presets: presets > 0, power: power > 0 };
  return menu.profiles || menu.presets || menu.power ? menu : null;
}

/**
 * Is a stream up? The control socket shows a running client; our running shortcut covers a
 * launch still waking its host or connecting, which has no socket yet and still needs Stop.
 */
export function streamIsUp(socket: boolean, shortcut: boolean): boolean {
  return socket || shortcut;
}
