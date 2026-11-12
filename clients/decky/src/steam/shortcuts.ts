// The two fixed Punktfunk shortcuts. gamescope focuses and fullscreens only the window tree Steam
// launched, so a stream starts as a non-Steam shortcut whose exe is `/bin/sh` running the wrapper
// (bin/punktfunkrun.sh), via RunGame. Both share the name "Punktfunk", so Steam keys them to one
// controller layout:
//
//   • STREAM — hidden, stateful: its launch options carry the host reference and per-stream
//     extras, rewritten at every launch, so one shortcut serves every host.
//   • GAMEPAD UI — visible, stateless: PF_BROWSE opens the client's console home. The library
//     entry the user opens directly.
//
// A stream from a game's own page runs under a third kind; see game-shortcuts.ts.
import { applyControllerConfig, runnerInfo } from "../backend";
import { diag } from "../lib/diag";
import { read, write } from "../lib/storage";
import { applyArtwork } from "./art";
import type { AppStoreGlobal, CollectionStoreGlobal, SteamClientGlobal } from "./client";
import { compatToolOf } from "./compat";
import { artKey, CONFIG_KEY, recall, remember, STORAGE_KEY_STREAM, STORAGE_KEY_UI } from "./keys";
import { safeClientBin } from "./launch-options";
import { shortcutStillExists } from "./readiness";

declare const SteamClient: SteamClientGlobal;
declare const appStore: AppStoreGlobal;
declare const collectionStore: CollectionStoreGlobal;

/** Set a shortcut's library visibility (best-effort, deferred — the overview registers a moment
 *  after AddShortcut). Hides the stateful stream shortcut; keeps the gamepad-UI one visible. */
function setShortcutHidden(appId: number, hidden: boolean): void {
  const attempt = () => {
    try {
      collectionStore?.SetAppsAsHidden?.([appId], hidden);
    } catch {
      /* overview not registered yet, or the API changed — cosmetic, ignore */
    }
  };
  attempt(); // succeeds immediately for an already-registered (reused) shortcut
  setTimeout(attempt, 2500); // fresh shortcut: retry once its app overview lands
};

// User-visible in the overlay and the library, so brand-cased. Both fixed roles share it, so Steam
// keys them to the same controller layout (the configset key is the lowercase name).
const SHORTCUT_NAME = "Punktfunk";

/** A "Punktfunk" shortcut to ADOPT instead of minting a new library entry, for a lost appId or a
 *  pile of duplicates. Only overviews Steam says are shortcuts qualify, and the other role's id
 *  is excluded so the two roles never collapse onto one shortcut. The caller rewrites exe, dir,
 *  options and visibility, so any of them serves. */
function findAdoptableShortcut(excludeAppId: number | null): number | null {
  try {
    if (typeof appStore === "undefined" || !Array.isArray(appStore?.allApps)) {
      return null;
    }
    for (const app of appStore.allApps) {
      if (
        app?.display_name === SHORTCUT_NAME &&
        typeof app.appid === "number" &&
        app.appid !== excludeAppId &&
        app.BIsShortcut?.() === true
      ) {
        return app.appid;
      }
    }
  } catch {
    /* Steam internals drifted — AddShortcut is the fallback */
  }
  return null;
}

/** Remove every "Punktfunk" shortcut beyond the two remembered role ids. Reachable ONLY from the
 *  user-pressed recreate button, never from mount: deleting library entries at boot is a bigger
 *  hazard than the duplicates it would tidy. Returns how many entries were removed. */
function removeDuplicateShortcuts(): number {
  let removed = 0;
  try {
    if (typeof appStore === "undefined" || !Array.isArray(appStore?.allApps)) {
      return 0;
    }
    const keep = [recall(STORAGE_KEY_STREAM), recall(STORAGE_KEY_UI)];
    // Snapshot before removing — RemoveShortcut mutates the store's list under the iteration.
    const surplus = appStore.allApps.filter(
      (app) =>
        app?.display_name === SHORTCUT_NAME &&
        typeof app.appid === "number" &&
        !keep.includes(app.appid) &&
        app.BIsShortcut?.() === true,
    );
    for (const app of surplus) {
      SteamClient.Apps.RemoveShortcut(app.appid as number);
      write(artKey(app.appid as number), null);
      removed++;
    }
  } catch (e) {
    console.warn("punktfunk: duplicate-shortcut sweep incomplete", e);
  }
  return removed;
}

