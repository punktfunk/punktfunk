# Punktfunk — Steam Deck plugin (Decky)

A [Decky Loader](https://decky.xyz/) plugin that adds a Punktfunk panel to the Quick Access Menu.

**It is a launcher, not a client.** It decodes nothing, browses no library and holds no settings —
the Rust client does all of that. The plugin's whole job is to start it *the right way* so gamescope
fullscreens and focuses it. Setup and use are
[the docs site](https://docs.punktfunk.unom.io/docs/steam-deck)'s; this file is the mechanisms,
because none of them are obvious and all of them were expensive.

## Why the launch goes through Steam

gamescope only gives focus and fullscreen to the window tree Steam launched via `reaper` — it
detects the current app by AppID (gamescope#484). A client spawned from the plugin's own backend
comes up invisible and unfocused. So the plugin registers non-Steam shortcuts whose exe is `/bin/sh`
running `bin/punktfunkrun.sh`, and starts them with `RunGame`.

The exe is `/bin/sh` with the wrapper as an argument so the script never needs an exec bit: Decky's
zip extraction drops it, and the root-owned plugins dir cannot be chmodded by the unprivileged
backend.

There are two visible shortcuts, both named `Punktfunk` so Steam keys them to one Steam Input
configset (the key is the lowercase name): a hidden stateful one carrying the stream, and the
visible stateless library entry that opens console home.

## The stream that looks like the game

A tap runs the ordinary launch with `PF_GAME=steam:<appid>`, which the wrapper turns into
`punktfunk launch <host> --game steam:<appid>` — the Deck names the title, the host resolves it
against its library, so no launch recipe ever rides the Steam launch options.

The first stream of a title mints a *third* kind of shortcut: same `/bin/sh` + wrapper, hidden, but
named with the game's display name and dressed in its own grid, hero, logo, header and icon
(`game_art` reads Steam's `appcache/librarycache` first, the store CDN second). So the overlay, the
now-playing surfaces and the friends list show the game rather than Punktfunk, and Steam Input binds
the native-touch layout per game name. Steam then shows the page of the app it just launched or
closed — the hidden shortcut's — so the route patch sends that page back to the title's own. The
shortcut's last-played time is mirrored onto the title at launch and again at load, so the game
climbs Recent and stays there across a reboot.

## Reaching Steam's play bar

`/library/app/:appid` is patched and the render walked through five of Steam's section components.
The play section is a **MobX observer class**, so a prototype patch never runs — MobX installs a
read-only, non-configurable reactive `render` on each instance — and a plain function wrapper around
a class throws. Each class is therefore replaced by a subclass that wraps MobX's render at
definition time (`patch.ts`); function, memo and forwardRef components get wrapped copies. Every
render handler is guarded so a miss leaves Steam's output untouched.

The Play button class's bound `ShowStreamingMenu` cannot be extended either, so the ▾ is re-pointed
at a menu built from the same data (`overview.per_client_data`, `BIsPerClientDataLocal`,
`selected_clientid`), Steam's own Menu components and class names, and Steam's own localization
tokens. Selecting a Steam client calls `SteamClient.Apps.SetStreamingClientForApp`, exactly as
Steam's own item does.

Steam renders the ▾ only while `per_client_data` holds a second client. Where a Punktfunk host has
the title and the ▾ is missing, the plugin adds one: the main button's component in Steam's
`StreamingSelector` classes. The Play button is re-dressed only with a ▾ on the page, because the
menu's **This device** row is the way back.

Every attempt logs where it got to in `window.__punktfunkDiag` (CEF console lines start with
`punktfunk:`); `localStorage["punktfunk:diagVerbose"] = "1"` traces every step.

Which hosts get listed: each scan asks every **paired, online** host for its library
(`punktfunk library <host> --json`) and caches the set of `steam:<appid>` ids per host record. A
sleeping host keeps its last set so its titles still list — the launch wakes it. A host answering
`needs-pairing` or `refused` loses its set; a forgotten record is pruned. The match is Steam's own
appid against that set, so non-Steam shortcuts never match.

## Request access is a launch, not a ceremony

The plugin saves the host with the fingerprint it **advertised**, then starts an ordinary identified
connect with the handshake budget stretched to 185 s. The host parks that connection until its
operator approves the device, then admits the same connection.

**No advertised fingerprint, no request access.** That pinned fingerprint is the only thing between
a 185-second wait and an impostor answering for the host, so a host typed in by address gets the PIN
path only, and the sheet says why. The plugin never trusts-on-first-use past a missing fingerprint.

## Build & sideload

```sh
cd clients/decky
pnpm install
pnpm build                             # rollup → dist/index.js
pnpm run package                       # → out/punktfunk/ + out/punktfunk-v<ver>.zip
DECK=deck@<deck-ip> pnpm run deploy    # rsync → /tmp, sudo-install, restart loader

python3.13 scripts/test-backend.py     # stdlib-only backend checks
```

`~/homebrew/plugins/` is root-owned (the loader runs as root), so `deploy.sh` stages to a temp dir
then sudo-installs and restarts the loader — `DECKPASS=…` runs it non-interactively. A loader
restart is required for an out-of-band install to appear.

## Architecture

Everything below the panel is the CLI. `main.py` builds argv and maps exit codes; it parses none of
the client's data files and re-implements none of its rules.

| File | Role |
| --- | --- |
| `src/index.tsx` | Plugin entry and the QAM panel. |
| `src/hooks.ts` | The module-level host store (one scan merging discovery and the saved store), the update hooks, the launch action, the trust-state model. |
| `src/catalog.ts` | Which paired hosts have which Steam appids. |
| `src/library-page.tsx` · `src/play-from.tsx` | The game-page route patch and the descent to the play bar; Punktfunk hosts in Steam's "Play from" dropdown. |
| `src/patch.ts` · `src/game.tsx` · `src/diag.ts` | MobX-observer-safe subclassing; the title record and lens mark; the diagnostics buffer. |
| `src/trust.tsx` · `src/pair.tsx` | The trust sheet and the gamepad-navigable PIN keypad. |
| `src/steam.ts` | Shortcut launch (`AddShortcut` / `SetAppLaunchOptions` / `RunGame`) and the running-state feed. |
| `bin/punktfunkrun.sh` | The wrapper the shortcut runs. Reads `PF_REF` / `PF_PROFILE` / `PF_GAME` / `PF_REQUEST_ACCESS` / `PF_BROWSE`. |
| `main.py` | Five thin CLI shells plus the Steam-side work only a plugin can do — `runner_info`, `shortcut_art`, `game_art`, `apply_controller_config`, `kill_stream`, `check_update` / `update_client` (with an explicit CA-bundle search: Decky's embedded Python has no usable default TLS roots on SteamOS). |

The client must be **v0.22.0 or newer** — that is when the headless `punktfunk` CLI shipped, and the
panel drives everything through it.

## Known gaps

- **Presets and pinned cards can't be created here.** The panel renders them; making one needs the
  desktop client or the client's own gamepad UI.
- **Per-game pins are on hold.** The shared model pins host+preset; nothing persists a pinned
  *game* yet. The old `decky-pinned.json` is left on disk for a later migration.
- **A parked connect looks like a hanging one.** The plugin toasts before a request-access launch to
  set expectations, which is a patch, not a fix — the session's connect screen should learn the
  "waiting for approval" copy the console shell already has.
- **Our labels are English.** Steam's own "Stream" and "Stop" are localized; the tokens it uses are
  not something to guess at from outside.
