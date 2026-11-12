// What the shortcut code remembers across reloads, and the per-game shortcut registry read from
// it. An appId is stable for the life of its shortcut, so remembering one per role reuses it
// instead of churning the library. The registry's cache sits beside `remember`, the one writer
// that has to drop it.
import { keys, read, write } from "../lib/storage";

// The hidden stream shortcut's appId. The oldest key, so a one-shortcut install migrates into the
// stream role.
export const STORAGE_KEY_STREAM = "punktfunk:shortcutAppId";
export const STORAGE_KEY_UI = "punktfunk:uiAppId";
// The shortcut the most recent launch ran under, generic or per-game: the one Force-stop ends.
export const STORAGE_KEY_LAST_LAUNCH = "punktfunk:lastLaunchAppId";
/** `{ref, gameId, title}` of the last game-page stream: what the panel's End game ends. */
export const STORAGE_KEY_LAST_GAME = "punktfunk:lastGameStream";
// `punktfunk:gameShortcut:<steam appid>` → the per-game shortcut's appId.
const GAME_KEY_PREFIX = "punktfunk:gameShortcut:";

// The native-touch layout marker. Steam keys a layout by shortcut name: one marker covers both
// fixed roles, and each per-game name gets its own under the prefix.
export const CONFIG_KEY = "punktfunk:controllerConfig";
export const GAME_CONFIG_PREFIX = "punktfunk:controllerConfig:";

export function artKey(appId: number): string {
  return `punktfunk:shortcutArt:${appId}`;
}

export function gameArtKey(shortcutAppId: number): string {
  return `punktfunk:gameArt:${shortcutAppId}`;
}

export function gameKey(steamAppId: number): string {
  return `${GAME_KEY_PREFIX}${steamAppId}`;
}

export function remember(key: string, appId: number) {
  write(key, String(appId));
  if (key.startsWith(GAME_KEY_PREFIX)) {
    pairCache = null; // rebuilt from storage on the next read
  }
}

export function recall(key: string): number | null {
  const v = read(key);
  return v ? Number(v) : null;
}

/** The per-game shortcut minted for this Steam title, if any (liveness not checked here). */
export function gameShortcutFor(steamAppId: number): number | null {
  return recall(gameKey(steamAppId));
}

// Read from storage once and kept in step with our own writes; nothing else touches these keys.
// isOurShortcut runs for every app Steam starts or stops, where a full scan is work for nothing.
let pairCache: Array<[number, number]> | null = null;

/** Drop the cache after removing game keys behind `remember`'s back. */
export function dropGamePairCache(): void {
  pairCache = null;
}

/** Every (steam appid → shortcut appId) pair on record. */
export function gameShortcutPairs(): Array<[number, number]> {
  if (pairCache) {
    return pairCache;
  }
  const pairs: Array<[number, number]> = [];
  const found = keys(GAME_KEY_PREFIX);
  for (const key of found ?? []) {
    const steamAppId = Number(key.slice(GAME_KEY_PREFIX.length));
    const shortcut = Number(read(key));
    if (Number.isFinite(steamAppId) && Number.isFinite(shortcut) && shortcut > 0) {
      pairs.push([steamAppId, shortcut]);
    }
  }
  if (found) {
    pairCache = pairs; // only a scan that finished; a failed one must be retried, not cached
  }
  return pairs;
}

/** The Steam title a per-game shortcut stands for, or null for any other appid. */
export function steamAppIdForShortcut(shortcutAppId: number): number | null {
  return gameShortcutPairs().find(([, s]) => s === shortcutAppId)?.[0] ?? null;
}