// The exe is /bin/sh with the wrapper as an argument, never the script itself: Decky's zip
// extraction drops the exec bit, and the root-owned plugins dir cannot be chmodded back by the
// unprivileged backend.
const SHELL = "/bin/sh";

// Gated in storage so Steam's config dir is not rewritten on every launch; bump after the
// shipped .vdf changes.
const CONFIG_VERSION = 1;

/** Point shortcut `name` at the touch layout unless `marker` already records CONFIG_VERSION. */
async function configOnce(marker: string, name: string): Promise<void> {
  try {
    if (read(marker) === `${CONFIG_VERSION}`) {
      return;
    }
    const r = await applyControllerConfig(name);
    // `ok` alone isn't done: with zero account configset dirs (fresh Steam) the backend
    // succeeds without pointing any account at the template — keep retrying until one lands.
    if (r?.ok && (r.applied ?? []).some((a) => a.startsWith("configset:"))) {
      write(marker, `${CONFIG_VERSION}`);
    } else {
      console.warn("punktfunk: controller config not fully applied", r);
    }
  } catch (e) {
    console.warn("punktfunk: controller config not applied", e);
  }
}

export interface Ensured {
  appId: number;
  runner: string;
  clientBin: string;
}

/** What one shortcut role varies; `ensureShortcut` runs the rest. */
export interface ShortcutRole {
  /** Storage key of the role's appId. Also the single-flight key. */
  key: string;
  /** Library name. Steam files the controller layout under it too. */
  name: string;
  hidden: boolean;
  /** The other fixed role's key: a same-named shortcut it does not hold is adopted before a new
   *  one is minted. Absent for a per-game shortcut, whose title could be the user's own entry. */
  adoptExcept?: string;
  /** Controller-config marker, or null when this name gets no layout. */
  configMarker: string | null;
  /** Launch options set at every ensure; absent for the stream roles, set per launch instead. */
  launchOptions?: (runner: string, clientBin: string | undefined) => string;
  /** The art marker a freshly minted appId must not inherit from a deleted shortcut. */
  artKey: (appId: number) => string;
  /** Apply the role's artwork, fire-and-forget. */
  dress: (appId: number) => void;
}

/**
 * Ensure a role's shortcut exists, runs natively and points at the current plugin dir: reuse the
 * remembered appId only while the shortcut is alive (a stale id must fall through, not be
 * repointed), else adopt or mint one. Then rewrite exe, start dir, name, visibility and art.
 * Throws when the launch wrapper is missing.
 *
 * Any `SpecifyCompatTool` call, even an empty name, marks a shortcut as a compat app until Steam
 * restarts, and it then runs in the player's global default, which is Proton. Only a shortcut
 * that never had a tool runs natively, so a marked one is replaced, never cleared.
 */
async function ensureShortcutOnce(role: ShortcutRole): Promise<Ensured> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }
  const startDir = info.runner.replace(/\/[^/]*$/, ""); // the plugin's bin/ dir
  if (role.configMarker) {
    void configOnce(role.configMarker, role.name); // never blocks the launch
  }
  const mint = async (): Promise<number> => {
    const id = await SteamClient.Apps.AddShortcut(role.name, SHELL, startDir, "");
    write(role.artKey(id), null); // a recycled appId must not skip its art
    remember(role.key, id);
    return id;
  };
  const remembered = recall(role.key);
  let appId =
    remembered != null && (await shortcutStillExists(remembered)) ? remembered : null;
  if (appId == null) {
    appId = role.adoptExcept ? findAdoptableShortcut(recall(role.adoptExcept)) : null;
    if (appId == null) {
      appId = await mint();
    } else {
      remember(role.key, appId);
    }
  }
  const tool = await compatToolOf(SteamClient.Apps, appId);
  if (tool) {
    diag(`shortcut ${appId}: Steam would run it in ${tool}, replacing it`);
    SteamClient.Apps.RemoveShortcut(appId);
    appId = await mint();
  }
  SteamClient.Apps.SetShortcutExe(appId, SHELL);
  SteamClient.Apps.SetShortcutStartDir(appId, startDir);
  SteamClient.Apps.SetShortcutName(appId, role.name);
  if (role.launchOptions) {
    SteamClient.Apps.SetAppLaunchOptions(appId, role.launchOptions(info.runner, info.client_bin));
  }
  setShortcutHidden(appId, role.hidden);
  role.dress(appId);
  return { appId, runner: info.runner, clientBin: info.client_bin ?? "" };
}

