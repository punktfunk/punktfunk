// Which Steam compatibility tool a Punktfunk shortcut runs in. The wrapper needs the host's
// flatpak, so any tool breaks the launch: Proton and the Steam Linux Runtime are containers.

/** The slice of `SteamClient.Apps` this module reads. */
export interface CompatApps {
  /** Pushes the app's details on registration and on every change. */
  RegisterForAppDetails?(
    appId: number,
    callback: (details: { strCompatToolName?: string } | null) => void,
  ): { unregister(): void };
}

/** The tool Steam will run `appId` in, the player's global default included: "" for none,
 *  null when Steam does not answer within `timeoutMs`. */
export function compatToolOf(
  apps: CompatApps,
  appId: number,
  timeoutMs = 2000,
): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    let reg: { unregister(): void } | null = null;
    const settle = (tool: string | null) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      reg?.unregister();
      resolve(tool);
    };
    const timer = setTimeout(() => settle(null), timeoutMs);
    try {
      reg =
        apps.RegisterForAppDetails?.(appId, (d) => {
          if (d) {
            settle(d.strCompatToolName ?? "");
          }
        }) ?? null;
      if (settled) {
        reg?.unregister(); // answered synchronously, before `reg` was set
      }
      if (!reg) {
        settle(null);
      }
    } catch {
      settle(null);
    }
  });
}
