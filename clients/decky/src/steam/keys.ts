// What the shortcut code remembers across reloads, and the per-game and local entry registries
// read from it. An appId is stable for the life of its shortcut, so remembering one per role
// reuses it instead of churning the library. The registries' cache sits beside `remember` and
// `dropPairCache`, the two places that drop it.
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
// `punktfunk:localEntry:<library id>` → the appId of that title's entry, synced from this device.
const LOCAL_KEY_PREFIX = "punktfunk:localEntry:";
/** The host's sources the player switched on, as a JSON array of source ids. */
export const STORAGE_KEY_LOCAL_SOURCES = "punktfunk:localSources";
/** The collections the sync made, as a JSON object of name → collection id. */
export const STORAGE_KEY_LOCAL_COLLECTIONS = "punktfunk:localCollections";
/** The launch options the local entries were last pointed with. */
export const STORAGE_KEY_LOCAL_LAUNCH = "punktfunk:localLaunch";

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

export function localArtKey(appId: number): string {
  return `punktfunk:localArt:${appId}`;
}

export function gameKey(steamAppId: number): string {
  return `${GAME_KEY_PREFIX}${steamAppId}`;
}

export function localKey(libraryId: string): string {
  return `${LOCAL_KEY_PREFIX}${libraryId}`;
}

export function remember(key: string, appId: number) {
  write(key, String(appId));
  for (const prefix of pairCache.keys()) {
    if (key.startsWith(prefix)) {
      pairCache.delete(prefix); // rebuilt from storage on the next read
    }
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

// Read from storage once per prefix and kept in step with our own writes; nothing else touches
// these keys. isOurShortcut runs for every app Steam starts or stops, where a full scan is work
// for nothing.
const pairCache = new Map<string, Array<[string, number]>>();

/** Drop the cache after removing registry keys behind `remember`'s back. */
export function dropPairCache(): void {
  pairCache.clear();
}

/** Every (key suffix → appId) pair on record under `prefix`; null when storage can't be read. */
function pairsUnder(prefix: string): Array<[string, number]> | null {
  const cached = pairCache.get(prefix);
  if (cached) {
    return cached;
  }
  const found = keys(prefix);
  if (!found) {
    return null; // a failed scan is retried, never cached
  }
  const pairs: Array<[string, number]> = [];
  for (const key of found) {
    const appId = Number(read(key));
    if (Number.isFinite(appId) && appId > 0) {
      pairs.push([key.slice(prefix.length), appId]);
    }
  }
  pairCache.set(prefix, pairs);
  return pairs;
}

/** Every (steam appid → shortcut appId) pair on record. */
export function gameShortcutPairs(): Array<[number, number]> {
  return (pairsUnder(GAME_KEY_PREFIX) ?? [])
    .map(([id, shortcut]): [number, number] => [Number(id), shortcut])
    .filter(([id]) => Number.isFinite(id));
}

/** Every (library id → local entry appId) pair on record; null when storage can't be read. */
export function localEntryPairs(): Array<[string, number]> | null {
  return pairsUnder(LOCAL_KEY_PREFIX);
}

/** The Steam title a per-game shortcut stands for, or null for any other appid. */
export function steamAppIdForShortcut(shortcutAppId: number): number | null {
  return gameShortcutPairs().find(([, s]) => s === shortcutAppId)?.[0] ?? null;
}
