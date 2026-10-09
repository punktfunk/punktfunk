// Shared state hooks + user actions for the QAM panel.
import { toaster } from "@decky/api";
import { Navigation } from "@decky/ui";
import { useCallback, useEffect, useState } from "react";
import {
  checkUpdate,
  discover,
  hosts as listHosts,
  updateClient,
  UpdateInfo,
} from "./backend";
import { refreshLibraries } from "./catalog";
import { type HostView, mergeHosts } from "./hosts";
import { LaunchOpts, launchGameStream, launchStream } from "./steam";

export const DOCS_URL = "https://docs.punktfunk.unom.io/docs/steam-deck";

// Decky Loader exposes its already-authenticated WSRouter as a global. This is NOT part of
// @decky/api (it's a loader internal), so we treat it as optional and guard every use — on a
// loader without it we fall back to manual "Install Plugin from URL". We use it to drive
// Decky's own privileged install path (the root loader does the download + SHA-256 verify +
// extract + hot-reload), which is the only way a plugin can update itself: ~/homebrew/plugins
// is root-owned, so our unprivileged backend can't swap its own files.
declare global {
  interface Window {
    DeckyBackend?: {
      callable: (route: string) => (...args: unknown[]) => Promise<unknown>;
    };
  }
}

// PluginInstallType.UPDATE in decky-loader's browser.py (INSTALL=0/REINSTALL=1/UPDATE=2/…).
const INSTALL_TYPE_UPDATE = 2;

// ----------------------------------------------------------------------------------------
// Hosts — ONE store for both consumers. The QAM panel renders the rows; Steam's game page is
// not a child of the panel and needs the same rows (plus each host's library) before the panel
// has ever been opened, so the scan lives at module level and both subscribe.
// ----------------------------------------------------------------------------------------
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

let hostStore: HostStore = { views: [], scanning: false, problem: null, scannedAt: 0 };
const hostListeners = new Set<() => void>();

function setHostStore(patch: Partial<HostStore>): void {
  hostStore = { ...hostStore, ...patch };
  for (const listener of hostListeners) {
    listener();
  }
}

export function getHostStore(): HostStore {
  return hostStore;
}

export function subscribeHosts(listener: () => void): () => void {
  hostListeners.add(listener);
  return () => {
    hostListeners.delete(listener);
  };
}

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
    // Inline, not a toast: the panel remounts (and refreshes) on every QAM open, so while
    // the backend is unhappy a toast here nagged on each open. The panel row also sits next
    // to the Refresh button that retries it, which is where the eyes already are.
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
  if (Date.now() - hostStore.scannedAt < maxAgeMs) {
    return Promise.resolve();
  }
  return refreshHosts();
}

/** The store as React state — re-renders on every scan. */
export function useHostStore(): HostStore {
  const [state, setState] = useState(hostStore);
  useEffect(() => {
    const unsubscribe = subscribeHosts(() => setState(hostStore));
    setState(hostStore); // a scan that landed between render and subscribe
    return unsubscribe;
  }, []);
  return state;
}

/** The QAM panel's view: the rows, plus a scan on every mount (the panel remounts per open). */
export function useHosts() {
  const { views, scanning, problem } = useHostStore();
  useEffect(() => {
    void refreshHosts();
  }, []);
  return { views, scanning, problem, refresh: refreshHosts };
}

// ----------------------------------------------------------------------------------------
// Self-update — checks our registry on mount (the backend caches for 30 min + is non-fatal
// offline); `check(true)` bypasses the cache for the explicit "Check for updates" button.
// ----------------------------------------------------------------------------------------
export function useUpdate() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);

  const check = useCallback(async (force: boolean): Promise<UpdateInfo | null> => {
    setChecking(true);
    try {
      const res = await checkUpdate(force);
      setInfo(res);
      return res;
    } catch {
      return null;
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check(false);
  }, [check]);

  return { info, checking, check };
}

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

/** The explicit "Check for updates" action — always ends in a toast so the tap has feedback. */
export async function checkForUpdatesNow(
  check: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  const res = await check(true);
  let body: string;
  if (!res || res.error === "fetch-failed") {
    body = "Couldn’t reach the update server — are you online?";
  } else if (hasUpdate(res)) {
    const parts: string[] = [];
    if (res.update_available) parts.push(`plugin v${res.current} → v${res.latest}`);
    if (res.client_update_available) {
      parts.push(res.client_latest ? `client ${res.client_latest}` : "client");
    }
    body = `Update available: ${parts.join(" + ")}.`;
    if (clientUpdateIsManualOnly(res)) {
      // Say the honest thing up front rather than letting the user find out at the button.
      body += " The client updates outside Punktfunk on this install.";
    }
  } else if (res.client_error) {
    // A failed CLIENT check must never read as "up to date" — that is the one wrong answer.
    body =
      res.client_error === "client-outdated"
        ? "Couldn’t check the client — it predates update checks. Update it once by hand."
        : "Couldn’t check the client for updates";
  } else if (res.error === "update-channel-unknown") {
    body = "Development build — plugin updates are disabled; the client is up to date.";
  } else {
    body = `You’re up to date (plugin v${res.current}).`;
  }
  toaster.toast({ title: "Punktfunk", body });
}

