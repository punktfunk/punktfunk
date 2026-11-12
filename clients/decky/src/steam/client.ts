// The Steam internals the shortcut code drives. `SteamClient`, `appStore`, `collectionStore` and
// `App` are globals Steam injects into its CEF context, typed by @decky/ui only in part, so each
// module that calls one declares it against the slice typed here. Everything optional is Steam
// API drift the caller must survive.
import type { CompatApps } from "./compat";

export interface SteamClientGlobal {
  Apps: CompatApps & {
    AddShortcut(
      name: string,
      exePath: string,
      startDir: string,
      launchOptions: string,
    ): Promise<number>;
    SetShortcutName(appId: number, name: string): void;
    SetShortcutExe(appId: number, exe: string): void;
    SetShortcutStartDir(appId: number, dir: string): void;
    SetShortcutIcon(appId: number, iconPath: string): void;
    SetAppLaunchOptions(appId: number, options: string): void;
    // assetType: 0 = grid (portrait capsule), 1 = hero, 2 = logo, 3 = wide grid.
    SetCustomArtworkForApp(
      appId: number,
      base64Image: string,
      imageType: string,
      assetType: number,
    ): Promise<unknown>;
    RunGame(gameId: string, _unused: string, _i: number, _j: number): void;
    TerminateApp(gameId: string, _b: boolean): void;
    RemoveShortcut(appId: number): void;
  };
  GameSessions: {
    // `unAppID` is the 32-bit appid — for a non-Steam shortcut, the one AddShortcut returned.
    RegisterForAppLifetimeNotifications(
      callback: (n: { unAppID: number; nInstanceID: number; bRunning: boolean }) => void,
    ): { unregister(): void };
  };
}

// `SetAppsAsHidden` looks the app up in appStore, which registers a fresh shortcut a moment after
// AddShortcut; called at once it throws on a null overview. Visibility is therefore best-effort
// and deferred, never launch-blocking.
export type CollectionStoreGlobal =
  | { SetAppsAsHidden?: (appIds: number[], hidden: boolean) => void }
  | undefined;

// Every registered app and shortcut by appId. `GetAppOverviewByAppID` reads the store's own
// state through `this`, so it is only ever called as a method on appStore.
export type AppStoreGlobal =
  | {
      GetAppOverviewByAppID?: (appId: number) => SteamAppOverviewLike | null;
      allApps?: SteamAppOverviewLike[];
    }
  | undefined;

// The overview fields read here. Overviews are MobX observables: assigning a field re-sorts
// whatever shelf reads it.
export interface SteamAppOverviewLike {
  appid?: number;
  display_name?: string;
  /** Unix seconds; what the Deck's Recent shelf sorts by. */
  rt_last_time_locally_played?: number;
  /** The icon Steam embeds for a shortcut when it loads shortcuts at startup. */
  icon_data?: string;
  icon_data_format?: string;
  BIsShortcut?: () => boolean;
}

// `WaitForServicesInitialized` resolves once the client's app services are up. That alone does
// not mean the overview map is populated; see `appStoreHydrated`.
export type AppGlobal = { WaitForServicesInitialized?: () => Promise<boolean> } | undefined;

/** The 64-bit gameid RunGame and TerminateApp want for a 32-bit non-Steam shortcut appId: the
 *  standard non-Steam-game encoding, `appid << 32 | 0x02000000`. */
export function gameIdFromAppId(appId: number): string {
  return ((BigInt(appId) << 32n) | 0x02000000n).toString();
}
