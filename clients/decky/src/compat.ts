// Which Steam compatibility tool a Punktfunk shortcut runs in. The wrapper needs the host's
// flatpak, so it only lives outside Proton and outside the Steam Linux Runtime containers.

/** Steam's container-free runtime for a native non-Steam game; Steam installs it (226 MB) on the
 *  first launch that needs it. Every other tool is Proton or a Steam Linux Runtime container, and
 *  neither has flatpak. */
export const NATIVE_TOOL = "legacysteamruntime";

/** The slice of `SteamClient.Apps` this module drives. */
export interface CompatApps {
  /** An empty name removes the app's own pick; the player's global default then applies. */
  SpecifyCompatTool?(appId: number, toolName: string): void;
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

/**
 * Keep `appId` out of Proton. Where Steam reports a tool for the shortcut, its own pick or the
 * player's global default, pin the native runtime; Steam maps it at once. Clearing a pick is no
 * way out: a shortcut that ever had one then inherits the global default, which is Proton. Where
 * Steam reports none the launch already works. Returns the tool replaced.
 */
export async function pinNativeRuntime(
  apps: CompatApps,
  appId: number,
  timeoutMs?: number,
): Promise<string | null> {
  const tool = await compatToolOf(apps, appId, timeoutMs);
  if (!tool || tool === NATIVE_TOOL) {
    return null;
  }
  apps.SpecifyCompatTool?.(appId, NATIVE_TOOL);
  return tool;
}
