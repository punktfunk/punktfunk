// Start and stop streams through Steam: write the launch options onto the right shortcut, then
// RunGame it, so gamescope focuses and fullscreens the client.
import { readJson, write } from "../lib/storage";
import { gameIdFromAppId, type SteamClientGlobal } from "./client";
import { ensureGameShortcut } from "./game-shortcuts";
import {
  gameShortcutFor,
  recall,
  remember,
  STORAGE_KEY_LAST_GAME,
  STORAGE_KEY_LAST_LAUNCH,
  STORAGE_KEY_STREAM,
} from "./keys";
import { type LaunchOpts, launchOptions, validateLaunch } from "./launch-options";
import { ensureGamepadUiShortcut, ensureStreamShortcut } from "./shortcuts";

declare const SteamClient: SteamClientGlobal;

export type GameStreamRef = { ref: string; gameId: string; title: string };

/** The host and title the last game-page stream launched; `null` after a plain stream. */
export function lastGameStream(): GameStreamRef | null {
  return readJson<GameStreamRef>(STORAGE_KEY_LAST_GAME);
}

function setLastGameStream(v: GameStreamRef | null): void {
  write(STORAGE_KEY_LAST_GAME, v ? JSON.stringify(v) : null);
}

/** Launch the stateless gamepad-UI shortcut (console home) from the plugin, e.g. a QAM button. */
export async function launchGamepadUi(): Promise<void> {
  const appId = await ensureGamepadUiShortcut();
  if (appId != null) {
    SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
  }
}

/**
 * Stream `ref` fullscreen in Gaming Mode, optionally with a pinned card's preset. Encodes the
 * target into the STREAM shortcut's launch options — one hidden shortcut serves every host —
 * then RunGame. The plugin sends no wake packet: `punktfunk launch` wakes a sleeping host and
 * dials once it answers.
 */
export async function launchStream(ref: string, opts: LaunchOpts = {}): Promise<void> {
  validateLaunch(ref, opts);
  const { appId, runner, clientBin } = await ensureStreamShortcut();
  setLastGameStream(opts.gameId ? { ref, gameId: opts.gameId, title: opts.gameId } : null);
  runShortcut(appId, launchOptions(ref, runner, clientBin, opts));
}

/**
 * Stream a Steam title from its own page: the same launch as `launchStream` with the game
 * named, but under the hidden per-game shortcut so Steam shows the game running, not Punktfunk.
 * `title` and `iconHash` come from Steam's overview of the game and only dress the shortcut.
 */
export async function launchGameStream(
  ref: string,
  steamAppId: number,
  title: string,
  iconHash: string,
  opts: LaunchOpts = {},
): Promise<void> {
  const full = { ...opts, gameId: `steam:${steamAppId}` };
  validateLaunch(ref, full);
  const { appId, runner, clientBin } = await ensureGameShortcut(steamAppId, title, iconHash);
  setLastGameStream({ ref, gameId: full.gameId, title });
  runShortcut(appId, launchOptions(ref, runner, clientBin, full));
}

function runShortcut(appId: number, options: string): void {
  SteamClient.Apps.SetAppLaunchOptions(appId, options);
  remember(STORAGE_KEY_LAST_LAUNCH, appId);
  SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
}

/** Stop the running stream — whichever shortcut the last launch ran under (best-effort; the
 *  in-stream chord/back also works). */
export function stopStream(): void {
  const appId = recall(STORAGE_KEY_LAST_LAUNCH) ?? recall(STORAGE_KEY_STREAM);
  if (appId != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(appId), false);
  }
}

/** End the stream of this Steam title — Steam's Stop, for our button. */
export function stopGameStream(steamAppId: number): void {
  const shortcut = gameShortcutFor(steamAppId);
  if (shortcut != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(shortcut), false);
  }
}
