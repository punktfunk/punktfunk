// The host rows, ONE store for both consumers. Steam's game page is not a child of the QAM panel
// and needs the same rows (plus each host's library) before the panel has ever been opened, so
// the scan lives at module level and both subscribe.
import { useEffect } from "react";
import { discover, hosts as listHosts } from "../backend";
import { signal, useSignal } from "../lib/signal";
import { refreshLibraries } from "./catalog";
import { type HostView, mergeHosts } from "./model";

export interface HostStore {
  views: HostView[];
  scanning: boolean;
  /**
   * Why the list is empty, when it is empty for a reason other than an empty LAN. Rendering
   * any of these as "No hosts yet" would blame the user's network for the plugin's problem:
   *   "client-outdated"    — the installed client predates `punktfunk discover`
   *   "client-unavailable" — there is no client installed at all
   *   "list-failed"        — the refresh itself blew up (backend down, call threw)
   */
  problem: string | null;
  /** When the last scan landed (ms since epoch); 0 = never. */
  scannedAt: number;
}

const hostStore = signal<HostStore>({ views: [], scanning: false, problem: null, scannedAt: 0 });

function setHostStore(patch: Partial<HostStore>): void {
  hostStore.set({ ...hostStore.get(), ...patch });
}

export const getHostStore = hostStore.get;
export const subscribeHosts = hostStore.subscribe;

async function doRefreshHosts(): Promise<void> {
  setHostStore({ scanning: true });
  try {
    // Both in flight at once: the browse is time-bounded and the probe is network-bound, so
    // running them in sequence would cost the sum of two waits for no benefit.
    const [d, s] = await Promise.all([discover(), listHosts()]);
    // Both calls run the same binary, so they fail the same way; take whichever answered.
    const problem =
      d.error === "client-unavailable" || s.error === "client-unavailable"
        ? "client-unavailable"
        : d.error === "client-outdated" || s.error === "client-outdated"
          ? "client-outdated"
          : null;
    const views = mergeHosts(s.hosts ?? [], d.hosts ?? []);
    setHostStore({ views, problem, scannedAt: Date.now() });
    // The libraries ride the same refresh but never gate the rows: the panel draws the moment
    // the scan lands, and the game page fills in as each host answers.
    void refreshLibraries(views);
  } catch (e) {
    // Inline, not a toast: the panel rescans on every QAM open, so a toast would repeat on each
    // open while the backend is unhappy. The row sits beside the Refresh button that retries.
    console.warn("punktfunk: host list refresh failed", e);
    setHostStore({ problem: "list-failed" });
  } finally {
    setHostStore({ scanning: false });
  }
}

// Single-flight: a QAM open racing a game-page mount shares one scan instead of running the
// same two subprocesses twice.
let scanInFlight: Promise<void> | null = null;

/** Rescan now: mDNS browse + saved-host probe, then every paired host's library. */
export function refreshHosts(): Promise<void> {
  scanInFlight ??= doRefreshHosts().finally(() => {
    scanInFlight = null;
  });
  return scanInFlight;
}

/** Rescan only when the last scan is older than `maxAgeMs`; a fresh one stands as is. */
export function refreshHostsIfStale(maxAgeMs: number): Promise<void> {
  if (Date.now() - hostStore.get().scannedAt < maxAgeMs) {
    return Promise.resolve();
  }
  return refreshHosts();
}

/** The store as React state — re-renders on every scan. */
export function useHostStore(): HostStore {
  return useSignal(hostStore);
}

/** The QAM panel's view: the rows, plus a scan on every mount (the panel remounts per open). */
export function useHosts() {
  const { views, scanning, problem } = useHostStore();
  useEffect(() => {
    void refreshHosts();
  }, []);
  return { views, scanning, problem, refresh: refreshHosts };
}
