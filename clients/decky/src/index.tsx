// Plugin entry. What only a Decky plugin can do: start a stream through Steam so gamescope
// focuses it (steam/), patch Steam's game page (game-page/), and stand in front of the trust
// decision that gates a stream (panel/, trust/). The client's own console home does the rest.
import { definePlugin } from "@decky/api";
import { FaTv } from "react-icons/fa";
import { installGamePageStream } from "./game-page/library-page";
import { refreshHosts } from "./hosts/store";
import { QamPanel } from "./panel/Panel";
import { watchRunningStreams } from "./steam/running";
import { ensureGamepadUiShortcut } from "./steam/shortcuts";
import { PluginErrorBoundary } from "./ui/boundary";

export default definePlugin(() => {
  // Ensure the visible, stateless "Punktfunk" library entry (opens the gamepad UI / console
  // home) exists and is repointed to the current plugin dir — also installs the native-touch
  // controller config. Fire-and-forget: cosmetic library upkeep must never block plugin load.
  void ensureGamepadUiShortcut();
  // Warm the host list and each paired host's library now, so the first game page opened
  // already knows which titles a host can stream — the QAM panel may never have been opened.
  void refreshHosts();
  // The Stream button on Steam's game pages (see game-page/library-page.tsx). Removed on dismount, or
  // Steam keeps calling into a plugin that is gone.
  const removeGamePageStream = installGamePageStream();
  // Steam's app lifetime feed tells the game page when its stream is up (Stream ↔ Stop).
  const unwatchRunning = watchRunningStreams();
  return {
    onDismount() {
      removeGamePageStream();
      unwatchRunning();
    },
    // Equals plugin.json "name": Decky finds the plugin by it, not by the lowercase folder, and
    // shows it in its list and as the panel's title. No `titleView`: Decky's title bar already
    // carries Steam's title class, and nesting that class shifts the name and shadows the back button.
    name: "Punktfunk",
    content: (
      <PluginErrorBoundary>
        <QamPanel />
      </PluginErrorBoundary>
    ),
    icon: <FaTv />,
  };
});
