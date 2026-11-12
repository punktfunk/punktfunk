// Which of our shortcuts Steam says is up, so the Play button can read Stop, and the per-game
// shortcut's last-played time mirrored onto its Steam title.
import { signal } from "../lib/signal";
import type { AppStoreGlobal, SteamClientGlobal } from "./client";
import {
  gameShortcutFor,
  gameShortcutPairs,
  recall,
  STORAGE_KEY_STREAM,
  steamAppIdForShortcut,
} from "./keys";
import { sleep, STORE_GRACE_MS, STORE_WAIT_MS, waitForServicesInitialized } from "./readiness";

declare const SteamClient: SteamClientGlobal;
declare const appStore: AppStoreGlobal;

/** Shortcut appIds Steam reports running. Whoever changes it calls `notifyRunning`. */
export const running = new Set<number>();
const runningChanged = signal<void>(undefined);
export const notifyRunning = runningChanged.notify;
export const subscribeRunning = runningChanged.subscribe;

/** Is any of our stream shortcuts, generic or per-game, up right now? */
export function isAnyStreamRunning(): boolean {
  return running.size > 0;
}

function isOurShortcut(appId: number): boolean {
  return appId === recall(STORAGE_KEY_STREAM) || steamAppIdForShortcut(appId) != null;
}

/**
 * Copy the per-game shortcut's last-played time onto the Steam title, so the GAME climbs the
 * Deck's Recent shelf after a stream — a hidden entry never shows there. Steam persists the
 * shortcut's own timestamp, so re-applying at load carries it across reboots. Only ever moves
 * forward; a title played locally more recently keeps its own time.
 */
function mirrorLastPlayed(steamAppId: number, shortcutAppId: number): void {
  try {
    if (typeof appStore === "undefined" || !appStore?.GetAppOverviewByAppID) {
      return;
    }
    const from = appStore.GetAppOverviewByAppID(shortcutAppId);
    const to = appStore.GetAppOverviewByAppID(steamAppId);
    const played = from?.rt_last_time_locally_played;
    if (!to || typeof played !== "number" || played <= (to.rt_last_time_locally_played ?? 0)) {
      return;
    }
    to.rt_last_time_locally_played = played;
  } catch (e) {
    console.warn("punktfunk: last-played not mirrored", e);
  }
}

function mirrorAllLastPlayed(): void {
  for (const [steamAppId, shortcut] of gameShortcutPairs()) {
    mirrorLastPlayed(steamAppId, shortcut);
  }
}

/** Follow Steam's app lifetime feed for our shortcuts. Returns the unregister for dismount. */
export function watchRunningStreams(): () => void {
  // Re-apply the mirrored timestamps once the store is readable — a fresh boot's Recent shelf
  // should already show last night's stream.
  void (async () => {
    await Promise.race([waitForServicesInitialized(), sleep(STORE_WAIT_MS)]);
    await sleep(STORE_GRACE_MS);
    mirrorAllLastPlayed();
  })();
  try {
    const reg = SteamClient.GameSessions.RegisterForAppLifetimeNotifications((n) => {
      if (!isOurShortcut(n.unAppID)) {
        return;
      }
      if (n.bRunning) {
        running.add(n.unAppID);
      } else {
        running.delete(n.unAppID);
      }
      notifyRunning();
      const steamAppId = steamAppIdForShortcut(n.unAppID);
      if (steamAppId != null) {
        // Steam stamps the shortcut at start; a beat later it is readable here.
        setTimeout(() => mirrorLastPlayed(steamAppId, n.unAppID), 1500);
      }
    });
    return () => reg.unregister();
  } catch (e) {
    console.warn("punktfunk: app lifetime feed unavailable", e);
    return () => {};
  }
}

/** Is this Steam title being streamed right now (its per-game shortcut is up)? */
export function isGameStreaming(steamAppId: number): boolean {
  const shortcut = gameShortcutFor(steamAppId);
  return shortcut != null && running.has(shortcut);
}
