// The two launch actions behind the panel's rows and the game page's Stream: start the stream
// through Steam (steam/launch.ts), close the menus on success, toast on failure.
import { Navigation } from "@decky/ui";
import type { HostView } from "./hosts/model";
import { t } from "./i18n";
import { diag } from "./lib/diag";
import { toast } from "./lib/toast";
import { launchGameStream, launchStream } from "./steam/launch";
import type { LaunchOpts } from "./steam/launch-options";

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
  failedAs?: { preset: string } | { profile: string },
): Promise<void> {
  try {
    await launchStream(v.ref, opts);
    // No success toast: the QAM closes and Steam's own launch UI takes over, so a toast would
    // sit on the starting stream. Failure toasts, since the QAM may already be closed.
    Navigation.CloseSideMenus();
  } catch (e) {
    diag(`launch failed: ${e}`);
    const body = !failedAs
      ? t("decky_launch_failed")
      : "preset" in failedAs
        ? t("decky_launch_failed_preset", { preset: failedAs.preset })
        : t("decky_launch_failed_named", { name: failedAs.profile });
    toast(body);
  }
}

/**
 * Stream a Steam title from its own page. Same rules as `startStream`, under the per-game
 * shortcut that wears the game's name and art (steam/game-shortcuts.ts). `title` and `iconHash`
 * are Steam's own overview fields for the game.
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
    diag(`launch failed: ${e}`);
    toast(t("decky_launch_failed_named", { name: title }));
  }
}