/** One line of user-facing copy for whatever `updateClient()` came back with. */
function clientUpdateResultBody(r: Awaited<ReturnType<typeof updateClient>>): string {
  if (r.ok) {
    if (r.staged) return "Client updated — reboot to finish.";
    return r.updated ? "Client updated to the latest version." : "Client is already up to date.";
  }
  // "manual" is not a failure: the box simply can't install it, and `command` says how.
  if (r.error === "manual") {
    return r.command
      ? `This client updates outside Punktfunk. Run: ${r.command}`
      : "This client updates outside Punktfunk — use the way you installed it.";
  }
  if (r.error === "timeout") return "Client update timed out — check the box and try again";
  if (r.error === "client-unavailable")
    return "Couldn’t reach the client to update it — is it still installed?";
  const why = r.detail || r.error;
  return `Client update failed${why ? ` — ${why}` : ""}`;
}

/**
 * Apply whichever updates are pending.
 *
 * The CLIENT goes first and is awaited, by whichever route its install supports — a user-scope
 * `flatpak update`, or the packaged root helper via `punktfunk-client --apply-update`. An
 * install neither can serve is not attempted at all: the user gets the command in a toast,
 * because a button that can only fail teaches nothing.
 *
 * The PLUGIN goes last and is fire-and-forget: Decky's install RPC reinstalls and reloads the
 * plugin, tearing this panel down before any result could arrive. `check` (when passed)
 * refreshes the panel state after a client-only update so the "Update available" button clears.
 */
export async function applyUpdate(
  info: UpdateInfo,
  check?: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  if (info.client_update_available && clientUpdateIsOneTap(info)) {
    toaster.toast({
      title: "Punktfunk",
      // A package-manager run is not instant; say so before the wait, not after.
      body:
        info.client_applier === "helper"
          ? "Updating the client — this can take a few minutes…"
          : "Updating the client…",
    });
    try {
      const r = await updateClient();
      toaster.toast({ title: "Punktfunk", body: clientUpdateResultBody(r) });
    } catch {
      toaster.toast({ title: "Punktfunk", body: "Client update failed" });
    }
  } else if (info.client_update_available) {
    // Nothing here can install it — hand over the one line that does, rather than a button
    // that would fail. `client_opt_in` wins when joining the group is what's missing, since
    // that is the step that turns this into a one-tap update from then on.
    const line = info.client_opt_in || info.client_command;
    toaster.toast({
      title: "Punktfunk",
      body: line
        ? `Client update available (${info.client_latest}). Run: ${line}`
        : `A newer client (${info.client_latest}) is available — update it the way you installed it.`,
      duration: 12_000,
    });
  }

  if (info.update_available) {
    // The manifest names the channel's alias zip, which every publish replaces. Read it again
    // now, past the backend's 30 min cache, so the hash is the one beside the zip Decky fetches.
    const fresh = await checkUpdate(true).catch(() => null);
    const plugin = fresh?.update_available ? fresh : info;
    try {
      const backend = window.DeckyBackend;
      if (backend?.callable) {
        // Fire-and-forget: the loader reinstalls + reloads THIS plugin, tearing the panel down
        // before any result could arrive — so never await it. Decky shows its own confirm prompt.
        void backend.callable("utilities/install_plugin")(
          plugin.artifact,
          // The name Decky uninstalls before extracting the new zip — it locates the folder by
          // matching plugin.json "name", so this must equal THIS build's plugin.json name (the
          // brand-cased one), not the lowercase on-disk dir.
          "Punktfunk",
          plugin.latest,
          plugin.hash,
          INSTALL_TYPE_UPDATE,
        );
        toaster.toast({
          title: "Punktfunk",
          // Decky's installer also phones the plugin store first, which can hang on some
          // networks before the actual install proceeds — set expectations.
          body: `Updating the plugin to v${plugin.latest} — confirm Decky’s prompt. This can take a couple of minutes.`,
        });
        return;
      }
    } catch {
      // fall through to the manual path
    }
    toaster.toast({
      title: "Punktfunk",
      body: "Update the plugin from Decky → Developer → Install Plugin from URL.",
    });
    return;
  }

  // Client-only update (no plugin reinstall): refresh so the button clears.
  if (check) void check(true);
}

// ----------------------------------------------------------------------------------------
// Stream launch — via the hidden Steam shortcut (see steam.ts for why it can't be direct).
// ----------------------------------------------------------------------------------------

/**
 * Stream this host. `opts.presetId` streams one of its pinned cards; `opts.requestAccess`
 * runs the supervised launch that waits for the host's operator to approve this Deck.
 *
 * The host is named by REFERENCE (`v.ref`), never by value — no resolution, bitrate or codec
 * ever rides the launch path, which is the same rule the deep-link grammar enforces.
 */
export async function startStream(
  v: HostView,
  opts: LaunchOpts = {},
  label?: string,
): Promise<void> {
  try {
    await launchStream(v.ref, opts);
    // No success toast: the user just pressed the button that names this host/card, the QAM
    // closes, and Steam's own launch UI takes over — a toast here fired on EVERY launch and
    // then sat on top of the starting stream. Failure still toasts (the QAM may already be
    // closed, so inline error state would go unseen).
    Navigation.CloseSideMenus();
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed${label ? ` (${label})` : ""}: ${e}` });
  }
}

/**
 * Stream a Steam title from its own page. Same rules as `startStream`, under the per-game
 * shortcut that wears the game's name and art (see steam.ts). `title` and `iconHash` are
 * Steam's own overview fields for the game.
 */
export async function startGameStream(
  v: HostView,
  steamAppId: number,
  title: string,
  iconHash: string,
): Promise<void> {
  try {
    await launchGameStream(v.ref, steamAppId, title, iconHash);
    Navigation.CloseSideMenus();
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed (${title}): ${e}` });
  }
}
