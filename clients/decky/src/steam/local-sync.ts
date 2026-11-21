// Syncs this device's library into Steam: each title of a source the player switched on becomes a
// Steam entry of its own, filed in a collection, and the host gets the map back. Adds run at
// plugin load, on panel open when the last run is stale, and after a switch; removals only from
// the panel, and only of entries on record. One run at a time; storage records each entry as it
// lands, so a run stopped part-way resumes on the next.
import { hostBin, localEntries, localMap, runnerInfo } from "../backend";
import { diag } from "../lib/diag";
import { signal } from "../lib/signal";
import { keys, read, readJson, write } from "../lib/storage";
import { applyHostArtwork } from "./art";
import type { AppStoreGlobal, SteamClientGlobal } from "./client";
import { dropEmptyCollections, fileIn, tidyCollections, unfile } from "./collections";
import {
  dropPairCache,
  GAME_CONFIG_PREFIX,
  gameShortcutPairs,
  localArtKey,
  localEntryPairs,
  localKey,
  recall,
  STORAGE_KEY_LOCAL_LAUNCH,
  STORAGE_KEY_LOCAL_SOURCES,
  STORAGE_KEY_STREAM,
  STORAGE_KEY_UI,
} from "./keys";
import { isSafeClientBin, localLaunchOptions } from "./launch-options";
import { ensureLocalEntry } from "./local-entries";
import { collectionFor, type LocalAnswer, plan, readAnswer, type Removal, steamMap, wanted } from "./local-plan";
import { queryShortcutAlive, sleep, STORE_GRACE_MS, STORE_WAIT_MS, waitForServicesInitialized } from "./readiness";

declare const SteamClient: SteamClientGlobal;
declare const appStore: AppStoreGlobal;

// At most four adds or removals a second: how Steam takes thousands of added entries is unmeasured.
const PACE_MS = 250;
// A panel open re-runs the sync once the last run that reached the host is this old.
const STALE_MS = 10 * 60_000;

export interface LocalState {
  /** The host's last answer; null while it has not answered or Steam does not own the desk. */
  answer: LocalAnswer | null;
  /** Adds and removals left in the run under way. */
  left: number;
}

export const localState = signal<LocalState>({ answer: null, left: 0 });

/** The host's sources the player switched on. */
export function sourcesOn(): Set<string> {
  const ids = readJson<unknown>(STORAGE_KEY_LOCAL_SOURCES);
  return new Set(Array.isArray(ids) ? ids.filter((id): id is string => typeof id === "string") : []);
}

/** Switch a source on or off, then sync: an off source's entries go. */
export function setSourceOn(id: string, on: boolean): void {
  const ids = sourcesOn();
  if (on) {
    ids.add(id);
  } else {
    ids.delete(id);
  }
  write(STORAGE_KEY_LOCAL_SOURCES, JSON.stringify([...ids]));
  requestSync("off");
}

/** Lowercase names that carry the stream's controller layout. */
export function streamLayoutNames(): Set<string> {
  return new Set((keys(GAME_CONFIG_PREFIX) ?? []).map((k) => k.slice(GAME_CONFIG_PREFIX.length)));
}

let lastRun = 0;
let running: Promise<void> | null = null;
let next: Removal | null = null;
let stopped = false;
const RANK: Record<Removal, number> = { none: 0, off: 1, all: 2 };

/** Sync now, or once the run under way is done; that run stops adding at its next step. */
export function requestSync(removal: Removal): void {
  if (stopped) {
    return;
  }
  next = next == null || RANK[removal] > RANK[next] ? removal : next;
  running ??= drive().finally(() => {
    running = null;
    if (next != null) {
      requestSync(next); // asked for after the last run's final check
    }
  });
}

/** The panel opened: sync when the host has not answered yet or the last run is stale. */
export function panelOpened(): void {
  if (!localState.get().answer || Date.now() - lastRun > STALE_MS) {
    requestSync("off");
  }
}

/** The plugin is going: no further step runs. */
export function stopSync(): void {
  stopped = true;
  next = null;
}

async function drive(): Promise<void> {
  while (next != null) {
    const removal = next;
    next = null;
    try {
      await run(removal);
    } catch (e) {
      diag(`local sync stopped: ${e}`);
    }
  }
  localState.set({ ...localState.get(), left: 0 });
}

