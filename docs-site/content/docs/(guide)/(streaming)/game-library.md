---
title: Your game library
description: Fill the host's game library from your launchers, add a game by hand, and launch a title from a client, Moonlight, a link or the command line.
---

Fill the host's game library once, and every client, Moonlight and the web console can launch
from it.

## Add your launchers

The library fills from [plugins](/docs/plugins), one per launcher. A fresh host has no games until
you add one:

1. Open the web console's **Library** page.
2. In **Game sources**, under **Add a source**, click each launcher you use. **Detected** marks the
   ones found on this host.

Each plugin reads its launcher's own files on the host, with no account or API key.
[Plugins](/docs/plugins) lists them all and what each one covers.

Every title gets a stable id, such as `steam:570`. To see what the host found without a client, run
`punktfunk-host library` on the host; it prints the library as JSON.

If a source reads **… can't read N of its folders**, click that line and **Allow** the folders.
A source marked **Stopped** has a plugin that isn't running: see
[Plugins → Troubleshooting](/docs/plugins#troubleshooting).

## Hide games

- **A whole source:** click its button in **Game sources**. Its titles leave every client, Moonlight
  and launching; nothing is deleted. Click again to bring them back.
- **One title:** **Hide from your devices** on its poster. The console keeps it, marked **Hidden**,
  so you can **Show on your devices again**.
- **For good:** uninstall the source's plugin.

## A game's page

Click a poster to open its page: **Information**, **Media** and **Launch**, plus a tab for each
plugin that keeps something per game. A title a plugin syncs is read-only there, marked
**Managed by** its source: change it in the launcher and the plugin syncs it again.

## Adding a game by hand

For anything no plugin knows, such as an emulator, a DRM-free build or a tool:

1. On **Library**, click **Add custom game**.
2. On **Information**, enter a **Title**.
3. Optionally, on **Launch**, enter a **Launch command**: what the host runs for this title. Without
   one, the entry is a poster with nothing to launch.
4. Click **Add**. The page stays open on the new entry.

Saving a command, or an entry with prep commands, asks for your **Console password**. To change
the entry later, open its page and **Save**; **Delete** is there too. Everything but the title is
optional:

| Tab | Field | What it does |
|---|---|---|
| **Information** | Platform, description, developer, publisher, release year, players, region, genres, tags | Free text. A poster shows the platform unless it is `PC`. |
| **Media** | **Portrait**, **Hero**, **Header**, **Logo** | The title's art, previewed as you type. See [Cover art](#cover-art). |
| **Media** | **Brand mark** | Drawn on a launcher tile that has no cover art. |
| **Launch** | **This entry opens a launcher** | Puts it in the **Launchers** row above your games. |
| **Launch** | **Executable path**, **Install directory** or **Process name** | How the host recognizes the running game. Without one it can't see the game exit, end it on disconnect or count play time. |
| **Launch** | **Who hears this title** | On a [shared display](/docs/virtual-displays): **Everyone**, **The display owner only**, **Joined sessions only** or **The session that launched it**. |

Hand-added entries live in `library.json` in the host config directory (`~/.config/punktfunk/`
on Linux, `%ProgramData%\punktfunk\` on Windows), readable by the host user only. Steps that run
before a title and undo after it go in that file: see
[Per-app prep/undo](/docs/automation#per-app-prepundo).

### Cover art

Enter an `http://` or `https://` URL, or the path of an image file on the host. A URL previews
right away; a path previews after you save.

- **A URL:** the host downloads it the first time a client asks, keeps it, and serves it to every
  client from then on. A redirect, a file over 16 MiB or a file that isn't an image goes to
  the client as a plain URL instead.
- **A path:** it must be inside a folder the host may read art from. `PUNKTFUNK_LIBRARY_ART_ROOTS`
  in [Configuration](/docs/configuration) sets those folders. Network (UNC) paths are refused.

`punktfunk-host library art --clear` empties the host's cover store. Plugin titles bring their own
art; a source below fills what they lack.

### Filling missing art and details

Install an Art & Metadata source to give every game covers and details, whatever listed it:
**SteamGridDB** (covers, heroes, logos; needs a free API key) and **Libretro** (box art and details
for ROMs). They appear under **Library** → **Art & Metadata**.

- A source fills only what a game is missing. The first source in the list wins; reorder with the
  arrows.
- **Use for every game** lets that source's art replace a game's own covers too. Details still
  only fill gaps.
- On a game's page, **Media** says where each image came from. **Choose…** shows every source's
  images for that slot, a **Wrong game?** search when a source matched the wrong title, and a
  field for your own URL. **Reset** goes back to the automatic image.
- **Information** marks a field a source filled. On your own entries it shows as a hint until you
  type your own.

Your picks survive the plugin's next sync. Turning a source off or removing it takes its art and
details away again.

## Launching a game

The client sends only the title's id. The host runs its own launch recipe for it, so a client can
never make the host run a command. The host must be [paired](/docs/pairing).

Every library starts with a **Desktop** tile that streams the host and launches nothing. While a
game runs on the host, the tile reads **Resume** and the game's name.

| From | How |
|---|---|
| Linux, Windows, Android (touch) | A host card's menu → **Browse library…** |
| Mac | **Library** in the window's sidebar |
| iPhone, iPad, Apple TV | The **Library** tab |
| Controller home (TV, Steam Deck, a client with a controller) | **Y** on a saved host, or the host's options → **Library** |
| Steam Deck | **Open Punktfunk** in the Decky panel, then the controller home. See [Steam Deck](/docs/steam-deck). |
| Moonlight | Turn on GameStream ([Moonlight](/docs/moonlight)). Titles appear in Moonlight's app list beside **Desktop**; titles with no launch command are left out. |
| A link | `punktfunk://connect/couch-pc?launch=steam:570`. See [Presets and links](/docs/presets-and-links). |
| Command line | `punktfunk library couch-pc` lists ids; `punktfunk launch couch-pc --game steam:570` streams one. See [the `punktfunk` CLI](/docs/clients#scripting-the-punktfunk-cli). |

Where the game opens (your desktop, a gamescope session, or a headless session of its own) is
display policy: see [Dedicated game sessions](/docs/virtual-displays#dedicated-game-sessions).
Whether quitting the game ends the session, and the reverse, is under
[When a game ends](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does).

## Play stats

Each launch from Punktfunk records the time and adds one to the title's launch count. Play time
grows while the host sees the game running with a session attached. A launcher tile, or a custom
entry with no **Process** hint, gets no play time.

The Apple apps sort by **Recent** and **Most played** and keep a **Recently Played** row.

The numbers live in `library-stats.json` in the host config directory. Delete a title's line to
reset it.
