// Self-update: the check behind the panel's update row, and the install for the plugin and the
// client, which version independently.
import { useCallback, useEffect, useState } from "react";
import { checkUpdate, updateClient, type UpdateInfo } from "../backend";
import { t } from "../i18n";
import { diag } from "../lib/diag";
import { toast } from "../lib/toast";
import { type CheckKind, checkOutcome, clientUpdateIsOneTap } from "./model";

// Decky Loader's own authenticated WSRouter, a loader internal and not @decky/api, so optional
// and guarded. It drives the root loader's install (download, SHA-256 check, extract, reload):
// the only way a plugin updates itself, since the unprivileged backend can't write the
// root-owned plugins dir. Without it the user installs from a URL by hand.
declare global {
  interface Window {
    DeckyBackend?: {
      callable: (route: string) => (...args: unknown[]) => Promise<unknown>;
    };
  }
}

// PluginInstallType.UPDATE in decky-loader's browser.py (INSTALL=0/REINSTALL=1/UPDATE=2/…).
const INSTALL_TYPE_UPDATE = 2;

/** The update state, checked on mount. The backend caches the check for 30 min and survives
 *  being offline; `check(true)` bypasses the cache for the explicit "Check for updates" button. */
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

/** One toast line for what "Check for updates" found. */
function checkBody(kind: CheckKind, res: UpdateInfo | null): string {
  const current = res?.current ?? "";
  const latest = res?.latest ?? "";
  const version = res?.client_latest ?? "";
  switch (kind) {
    case "unreachable":
      return t("decky_update_server_unreachable");
    case "both":
      return t("decky_update_available_both", { current, latest, version });
    case "both-unknown":
      return t("decky_update_available_both_unknown", { current, latest });
    case "plugin":
      return t("decky_update_available_plugin", { current, latest });
    case "client":
      return t("decky_update_available_client", { version });
    case "client-unknown":
      return t("decky_update_available_client_unknown");
    case "client-outdated":
      return t("decky_update_client_outdated");
    case "client-check-failed":
      return t("decky_update_client_check_failed");
    case "dev-build":
      return t("decky_update_dev_build");
    case "up-to-date":
      return t("decky_up_to_date", { version: current });
  }
}

/** The explicit "Check for updates" action — always ends in a toast so the tap has feedback. A
 *  client this Deck can't update says so here, before the player finds out at the button. */
export async function checkForUpdatesNow(
  check: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  const res = await check(true);
  const { kind, manual } = checkOutcome(res);
  const body = checkBody(kind, res);
  toast(manual ? `${body} ${t("decky_update_client_manual_note")}` : body);
}

/** One line of user-facing copy for whatever `updateClient()` came back with. */
function clientUpdateResultBody(r: Awaited<ReturnType<typeof updateClient>>): string {
  if (r.ok) {
    if (r.staged) return t("decky_client_updated_reboot");
    return r.updated ? t("decky_client_updated") : t("decky_client_current");
  }
  // "manual" is not a failure: the box simply can't install it, and `command` says how.
  if (r.error === "manual") {
    return r.command
      ? t("decky_client_manual_command", { command: r.command })
      : t("decky_client_manual");
  }
  if (r.error === "timeout") return t("decky_client_update_timeout");
  if (r.error === "client-unavailable") return t("decky_client_update_unreachable");
  // The client's own line is English; it goes to the diagnostics, not to the toast.
  diag(`client update failed: ${r.detail || r.error}`);
  return t("decky_client_update_failed");
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
    // A package-manager run is not instant; say so before the wait, not after.
    toast(
      info.client_applier === "helper"
        ? t("decky_client_updating_slow")
        : t("decky_client_updating"),
    );
    try {
      const r = await updateClient();
      toast(clientUpdateResultBody(r));
    } catch {
      toast(t("decky_client_update_failed"));
    }
  } else if (info.client_update_available) {
    // Nothing here can install it — hand over the one line that does, rather than a button
    // that would fail. `client_opt_in` wins when joining the group is what's missing, since
    // that is the step that turns this into a one-tap update from then on.
    const line = info.client_opt_in || info.client_command;
    toast(
      line
        ? t("decky_client_update_run", { version: info.client_latest, command: line })
        : t("decky_client_update_manual", { version: info.client_latest }),
      { duration: 12_000 },
    );
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
        // Decky's installer also phones the plugin store first, which can hang on some
        // networks before the actual install proceeds — set expectations.
        toast(t("decky_plugin_updating", { version: plugin.latest }));
        return;
      }
    } catch {
      // fall through to the manual path
    }
    toast(t("decky_plugin_update_manual"));
    return;
  }

  // Client-only update (no plugin reinstall): refresh so the button clears.
  if (check) void check(true);
}
