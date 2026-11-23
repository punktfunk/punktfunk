// The Steam collections synced titles are filed in, one per platform, else per source. A
// collection of that name the player already has is used as it is. One the sync made is kept
// under STORAGE_KEY_LOCAL_COLLECTIONS by its id and deleted once empty; nothing else is ever
// deleted. Every call is guarded: a Steam without these methods syncs with no collections.
import { readJson, write } from "../lib/storage";
import type {
  AppStoreGlobal,
  CollectionStoreGlobal,
  SteamAppOverviewLike,
  SteamCollection,
} from "./client";
import { STORAGE_KEY_LOCAL_COLLECTIONS } from "./keys";

declare const collectionStore: CollectionStoreGlobal;
declare const appStore: AppStoreGlobal;

/** name → id of each collection the sync made. */
function made(): Record<string, string> {
  return readJson<Record<string, string>>(STORAGE_KEY_LOCAL_COLLECTIONS) ?? {};
}

function registered(appId: number): boolean {
  return typeof appStore !== "undefined" && appStore?.GetAppOverviewByAppID?.(appId) != null;
}

/** Have the player's collections loaded? Every account has the system "hidden" one, and each
 *  store method throws before the load. */
function loaded(): boolean {
  try {
    return typeof collectionStore !== "undefined" && collectionStore?.GetCollection?.("hidden") != null;
  } catch {
    return false;
  }
}

function named(name: string): SteamCollection[] {
  return collectionStore?.GetUserCollectionsByName?.(name) ?? [];
}

const same = (a: string, b: string) => a.toLocaleLowerCase() === b.toLocaleLowerCase();

/**
 * File the apps in the collection named `name`, made when the player has none by that name:
 * Steam deletes a same-named one when a new collection is saved. An app Steam has not registered
 * yet is left for the next call, since its membership would be saved to the account's cloud.
 */
export async function fileIn(name: string, appIds: number[]): Promise<void> {
  try {
    if (!loaded()) {
      return;
    }
    const found = named(name)[0];
    const apps = appIds.filter((id) => !found?.apps.has(id) && registered(id)).map((appid) => ({ appid }));
    if (apps.length === 0) {
      return;
    }
    if (found) {
      found.AsDragDropCollection?.()?.AddApps(apps);
      return;
    }
    const fresh = collectionStore?.NewUnsavedCollection?.(name, undefined, apps);
    if (fresh?.Save) {
      await fresh.Save();
      write(STORAGE_KEY_LOCAL_COLLECTIONS, JSON.stringify({ ...made(), [name]: fresh.id }));
    }
  } catch (e) {
    console.warn("punktfunk: collection not filed", name, e);
  }
}

/** Take each synced app out of a collection the sync made that is no longer its own. `home` maps
 *  a synced app to its collection's name; any other app stays where it is. */
export function tidyCollections(home: Map<number, string>): void {
  try {
    if (!loaded()) {
      return;
    }
    for (const [name, id] of Object.entries(made())) {
      const ours = named(name).find((c) => c.id === id);
      const strays: SteamAppOverviewLike[] = [...(ours?.apps ?? [])]
        .filter((appid) => home.has(appid) && !same(home.get(appid) ?? "", name))
        .map((appid) => ({ appid }));
      if (strays.length > 0) {
        ours?.AsDragDropCollection?.()?.RemoveApps(strays);
      }
    }
  } catch (e) {
    console.warn("punktfunk: collections not tidied", e);
  }
}

/** Take the apps out of every user collection they are in, before their entries go: a dead
 *  appId left in a collection is saved to the account's cloud as if it were a Steam game. */
export function unfile(appIds: number[]): void {
  try {
    if (!loaded()) {
      return;
    }
    const byId = new Map<string, [SteamCollection, SteamAppOverviewLike[]]>();
    for (const appid of appIds) {
      for (const c of collectionStore?.GetCollectionListForAppID?.(appid) ?? []) {
        const group = byId.get(c.id) ?? [c, []];
        group[1].push({ appid });
        byId.set(c.id, group);
      }
    }
    for (const [c, apps] of byId.values()) {
      c.AsDragDropCollection?.()?.RemoveApps(apps);
    }
  } catch (e) {
    console.warn("punktfunk: synced entries not unfiled", e);
  }
}

/** Delete each collection the sync made that is now empty, and forget one that is gone. */
export async function dropEmptyCollections(): Promise<void> {
  try {
    if (!loaded()) {
      return;
    }
    const record = made();
    for (const [name, id] of Object.entries(record)) {
      const ours = named(name).find((c) => c.id === id);
      if (ours && ours.apps.size > 0) {
        continue;
      }
      await ours?.AsDeletableCollection?.()?.Delete();
      delete record[name];
    }
    write(STORAGE_KEY_LOCAL_COLLECTIONS, JSON.stringify(record));
  } catch (e) {
    console.warn("punktfunk: empty collections not deleted", e);
  }
}
