// The QAM panel's one-tap actions. Each ends in a toast, or in closing the menus, so the tap has
// feedback even when the panel is gone.
import { Navigation } from "@decky/ui";
import {
  endGame,
  type HostAction,
  hostAction,
  killStream,
  pickScreen,
  runAction,
  type ScreenRow,
  type ScreensResult,
  wake,
} from "../backend";
import type { HostView } from "../hosts/model";
import { refreshHosts } from "../hosts/store";
import { t } from "../i18n";
import { toast } from "../lib/toast";
import { removeGameShortcuts } from "../steam/game-shortcuts";
import { lastGameStream, stopStream } from "../steam/launch";
import { recreateShortcuts } from "../steam/shortcuts";

// Recovery action for "the Punktfunk library entry vanished" — recreates the visible shortcut
// and sweeps duplicate entries. Self-heal waits for the next mount; this works in-session.
// Always ends in a toast so the tap has feedback.
export async function recreatePunktfunkShortcut(): Promise<void> {
  const { appId, removedDuplicates } = await recreateShortcuts();
  toast(
    appId == null
      ? t("decky_shortcut_create_failed")
      : removedDuplicates > 0
        ? t("decky_shortcut_restored_dupes", { count: removedDuplicates })
        : t("decky_shortcut_restored"),
  );
}

/** Delete the hidden per-game entries that game-page streams minted. Each comes back, with its
 *  art, on the next Stream tap for that game — so this is tidying, never a loss. */
export function removeGamePageShortcuts(): void {
  const removed = removeGameShortcuts();
  toast(
    removed === 0
      ? t("decky_game_shortcuts_none")
      : t("decky_game_shortcuts_removed", { count: removed }),
  );
}

/** Force-stop a wedged stream: end Steam's "game", then make sure the client itself is gone. */
export async function forceStop(): Promise<void> {
  stopStream();
  try {
    await killStream();
  } catch {
    /* best-effort — the TerminateApp above is usually enough */
  }
  toast(t("decky_force_stopped"));
}

// Press a host system button (guide/QAM) on the running stream, then hand the screen back
// to it — closing the local menus is what lets the HOST's overlay show through. The raw
// Steam/··· presses stay on the Deck by default (both overlays would open at once), so this
// is the panel route to the host's menus; holding Select is the controller route.
export async function pressHost(action: "guide" | "qam"): Promise<void> {
  const r = await hostAction(action).catch(() => ({ ok: false as const, error: "backend" }));
  if (r.ok) {
    Navigation.CloseSideMenus();
  } else {
    toast(r.error === "no-stream" ? t("decky_no_stream") : t("decky_stream_unreachable"));
  }
}

/** Wake a host and wait for it (up to 90 s), then rescan so its row reads online. */
export async function wakeHost(host: HostView): Promise<void> {
  toast(t("dial_waking", { host: host.name }));
  const r = await wake(host.ref).catch(() => null);
  if (r?.ok) {
    toast(t("decky_host_awake", { host: host.name }));
    void refreshHosts();
  } else {
    toast(t("dial_wake_parked", { host: host.name }));
  }
}

/** This plugin's words for a host action it knows; the CLI's for any other. */
export function actionLabel(a: HostAction): string {
  switch (a.id) {
    case "power.sleep":
      return t("host_power_sleep");
    case "power.reboot":
      return t("host_power_reboot");
    case "power.shutdown":
      return t("host_power_shutdown");
    case "host.restart":
      return t("host_power_restart_service");
    case "display.next":
      return t("hostaction_display_next");
    default:
      return a.label;
  }
}

/** Run one of the host's actions; the outcome is a toast. The caller confirms a danger one. */
export async function runHostAction(host: HostView, a: HostAction): Promise<void> {
  const action = actionLabel(a);
  if (!a.available) {
    toast(a.unavailable_reason || t("hostcard_action_unavailable_body", { action }));
    return;
  }
  const r = await runAction(host.ref, a.id).catch(() => null);
  // The CLI's sentence says why it failed; its words follow the Deck's locale, not Steam's.
  toast(
    r?.ok
      ? t("hostcard_action_sent_body", { host: host.name, action })
      : r?.notice || t("hostcard_action_unavailable_body", { action }),
  );
}

/** This plugin's words for a Screen row: a role, an older host's own screen, else the host's
 *  name for the monitor. */
export function screenLabel(row: ScreenRow): string {
  if (row.kind === "monitor") {
    return row.label || row.id;
  }
  switch (row.id) {
    case "only":
      return t("screen_role_only");
    case "main":
      return t("screen_role_main");
    case "second":
      return t("screen_role_second");
    default:
      return t("screen_own");
  }
}

/** Make a Screen row this Deck's screen on the host; a toast names it, or says why not. The
 *  rows the host answers with after the pick, or null when it failed. */
export async function pickHostScreen(host: HostView, row: ScreenRow): Promise<ScreensResult | null> {
  const label = screenLabel(row);
  const r = await pickScreen(host.ref, row.id).catch(() => null);
  if (r?.ok) {
    toast(row.kind === "monitor" ? t("screen_streams", { screen: label }) : label);
    return r;
  }
  toast(r?.notice || t("hostcard_action_unavailable_body", { action: label }));
  return null;
}

/** End the title the last game-page stream launched, then the stream — only once the host says
 *  the game is gone. A refusal keeps the stream and shows the host's sentence. */
export async function endStreamedGame(): Promise<void> {
  const last = lastGameStream();
  if (!last) {
    return;
  }
  const r = await endGame(last.ref, last.gameId).catch(() => null);
  if (r?.ok) {
    stopStream();
  }
  // The CLI's own sentence leads when it sent one; its words are English.
  const body =
    r === null
      ? t("decky_client_unreachable_end")
      : r.notice ||
        (r.ok
          ? t("decky_game_ended", { title: last.title })
          : r.error === "unresolved"
            ? t("decky_end_game_unresolved")
            : t("decky_end_game_failed"));
  toast(body);
}
