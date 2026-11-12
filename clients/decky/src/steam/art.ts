// Shortcut artwork: the plugin's own on the two fixed roles, the game's own on a per-game
// shortcut. Cosmetic and best-effort; a marker per appId records the version applied.
import { gameArt, saveIcon, shortcutArt } from "../backend";
import { read, write } from "../lib/storage";
import type { AppStoreGlobal, SteamClientGlobal } from "./client";
import { artKey, gameArtKey } from "./keys";

declare const SteamClient: SteamClientGlobal;
declare const appStore: AppStoreGlobal;

// Bump when the shipped artwork changes; each fixed-role shortcut re-applies it once.
const ART_VERSION = 3;

/**
 * Run `apply` unless `marker` already records `version`, and record it once `apply` reports done.
 * A shortcut fresh out of AddShortcut may not be registered yet (the race setShortcutHidden
 * defers around), so a failure retries once 2.5 s later, then waits for the next call.
 */
async function artOnce(
  marker: string,
  version: number,
  apply: () => Promise<boolean>,
  isRetry = false,
): Promise<void> {
  try {
    if (read(marker) === `${version}`) {
      return;
    }
    if (await apply()) {
      write(marker, `${version}`);
    }
  } catch (e) {
    if (!isRetry) {
      setTimeout(() => void artOnce(marker, version, apply, true), 2500);
    }
    console.warn("punktfunk: shortcut artwork not applied", e);
  }
}

/** Set each present image into its slot, in order: grid, hero, logo, wide grid. */
async function setArtSlots(
  appId: number,
  images: Array<[data: string | undefined, type: string]>,
): Promise<boolean> {
  let applied = false;
  for (const [slot, [data, type]] of images.entries()) {
    if (data) {
      await SteamClient.Apps.SetCustomArtworkForApp(appId, data, type, slot);
      applied = true;
    }
  }
  return applied;
}

/** Apply the plugin's grid/hero/logo/icon to a fixed-role shortcut, once per ART_VERSION. */
export function applyArtwork(appId: number): Promise<void> {
  return artOnce(artKey(appId), ART_VERSION, async () => {
    const art = await shortcutArt();
    const applied = await setArtSlots(appId, [
      [art.grid, "png"],
      [art.hero, "png"],
      [art.logo, "png"],
      [art.gridwide, "png"],
    ]);
    if (art.icon_path) {
      SteamClient.Apps.SetShortcutIcon(appId, art.icon_path);
      return true;
    }
    // Only "done" when something landed: a build whose assets/ is empty keeps retrying.
    return applied;
  });
}

// Bump when what applyGameArtwork fetches changes; each per-game shortcut re-applies it once.
const GAME_ART_VERSION = 3;

/** Steam's cached icons are JPG; a shortcut icon has to be PNG (or ICO). Decode on a canvas. */
async function toPng(base64: string, type: string): Promise<string> {
  if (type === "png") {
    return base64;
  }
  const img = new Image();
  img.src = `data:image/${type === "jpg" ? "jpeg" : type};base64,${base64}`;
  await img.decode();
  const canvas = document.createElement("canvas");
  canvas.width = img.naturalWidth || 32;
  canvas.height = img.naturalHeight || 32;
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    throw new Error("no 2d canvas");
  }
  ctx.drawImage(img, 0, 0);
  return canvas.toDataURL("image/png").split(",")[1];
}

/**
 * The icon for a per-game shortcut, two ways: the PNG file for Steam's next start (it embeds a
 * shortcut's icon into the overview only when loading shortcuts), and the bytes into the live
 * overview now, which is what the Steam menu draws this session.
 */
async function applyGameIcon(
  shortcutAppId: number,
  steamAppId: number,
  art: { icon?: string; icon_type?: string; icon_path?: string },
): Promise<boolean> {
  if (!art.icon) {
    return false;
  }
  const png = await toPng(art.icon, art.icon_type ?? "jpg");
  let path = art.icon_path ?? "";
  if (!path) {
    const saved = await saveIcon(steamAppId, png);
    path = saved.ok && saved.path ? saved.path : "";
  }
  if (path) {
    SteamClient.Apps.SetShortcutIcon(shortcutAppId, path);
  }
  try {
    const overview = appStore?.GetAppOverviewByAppID?.(shortcutAppId);
    if (overview) {
      overview.icon_data = png;
      overview.icon_data_format = "png";
    }
  } catch {
    /* the file still lands for the next start */
  }
  return !!path;
}

/** Dress a per-game shortcut in the game's own grid/hero/logo/header and icon, once per version. */
export function applyGameArtwork(
  shortcutAppId: number,
  steamAppId: number,
  iconHash: string,
): Promise<void> {
  return artOnce(gameArtKey(shortcutAppId), GAME_ART_VERSION, async () => {
    const art = await gameArt(steamAppId, iconHash);
    if (!art.ok) {
      return false;
    }
    const applied = await setArtSlots(shortcutAppId, [
      [art.grid, art.grid_type ?? "jpg"],
      [art.hero, art.hero_type ?? "jpg"],
      [art.logo, art.logo_type ?? "jpg"],
      [art.gridwide, art.gridwide_type ?? "jpg"],
    ]);
    // Done only when the icon landed too: it is the one piece the overlay shows on every
    // frame, and a gray box there must be retried on the next launch, not recorded as fine.
    const iconLanded = await applyGameIcon(shortcutAppId, steamAppId, art);
    return applied && iconLanded;
  });
}
