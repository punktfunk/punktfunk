// Is Steam's app store ready to say a shortcut is gone? The plugin mounts while Steam is still
// starting, before appStore has registered its overviews, so a live shortcut can look up as
// null. A null overview is believed only once the store is demonstrably hydrated.
import type { AppGlobal, AppStoreGlobal } from "./client";

declare const App: AppGlobal;
declare const appStore: AppStoreGlobal;

export const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

let servicesInitialized: Promise<void> | undefined;
export function waitForServicesInitialized(): Promise<void> {
  servicesInitialized ??= (async () => {
    try {
      if (typeof App !== "undefined" && App?.WaitForServicesInitialized) {
        await App.WaitForServicesInitialized();
      }
    } catch {
      /* no signal — the hydration witness still gates the verdict */
    }
  })();
  return servicesInitialized;
}

/** Has appStore demonstrably finished its initial load? An empty `allApps` means "not yet":
 *  any account that ever had our shortcut has at least one app, so a populated map is the
 *  witness that a null overview lookup is an ANSWER rather than a not-loaded-yet. null =
 *  can't tell (missing global, API drift). */
function appStoreHydrated(): boolean | null {
  try {
    if (typeof appStore === "undefined" || !appStore) {
      return null;
    }
    const apps = appStore.allApps;
    return Array.isArray(apps) ? apps.length > 0 : null;
  } catch {
    return null;
  }
}

/** One overview lookup: true = live, false = absent, null = can't tell. */
export function queryShortcutAlive(appId: number): boolean | null {
  try {
    // A method call on appStore, never an extracted function: a lost `this` throws, and the
    // catch would turn that into a permanent "can't tell". `typeof` first, because a bare
    // reference to a missing global is a ReferenceError optional chaining does not prevent.
    if (typeof appStore === "undefined" || !appStore?.GetAppOverviewByAppID) {
      return null;
    }
    return appStore.GetAppOverviewByAppID(appId) != null;
  } catch {
    return null;
  }
}

// 30 s is comfortably past any real boot's store hydration, and the wait burns only on the
// absent or unverifiable paths; a live overview answers on the first query. Registration can
// trail the bulk hydration by a beat, so "hydrated but absent" gets one 2 s grace recheck.
export const STORE_WAIT_MS = 30_000;
const STORE_POLL_MS = 1_000;
export const STORE_GRACE_MS = 2_000;

/** True if a remembered appId still maps to a live Steam shortcut.
 *
 *  FALSE is the dangerous verdict: it sends the caller to AddShortcut, so a wrong "deleted"
 *  mints a duplicate library entry. Absent is believed only once the store is hydrated; past
 *  the budget the answer is true, because a false "alive" merely no-ops Set-calls until the
 *  next ask, while a false "dead" duplicates forever. */
export async function shortcutStillExists(appId: number): Promise<boolean> {
  if (queryShortcutAlive(appId) === true) {
    return true;
  }
  // Race the init signal against the same budget the poll loop gets: a signal that never
  // resolves must not wedge the guard (the single-flight ensure would stay occupied forever).
  await Promise.race([waitForServicesInitialized(), sleep(STORE_WAIT_MS)]);
  for (let waited = 0; waited < STORE_WAIT_MS; waited += STORE_POLL_MS) {
    if (queryShortcutAlive(appId) === true) {
      return true;
    }
    if (appStoreHydrated() === true) {
      await sleep(STORE_GRACE_MS);
      return queryShortcutAlive(appId) !== false; // null = unverifiable → reuse
    }
    await sleep(STORE_POLL_MS);
  }
  return true; // store never became inspectable — reusing beats duplicating
}
