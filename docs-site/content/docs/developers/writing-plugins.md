---
title: Writing plugins
description: Build a host plugin with @punktfunk/plugin-kit — the manifest, the sandbox, a minimal library source, testing on a host and publishing to a catalog.
---

A plugin is an npm package that the host's plugin runner loads and supervises. This page takes you
from an empty folder to a package operators can install from the console. Most plugins are library
sources, like [`punktfunk-plugin-steam`](https://git.unom.io/unom/punktfunk-plugin-steam);
[`punktfunk-plugin-rom-manager`](https://git.unom.io/unom/punktfunk-plugin-rom-manager) also has
its own console page.

## How a plugin runs

The runner (`punktfunk-scripting`) starts every package listed in `<config>/plugins/package.json`
whose name is `@<scope>/plugin-*` or `punktfunk-plugin-*`. `<config>` is `~/.config/punktfunk` on
Linux and `%ProgramData%\punktfunk` on Windows.

- It imports the package's `module` (else `main`) entry. The default export must be a plugin
  definition.
- A crash restarts the plugin with backoff, from 1 s up to 60 s. A clean return ends it.
- On stop, the plugin's Effect fiber is interrupted, so scoped finalizers run.
- The plugin calls the [management API](/docs/developers/management-api) with a token of its own.
  It reaches the plugin allowlist only: it can write its own library titles and registration,
  but not hooks, pairing, the plugin store, or a `command` launch.
- Lines it logs with `Effect.log` show on the console's **Troubleshooting** page as
  `plugin:<id>`.

| | Linux | Windows |
|---|---|---|
| Isolation | One bubblewrap sandbox per plugin, with its own PID namespace | All plugins share the runner's `NT AUTHORITY\LocalService` account |
| Files | `/usr`, its own code, its state dir, the manifest's paths and the operator's grants | The config dir is read-only except `plugin-state`; the user's profile is closed |
| Network | None unless the manifest sets `network` | Unrestricted |
| Host API | A socket the runner forwards; `connect()` finds it | Loopback HTTPS |

## The manifest

Declare what the plugin needs in a `punktfunk` block in its `package.json`; the
[minimal plugin](#a-minimal-library-plugin) below has one. It ships inside the reviewed tarball, so
nothing the plugin does at runtime widens it. Without it, a plugin does not start on Linux.

| Field | Meaning |
|---|---|
| `schema` | `1`. Any other value is refused. |
| `id` | The plugin id: `[a-z][a-z0-9-]*`, at most 64 characters. Use the same string as the plugin's `name`; it keys the plugin's token, state dir and library provider. |
| `reads` | Paths the plugin reads. Absolute or `~/`-rooted. A missing path is skipped. |
| `writes` | Paths it also writes. |
| `network` | `true` to reach the network. |
| `exec` | Named argv templates the host may run for this plugin's titles. |

The sandbox never binds `/`, the home directory or its parents, `/proc`, `/sys`, `/dev`, `/run`
(except `/run/media`), `~/.ssh`, `~/.gnupg` or `~/.config/punktfunk`.

An `exec` template names a program and its arguments with `{param}` placeholders. A title supplies
the values, and the host builds the argv itself. `exe` is a program name looked up on `PATH`, or an absolute path inside the plugin's roots; it may
be a `{param}` of kind `path`. `cwd` is optional and must also lie inside the roots. Each parameter
declares its kind:

| Kind | Accepts |
|---|---|
| `name` | Printable text, up to 128 characters |
| `id` | `[A-Za-z0-9._-]`, up to 128 characters |
| `digits` | ASCII digits, up to 32 |
| `path` | An absolute path inside the plugin's declared or granted roots |
| `args` | Extra argv elements. The only kind that may start with `-` or repeat |

No other value may start with `-`. The host drops a title whose values don't fit when the plugin
publishes it.

## A minimal library plugin

`@punktfunk/plugin-kit/library` supplies everything except the scan: the sync loop, file watching,
the console settings form and the CLI. Four files:

```json title="package.json"
{
  "name": "@you/plugin-hello",
  "version": "0.1.0",
  "type": "module",
  "module": "./dist/index.js",
  "bin": { "punktfunk-plugin-hello": "./dist/cli.js" },
  "files": ["dist"],
  "scripts": {
    "build": "bun build src/index.ts src/cli.ts --target=bun --outdir dist --external effect --external '@punktfunk/*'",
    "test": "bun test"
  },
  "dependencies": { "@punktfunk/plugin-kit": "^0.5.3", "effect": "4.0.0-beta.99" },
  "punktfunk": {
    "schema": 1,
    "id": "hello",
    "reads": ["~/.local/share/hello"],
    "exec": {
      "play": { "exe": "hello-launcher", "args": ["{game}"], "params": { "game": "id" } }
    }
  }
}
```

```ts title="src/plugin.ts"
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Effect, Schema } from "effect";
import { defineLibraryPlugin } from "@punktfunk/plugin-kit/library";

const dir = () => path.join(os.homedir(), ".local/share/hello");
const db = () => path.join(dir(), "games.json");

export const plugin = defineLibraryPlugin({
  name: "hello",
  configSchema: Schema.Struct({}),
  detect: () => Effect.sync(() => fs.existsSync(db())),
  scan: () =>
    Effect.sync(() => {
      const games: { id: string; title: string }[] = JSON.parse(fs.readFileSync(db(), "utf8"));
      return games.map((g) => ({
        external_id: g.id,
        title: g.title,
        launch: { kind: "exec", value: "play", args: [{ name: "game", value: g.id }] },
      }));
    }),
  watchDirs: () => [dir()],
});
```

```ts title="src/index.ts"
import { plugin } from "./plugin.js";
export default plugin.def;
```

```ts title="src/cli.ts"
#!/usr/bin/env bun
import { plugin } from "./plugin.js";
await plugin.cli();
```

Before `bun install`, add `@punktfunk:registry=https://git.unom.io/api/packages/unom/npm/` to
`.npmrc`.

The host lists each title as `hello:<external_id>`. Other `launch` kinds cover the common
launchers; the table is on `LaunchSpec` in
[`plugin-kit/src/wire.ts`](https://git.unom.io/unom/punktfunk/src/branch/main/plugin-kit/src/wire.ts).
Add `detect: { install_dir }` to a title whose launch hands off and exits, so the host can still
tell when the game quits.

## Beyond a library source

| Need | Use |
|---|---|
| Settings | `configSchema`. The console renders a form from it; the file is `<config>/plugin-state/<id>/config.json` |
| Your own state | `pluginStateDir("<id>")`; writable on every platform |
| Data from a desktop app | `pluginIngestDir("<id>")`, an inbox any local user may write. Treat its contents as untrusted |
| A console page | `definePluginKit` with `serveUi({ title, icon, staticDir, api })` |
| A tab on each game's page | `serveUi({ title, game })`; see [below](#a-tab-on-each-games-page) |
| Art or details for every game | `defineMetadataPlugin`; see [below](#a-source-of-art-and-details) |
| Reacting to events only | `definePlugin({ name, main: async (pf) => … })` from `@punktfunk/host` |

Keep Effect values inside the plugin: the runner bundles its own copy of Effect, so the default
export must be a plain async `main`. `definePluginKit` and `defineLibraryPlugin` build that for you.

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

- **Which games:** the kit looks up a game only when it lacks something in `offers`, and never a
  launcher tile. A game that already has the art keeps it unless the operator ticks **Use for
  every game**.
- **Matching:** `entry.ids` carries the ids its plugin knows (`steam`, `gog`, `epic`, `libretro`,
  `sgdb`). `matching: "exact"` says you match only by those; exact sources rank before
  `"search"` ones by default.
- **Pins:** the operator's **Wrong game?** stores a `key` from your `search`; it reaches `match`
  as `pin`.
- **Art** is an `http(s)` URL. The host fetches and keeps it; anything else is dropped.
- **Failures:** fail with `SourceRateLimited` to stop the round and retry later, or
  `SourceUnauthorized` to stop it and show the reason in the console. Any other failure skips
  that game until the next round.
- **Choose…** on a game's Media tab lists `images(match, kind)` from every source, or the one
  image `fetch` found. `lookup <id>` on the plugin's CLI prints what it finds for one game.

Add `"network": true` to the manifest. Register in the store's `metadata` category so the console
offers it under **Library** → **Art & Metadata**.

## Folders you can't know in advance

A package can't know where someone keeps their ROMs or a second Steam library. Return those
folders from `wants: (cfg) => [...]`. Before each scan the kit asks the host for every one the
plugin can't reach, then scans what it can reach now.

A plugin that doesn't use `defineLibraryPlugin` yields `requestAccess(paths, reason)` from
`@punktfunk/plugin-kit`. A request only creates a pending row: the operator allows it on the
library source in the console, or with `punktfunk-host plugins grant`. A grant restarts the plugin.
Don't tell users to widen the runner unit or change ACLs by hand.

When the operator is the one who knows the folder, a save folder or a config directory, make it a
`handedPath()` field in `game` or `config`. The console grants each folder the operator adds there
when the form saves, read-only unless `handedPath({ write: true })`. A folder the plugin fills in
itself is never granted that way. On Windows a plugin's own write request is always refused;
write access comes only from the operator, and never inside Program Files or ProgramData.

## Test it

Without a host, from your plugin's folder:

```sh
bun test
bun src/cli.ts detect           # present / absent
bun src/cli.ts scan --preview   # the entries a sync would send, as JSON
```

On a Linux host, install the packed tarball like any plugin. `plugins add` restarts the runner:

```sh
bun run build && bun pm pack --destination /tmp/pf-dev
punktfunk-host plugins add /tmp/pf-dev/you-plugin-hello-0.1.0.tgz --allow-public-registry
journalctl --user -u punktfunk-scripting -f
punktfunk-host plugins remove @you/plugin-hello   # when you're done
```

Any package outside the `@punktfunk` scope needs `--allow-public-registry`.

Before a release, guard your title ids. With the previous version installed,
`bun src/cli.ts parity --snapshot before.json` records what the host lists for your source;
`bun src/cli.ts parity --compare before.json` diffs your working copy's scan against it and exits
non-zero on any change. A changed id breaks pins and art caches on every client.

## Publish

1. Publish under a scoped name, `@<scope>/plugin-<id>`, to an HTTPS npm registry. The
   first-party repos publish from CI on a `v*` tag with `bun publish`.
2. From the public npm registry, operators can install it by name:
   `punktfunk-host plugins add @<scope>/plugin-<id> --allow-public-registry`. The console shows
   it as **Installed via CLI**.
3. To appear under **Browse** as **Verified**, open a pull request against
   [`punktfunk-plugin-index`](https://git.unom.io/unom/punktfunk-plugin-index): add an entry to
   `v1/index.json` pinning the exact `version` and the registry's `integrity` hash, then run
   `bun run validate`. unom reviews that tarball; each new version is a new pull request.
4. To run your own catalog, serve an `index.json` in the same format over HTTPS, with its ed25519
   signature at `<url>.sig`; the index repo's `tools/` generate a key and sign. Operators add it
   under **Plugins** → **Sources** with your public key. Its plugins install without the
   **Verified** badge, and an unsigned source is flagged.
