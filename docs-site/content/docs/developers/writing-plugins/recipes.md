---
title: Plugin recipes
description: What a plugin can do beyond listing games — settings, a console page, a tab on each game, art and details, holding a launch, and titles that download on demand.
---

Add one of these to a plugin: each recipe is the code you write and the rules the host keeps for
you.

| Need | Use |
|---|---|
| Settings | `configSchema`. The console renders a form from it; the file is `<config>/plugin-state/<id>/config.json` |
| Your own state | `pluginStateDir("<id>")`; writable on every platform |
| Data from a desktop app | `pluginIngestDir("<id>")`, an inbox any local user may write. Treat its contents as untrusted |
| A console page | `definePluginKit` with `serveUi({ title, icon, staticDir, api })` |
| A tab on each game's page | `serveUi({ title, game })`: [below](#a-tab-on-each-games-page) |
| Art or details for every game | `defineMetadataPlugin`: [below](#a-source-of-art-and-details) |
| Work before a game starts | `serveUi({ title, holds })`: [below](#hold-a-launch) |
| Titles that download before they play | `serveUi({ title, install })`: [below](#install-a-title) |
| Reacting to events only | `definePlugin({ name, main: async (pf) => … })` from `@punktfunk/host` |

Keep Effect values inside the plugin: the runner bundles its own copy of Effect, so the default
export must be a plain async `main`. `definePluginKit` and `defineLibraryPlugin` build that for you.

## A console page

A console page runs sandboxed, with an origin of its own: no cookies, `localStorage` or
`indexedDB`, and no access to the console or another plugin's page.

- Its address carries its credential in the first path segment, so reach your API with relative
  URLs or through `resolvePluginBase()` from `@punktfunk/plugin-kit/react`, never a hand-written
  `/plugin-ui/<id>/`.
- Keep state in your plugin's backend.
- Don't send `fetch` with `credentials: "include"`.

## A tab on each game's page

Something a plugin keeps per game, like the files it swaps or a per-game switch, belongs on that
game's page in the console. Pass `game` to `serveUi`:

```ts
import { Effect, Schema } from "effect";
import { handedPath, serveUi } from "@punktfunk/plugin-kit";

const Section = Schema.Struct({
  enabled: Schema.Boolean.annotate({ title: "Swap this game's files" }),
  paths: Schema.Array(handedPath({ write: true })).annotate({ title: "Folders" }),
});

yield* serveUi({
  title: "Game slots",
  game: {
    schema: Section,
    load: (entryId) => Effect.succeed(store.get(entryId) ?? { enabled: false, paths: [] }),
    save: (entryId, value) => Effect.sync(() => store.set(entryId, value)),
    status: (entryId) => Effect.succeed([{ level: "info", text: "No slot active." }]),
  },
});
```

- `entryId` is the library id, `steam:570` or `custom:<id>`. `load` returning `undefined` means
  no tab on that entry.
- `save` receives the value decoded against `schema`; a body that doesn't decode never reaches it.
- `status` returns up to eight short lines shown above the form.
- A plugin with a `game` section and no `staticDir` gets no nav entry.

## A source of art and details

An Art & Metadata source fills covers and details for games other plugins list, the way
[`punktfunk-plugin-steamgriddb`](https://git.unom.io/unom/punktfunk-plugin-steamgriddb) covers every
game. Write how to find a game and what to fetch for it; `defineMetadataPlugin` does the rest:

```ts
import { Effect, Schema } from "effect";
import { defineMetadataPlugin } from "@punktfunk/plugin-kit/metadata";

export const plugin = defineMetadataPlugin({
  name: "covers",
  configSchema: Schema.Struct({}),
  matching: "search",
  offers: { art: ["portrait", "hero"], meta: ["developer"] },
  match: (entry, cfg, pin) =>
    Effect.succeed({ key: pin ?? entry.title, label: entry.title }),
  fetch: (match) =>
    Effect.succeed({ art: { portrait: `https://covers.example/${match.key}.png` } }),
});

export default plugin.def;
```

| Topic | Rule |
|---|---|
| Which games | The kit looks up a game only when it lacks something in `offers`, and never a launcher tile. A game that already has the art keeps it unless the operator ticks **Use for every game**. |
| Matching | `entry.ids` carries the ids its plugin knows (`steam`, `gog`, `epic`, `libretro`, `sgdb`). `matching: "exact"` says you match only by those; exact sources rank before `"search"` ones by default. |
| Pins | The operator's **Wrong game?** stores a `key` from your `search`; it reaches `match` as `pin`. |
| Art | An `http(s)` URL. The host fetches and keeps it; anything else is dropped. |
| Failures | Fail with `SourceRateLimited` to stop the round and retry later, or `SourceUnauthorized` to stop it and show the reason in the console. Any other failure skips that game until the next round. |
| Choosing by hand | **Choose…** on a game's Media tab lists `images(match, kind)` from every source, or the one image `fetch` found. `lookup <id>` on the plugin's CLI prints what it finds for one game. |

Add `"network": true` to the manifest. Register in the store's `metadata` category so the console
offers it under **Library** → **Sources** → **Art & Metadata**.

## Hold a launch

A plugin that must act before a game starts, like swapping its config files, holds the
`game.launching` stage. The host calls the handler before it spawns the game and waits for it:

```ts
yield* serveUi({
  title: "Game slots",
  holds: {
    "game.launching": (game) => swapIn(game.app, game.fingerprint, game.preset),
  },
  holdTimeoutMs: 60_000,
});
```

- `game` is the event's `GameRef`: app id, store, title, client, fingerprint, plane, profile and
  the preset the device dialled with.
- The stage fires only when the host starts the game, not when it picks up one still running.
- `holdTimeoutMs` is 1–120 000, default 30 000. Past it the host starts the game anyway and logs
  the plugin; the handler's Effect is interrupted.
- A failure logs and never blocks the launch. A plugin mid-restart misses the stage.
- A library source holds through `defineLibraryPlugin({ holds, holdTimeoutMs })`; its handler
  also gets the plugin's config: `(game, cfg) => …`. It fires for every launch on the host, so
  check that `game.app` is one of yours.

## Install a title

A plugin can list titles that aren't on the host yet: a ROM on a server, a game in a store. Mark
each with `install` in its entry, serve the host's install requests, and report progress. The host
does the rest: speed and time left, the console's Downloads list, and every client's launch screen.
A player who starts a title that isn't installed waits on that screen until the files are in, then
the game starts.

```ts
const reporter = yield* downloadReporter("my-plugin");
yield* serveUi({
  title: "My library",
  install: {
    start: ({ externalId }) => startDownload(externalId, reporter),
    pause: ({ externalId }) => pauseDownload(externalId),
    cancel: ({ externalId }) => cancelDownload(externalId),
    uninstall: ({ externalId }) => removeFiles(externalId),
  },
});
// In each entry the plugin reconciles:
//   install: { state: "missing", size_bytes: 26e9, target: "/mnt/games/my-plugin" }
```

| Handler | Rule |
|---|---|
| `start` | Answers once the download is under way, not when it ends. Start a paused one again from where it stopped. |
| `pause` | Keeps the partial files. |
| `cancel` | Discards them. |
| `uninstall` | Removes what the plugin downloaded and nothing else. Saves stay. |

- Every handler may fail with `InstallRefused({ message })`: one sentence the player or operator
  reads ("Not enough space in /mnt/games: it needs 26 GB and 8 GB is free.").
- Call `reporter.set({ externalId, state, doneBytes, totalBytes, phase, error })` on every change.
  The reporter keeps the host's rules for you: one send a second at most, and a restatement every
  5 s while a title is `queued`, `downloading` or `installing`. A title the host hears nothing
  about for 30 s counts as stalled, and a launch waiting on it gives up.
- After a download finishes or files go, reconcile again so the entry's `install.state` says what
  is on disk. `target` must be a folder the plugin may write; the host shows its free space.
- On a host that predates downloads, `reporter.supported()` turns `false`: keep fetching in a
  launch hold.
- A library source passes the four handlers to `defineLibraryPlugin({ install })`, each with
  the config as its second argument, and reports from `downloads: (reporter, config) => …`,
  which runs beside the sync. The host takes a row for any title the plugin lists, so a source
  can also report downloads its launcher started.