// Concurrent ensures of one role share one run: two racing past the liveness check would each
// AddShortcut, a duplicate entry, and the store-readiness wait makes that window real.
// Sequential calls still re-run, so per-launch repointing is kept.
const ensuring = new Map<string, Promise<Ensured>>();
export function ensureShortcut(role: ShortcutRole): Promise<Ensured> {
  let p = ensuring.get(role.key);
  if (!p) {
    p = ensureShortcutOnce(role).finally(() => ensuring.delete(role.key));
    ensuring.set(role.key, p);
  }
  return p;
}

/** The STREAM shortcut (hidden, stateful): its launch options are rewritten per stream. */
const STREAM_ROLE: ShortcutRole = {
  key: STORAGE_KEY_STREAM,
  name: SHORTCUT_NAME,
  hidden: true,
  adoptExcept: STORAGE_KEY_UI,
  configMarker: CONFIG_KEY,
  artKey,
  dress: (appId) => void applyArtwork(appId),
};

/**
 * The GAMEPAD-UI shortcut (visible, stateless): the library-facing "Punktfunk" entry that opens
 * the client's console home. PF_BROWSE → the wrapper runs the session's `--browse --fullscreen`;
 * %command% expands to the shortcut exe (/bin/sh) with the wrapper behind it as an arg.
 * PF_CLIENT_BIN only for a resolved NATIVE client, else the wrapper's flatpak default stands.
 */
const UI_ROLE: ShortcutRole = {
  key: STORAGE_KEY_UI,
  name: SHORTCUT_NAME,
  hidden: false,
  adoptExcept: STORAGE_KEY_STREAM,
  configMarker: CONFIG_KEY,
  launchOptions: (runner, clientBin) =>
    `${safeClientBin(clientBin) ? `PF_CLIENT_BIN=${clientBin} ` : ""}PF_BROWSE=1 %command% "${runner}"`,
  artKey,
  dress: (appId) => void applyArtwork(appId),
};

export const ensureStreamShortcut = (): Promise<Ensured> => ensureShortcut(STREAM_ROLE);

/** Ensure the visible library entry — call on plugin mount so it always exists and stays
 *  repointed to the current plugin dir. Best-effort: null on any failure. */
export async function ensureGamepadUiShortcut(): Promise<number | null> {
  try {
    return (await ensureShortcut(UI_ROLE)).appId;
  } catch (e) {
    console.warn("punktfunk: gamepad-UI shortcut not ensured", e);
    return null;
  }
}

/**
 * Force the visible "Punktfunk" library entry back into existence: the recovery button, safe to
 * press anytime. Drops each remembered appId that no longer maps to a live shortcut (so it can't
 * shadow a fresh AddShortcut), re-ensures, then sweeps surplus "Punktfunk" entries. Covers a
 * shortcut deleted without a reinstall, which no mount repairs. Returns the visible appId (null
 * on failure) and how many duplicates went.
 */
export async function recreateShortcuts(): Promise<{
  appId: number | null;
  removedDuplicates: number;
}> {
  for (const key of [STORAGE_KEY_STREAM, STORAGE_KEY_UI]) {
    const id = recall(key);
    if (id != null && !(await shortcutStillExists(id))) {
      write(artKey(id), null); // stale art marker for the dead appId
      write(key, null);
    }
  }
  // The hidden stream shortcut re-registers lazily on the next launch. The sweep runs after the
  // ensure, so the kept ids are fresh, and only when it succeeded: a failed ensure's keep list
  // can't be trusted, and deleting entries a later ensure would adopt could leave none at all.
  const appId = await ensureGamepadUiShortcut();
  const removedDuplicates = appId != null ? removeDuplicateShortcuts() : 0;
  return { appId, removedDuplicates };
}
