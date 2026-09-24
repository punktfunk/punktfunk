---
title: Steam Deck (Decky)
description: Install the Punktfunk Decky plugin to discover, pair, and stream from the Steam Deck's Gaming Mode — no drop to Desktop.
---

The Decky plugin adds a **Punktfunk** panel to the Quick Access Menu (the `…` button), so you can
find a host, pair and stream without leaving Gaming Mode.

The plugin launches the [Linux app](/docs/clients#linux-desktop-client-gtk4); it has no settings of
its own. In Desktop Mode, run the app directly. Both share one identity and one list of hosts.

## Before you start

1. Install [Decky Loader](https://decky.xyz/).
2. Install the Punktfunk client, in Desktop Mode:

   ```sh
   flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
   ```

   A native `punktfunk-client` (a sysext, a distro package, your own build) works too. With both
   installed the Flatpak wins; set `PF_DECKY_CLIENT=native` in the plugin's environment to change
   that. The client must be v0.22.0 or newer.
3. Run a [Punktfunk host](/docs/install) on your network.

## Install the plugin

1. Open the Quick Access Menu → the **plug** icon (Decky) → the **gear** → turn on **Developer
   Mode**.
2. Open the **Developer** tab and choose **Install Plugin from URL**.
3. Paste `https://unom.io/pf-decky` and confirm.

The **Punktfunk** panel appears right away. That link is the stable channel; for canary builds or a
pinned version, see [Release Channels](/docs/channels).

## Use it

Open the **Punktfunk** panel from the Quick Access Menu.

- **Hosts** lists the hosts on your network and the ones you saved. **Refresh** rescans. A lock
  means the host hasn't let this Deck in yet.
- **Tap a locked host** and choose **Request access** or **Use a PIN instead…**. See
  [Request access](#request-access), or [pair with a PIN](/docs/pairing#pair-with-a-pin). After
  that, the host connects silently.
- **Tap a host** to stream fullscreen. A sleeping host is [woken](/docs/wake-on-lan) first once the
  client knows its MAC address.
- **▸ Preset name** under a host streams it with that [preset](/docs/presets-and-links). Pin
  presets in the Punktfunk app; the panel only shows them.
- **Open Punktfunk** opens the app's console: add a host by address, pair, browse a host's
  [library](/docs/game-library), and change [settings](/docs/client-settings).

The plugin also puts a **Punktfunk** entry in your Steam library. It opens the same console.

### Request access

Tap the host → **Request access**. The stream opens and waits; it starts once someone approves the
Deck in the host's [web console](/docs/web-console). With no approval in about three minutes it
gives up.

The host must be advertising on your network. A host you added by address offers the PIN only.

### Stream from Steam's Play button

On a game's page, press **▾** beside **Play**. Hosts with that game in their library are listed
with a small Punktfunk mark. Pick one and **Play** becomes **Stream**, even when the game isn't
installed on the Deck. Pick **This device** to hand the button back to Steam. The panel's
**Punktfunk in Steam's Play menu** switch turns this off.

### Controls while streaming

- **Leave:** hold [L1 + R1 + Start + Select](/docs/input#leaving-with-a-controller) for about 1.5
  seconds, or quit the game from the Steam overlay.
- **The Steam and `…` buttons open the Deck's own menus.** To press them on the host, hold
  **Select**, or use **Host menus** in the panel while a stream runs. To send them raw instead, set
  **Open Punktfunk → Settings → Steam / guide button** to **Send to host**.
- **Steam Input.** The plugin installs a Steam Input layout called **Punktfunk**. With Steam Input
  on, the touchscreen reaches the host as real touch and the Deck is a standard gamepad. With Steam
  Input off, the host gets a full Steam Deck pad (paddles, both trackpads, gyro) and the touchscreen
  stops working as touch. Set it per game: game page → ⚙ → **Controller Settings**.

## Updating

When the plugin or the client has an update, the panel shows **Update Punktfunk** at the top. Tap
it; nothing leaves Gaming Mode. A client the plugin can't update (a sysext, a nix profile, a source
build) shows the update command instead.

The plugin follows the [channel](/docs/channels) you installed it from. If the button never
appears, install the plugin from the same URL again; Decky replaces it in place.

To update the Flatpak client from a terminal: `flatpak update`, without `sudo`.

## Troubleshooting

For host-side problems, start at [Troubleshooting](/docs/troubleshooting).

### The panel says "Update the Punktfunk client"

The client is older than v0.22.0. Tap **Update Punktfunk** in the panel, or update it in Desktop
Mode.

### The panel says "Punktfunk isn’t installed"

The plugin found no client on the Deck. [Install the Flatpak](#before-you-start) in Desktop Mode.

### No hosts listed

The host isn't reachable over mDNS. Check it runs on the same network and tap **Refresh**. For a
VPN or another subnet, add it by address in **Open Punktfunk**.

### Pairing fails

The PIN was wrong or had run out. Click **Pair a device** in the host's console again and retry, or
use **Request access**.

### Request access isn't offered

The host isn't advertising on this network. Use the PIN.

### The stream launches but doesn't take focus

Start it from the panel, not by launching the client yourself.

### A host is missing from a game's ▾ menu

The host must be paired and online, and have the game in its library as a Steam title. Open the
panel once to rescan, then reopen the game page. Check that **Punktfunk in Steam's Play menu** is
on.

### Play stays green after picking a host

Reopen the game page.

### Hidden entries named after games pile up

Each game streamed from its page has one. Panel → **About** → **Remove game shortcuts**.

### The stream is stuck, black or won't close

Panel → **About** → **Force-stop**, then start it again.

### The Punktfunk library entry is gone

Panel → **About** → **Recreate library shortcut**.

## Uninstalling

1. In the panel, tap **About → Remove game shortcuts**.
2. Remove the plugin: Quick Access Menu → **plug** icon → **gear** → **Plugins** →
   **Punktfunk** → **Uninstall**.
3. Remove the two non-Steam entries named **Punktfunk** (one is hidden): in your library, **Manage
   → Remove non-Steam game from your library**.
4. To remove the client too, in Desktop Mode:

   ```sh
   flatpak uninstall --user --delete-data io.unom.Punktfunk
   ```

   Delete `~/.config/punktfunk` as well to forget this Deck's identity and saved hosts.
5. On the host, [unpair the Deck](/docs/pairing#managing-paired-devices).

The Steam Input template stays behind at `~/.local/share/Steam/controller_base/templates/punktfunk.vdf`;
delete it if you like.

The plugin source is [`clients/decky`](https://git.unom.io/unom/punktfunk/src/branch/main/clients/decky/README.md).
