# @punktfunk/plugin-kit

The framework Punktfunk host plugins are built on. It owns what every plugin shares: lifecycle and
shutdown, config and state files, the library sync loop, the console page, the CLI and logging.
Your plugin is its domain logic. Built on [`@punktfunk/host`](../sdk) and
[Effect](https://effect.website).

## Install

The package is on the unom npm registry. Point the `@punktfunk` scope at it once, in `.npmrc`:

```ini
@punktfunk:registry=https://git.unom.io/api/packages/unom/npm/
```

```sh
bun add @punktfunk/plugin-kit effect
```

`effect` and `@punktfunk/host` are peer dependencies; `react` is an optional one for
`@punktfunk/plugin-kit/react`.

## Usage

A library source is a scan function; `defineLibraryPlugin` adds the rest:

```ts
import { Effect, Schema } from "effect";
import { defineLibraryPlugin } from "@punktfunk/plugin-kit/library";

export const plugin = defineLibraryPlugin({
  name: "hello",
  configSchema: Schema.Struct({}),
  detect: () => Effect.succeed(true),
  scan: () => Effect.succeed([{ external_id: "1", title: "Hello World" }]),
});

export default plugin.def; // what the runner loads; `plugin.cli()` is the CLI entry
```

Any other plugin uses `definePluginKit({ name, layer, main })`, and `serveUi(...)` for a page in the
web console.

| Import | Holds |
|---|---|
| `@punktfunk/plugin-kit` | `definePluginKit`, config and cache stores, the sync engine, `serveUi`, `requestAccess`, the CLI scaffold |
| `@punktfunk/plugin-kit/library` | `defineLibraryPlugin`, parsers for VDF, SQLite and the Windows registry, the parity check |
| `@punktfunk/plugin-kit/wire` | The library wire schemas: `ProviderEntry`, `LaunchSpec` and its launch kinds |
| `@punktfunk/plugin-kit/react` | Browser helpers for a plugin's console page |
| `@punktfunk/plugin-kit/theme.css` | The console's colours for a plugin's page |

## Docs

[Writing plugins](https://docs.punktfunk.unom.io/docs/developers/writing-plugins) covers the
manifest, the sandbox, a complete minimal plugin, testing on a host and publishing.
[`punktfunk-plugin-steam`](https://git.unom.io/unom/punktfunk-plugin-steam) and
[`punktfunk-plugin-rom-manager`](https://git.unom.io/unom/punktfunk-plugin-rom-manager) are real
plugins to copy.

## Development

```sh
bun install
bun run check
bun run typecheck
bun test
```

Tag `plugin-kit-vX.Y.Z` (matching `package.json`) to publish.
