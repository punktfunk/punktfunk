// The stream that looks like the game: a stream started from a Steam game's own page runs under a
// hidden per-game shortcut named after the game and wearing its art and icon, so the overlay, the
// "now playing" surfaces and the friends list show the game rather than "Punktfunk". Minted on
// the first Stream tap for that game and reused after; keys.ts holds the registry.
import { keys, write } from "../lib/storage";
import { applyGameArtwork } from "./art";
import type { SteamClientGlobal } from "./client";
import { dropGamePairCache, GAME_CONFIG_PREFIX, gameArtKey, gameKey, gameShortcutPairs } from "./keys";
import { queryShortcutAlive } from "./readiness";
import { notifyRunning, running } from "./running";
import { type Ensured, ensureShortcut } from "./shortcuts";

declare const SteamClient: SteamClientGlobal;

/**
 * Ensure the hidden per-game shortcut for a Steam title: named after the game, dressed in its
 * art, pointed at the touch layout. Reused across launches by appid; recreated if the user
 * removed it. A title carrying a quote gets no layout: the name is written into a VDF as a
 * quoted key, and Steam's default layout beats a corrupt file.
 */
export function ensureGameShortcut(
  steamAppId: number,
  title: string,
  iconHash: string,
): Promise<Ensured> {
  return ensureShortcut({
    key: gameKey(steamAppId),
    name: title,
    hidden: true,
    configMarker: title.includes('"') ? null : `${GAME_CONFIG_PREFIX}${title.toLowerCase()}`,
    artKey: gameArtKey,
    dress: (appId) => void applyGameArtwork(appId, steamAppId, iconHash),
  });
}

/** Every key under `prefix`, gone. */
function removeByPrefix(prefix: string): void {
  for (const key of keys(prefix) ?? []) {
    write(key, null);
  }
}

/** Remove every per-game shortcut on record — the cleanup button. Returns how many went. */
export function removeGameShortcuts(): number {
  let removed = 0;
  for (const [steamAppId, shortcut] of gameShortcutPairs()) {
    try {
      if (queryShortcutAlive(shortcut) !== false) {
        SteamClient.Apps.RemoveShortcut(shortcut);
        removed++;
      }
      write(gameKey(steamAppId), null);
      write(gameArtKey(shortcut), null);
      running.delete(shortcut); // ours only — a generic stream that is up keeps its state
    } catch (e) {
      console.warn("punktfunk: game shortcut not removed", e);
    }
  }
  // The per-game layouts are keyed by game name, so no appid can find them.
  removeByPrefix(GAME_CONFIG_PREFIX);
  dropGamePairCache();
  notifyRunning();
  return removed;
}