function remove(id: string, appId: number): void {
  try {
    // Steam keeps a removed entry's art files until each slot is cleared.
    for (const slot of [0, 1, 2, 3]) {
      SteamClient.Apps.ClearCustomArtworkForApp?.(appId, slot)?.catch(() => {});
    }
    if (queryShortcutAlive(appId) !== false) {
      SteamClient.Apps.RemoveShortcut(appId);
    }
    write(localKey(id), null);
    write(localArtKey(appId), null);
  } catch (e) {
    console.warn("punktfunk: synced entry not removed", e);
  }
}

async function run(removal: Removal): Promise<void> {
  if (lastRun === 0) {
    await Promise.race([waitForServicesInitialized(), sleep(STORE_WAIT_MS)]);
    await sleep(STORE_GRACE_MS);
  }
  const answer = readAnswer(await localEntries().catch(() => null));
  localState.set({ answer, left: 0 });
  const have = localEntryPairs();
  if (!answer || !have) {
    return; // no answer, or storage unreadable: nothing is known, so nothing is touched
  }
  lastRun = Date.now();
  const work = plan(answer, sourcesOn(), have, queryShortcutAlive, removal);
  let left = work.add.length + work.drop.length;
  const step = () => localState.set({ answer, left: --left });
  localState.set({ answer, left });

  if (work.drop.length > 0) {
    unfile(work.drop.map(([, appId]) => appId));
    for (const [id, appId] of work.drop) {
      if (stopped) {
        return;
      }
      remove(id, appId);
      step();
      await sleep(PACE_MS);
    }
    dropPairCache();
  }

  const [bin, info] = await Promise.all([hostBin(), runnerInfo()]);
  const options = isSafeClientBin(bin.path) && info.exists ? localLaunchOptions(info.runner, bin.path) : null;
  if (!options) {
    diag(`local sync: no entry is made, host ${bin.path || "not found"}, wrapper ${info.runner}`);
  }
  for (const e of options ? work.add : []) {
    if (stopped || next != null) {
      return; // the next run starts from what is on record
    }
    const started = Date.now();
    try {
      const { appId } = await ensureLocalEntry(e, bin.path);
      await applyHostArtwork(appId, e.id, e.art);
    } catch (err) {
      diag(`local sync: ${e.id} not added: ${err}`);
    }
    step();
    await sleep(Math.max(0, PACE_MS - (Date.now() - started)));
  }
  localState.set({ answer, left: 0 });

  // A moved host binary or plugin reaches every entry on record.
  if (options && read(STORAGE_KEY_LOCAL_LAUNCH) !== options) {
    for (const [, appId] of localEntryPairs() ?? []) {
      try {
        if (queryShortcutAlive(appId) === true) {
          SteamClient.Apps.SetShortcutStartDir(appId, info.runner.replace(/\/[^/]*$/, ""));
          SteamClient.Apps.SetAppLaunchOptions(appId, options);
        }
      } catch (err) {
        diag(`local sync: ${appId} not repointed: ${err}`);
      }
    }
    write(STORAGE_KEY_LOCAL_LAUNCH, options);
  }
  // A renamed title, or new art for it, reaches its entry.
  for (const [e, appId] of work.keep) {
    try {
      const shown = typeof appStore === "undefined" ? null : appStore?.GetAppOverviewByAppID?.(appId)?.display_name;
      if (shown != null && shown !== e.title) {
        SteamClient.Apps.SetShortcutName(appId, e.title);
      }
      await applyHostArtwork(appId, e.id, e.art);
    } catch (err) {
      diag(`local sync: ${e.id} not updated: ${err}`);
    }
  }

  // A fresh entry registers a moment after AddShortcut; one still missing is filed next run.
  await sleep(STORE_GRACE_MS);
  const now = localEntryPairs();
  if (!now) {
    return;
  }
  const appIds = new Map(now);
  const home = new Map<number, string>();
  const groups = new Map<string, number[]>();
  for (const e of wanted(answer, sourcesOn())) {
    const appId = appIds.get(e.id);
    if (appId != null) {
      const name = collectionFor(e, answer.sources);
      home.set(appId, name);
      const group = groups.get(name) ?? [];
      group.push(appId);
      groups.set(name, group);
    }
  }
  tidyCollections(home);
  for (const [name, ids] of groups) {
    await fileIn(name, ids);
  }
  if (removal !== "none") {
    await dropEmptyCollections();
  }

  const own = [recall(STORAGE_KEY_STREAM), recall(STORAGE_KEY_UI), ...gameShortcutPairs().map(([, s]) => s)];
  const sent = await localMap(steamMap(now, own)).catch(() => null);
  if (!sent?.ok) {
    diag(`local sync: map not sent (${sent?.error ?? "backend"})`);
  }
}
