---
title: Management API
description: Drive a Punktfunk host over its HTTPS management API — credentials, a first call, the event stream, the TypeScript SDK and the OpenAPI spec.
---

Every host serves a REST API that the web console, `punktfunk-host ctl`, the tray and plugins all
use. This page gets you a first authenticated call and shows where the full reference lives.

## Where it listens

`https://<host>:47990/api/v1`, over HTTPS with the host's self-signed identity certificate.

- The listener binds all interfaces. Admin calls are accepted from loopback only; from the LAN,
  only a paired device gets in (see [Credentials](#credentials)).
- Move it with `--mgmt-bind <IP:PORT>` or `PUNKTFUNK_MGMT_BIND` in `host.env`
  ([Host CLI](/docs/host-cli)). `127.0.0.1:47990` makes it loopback-only.
- On every start the host writes the URL it bound to `<config>/mgmt-endpoint`, as
  `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>`. Read that instead of assuming 47990.

`<config>` is `~/.config/punktfunk` on Linux and `%ProgramData%\punktfunk` on Windows
(`PUNKTFUNK_CONFIG_DIR` overrides it).

## Credentials

Send a bearer token as `Authorization: Bearer <token>`. Which token decides what you may call:

| Credential | Where it comes from | Accepted from | May call |
|---|---|---|---|
| Admin token | `<config>/mgmt-token` | loopback | everything |
| Plugin token | `<config>/plugin-token`, minted while the plugin runner is installed | loopback | the plugin allowlist: status, library, sessions, displays, events, plugin registration. Not hooks, pairing admin, host logs, the plugin store or updates |
| Per-plugin token | handed to each plugin by the runner | loopback | the plugin allowlist, and only its own registration and library provider |
| Paired device | a paired client certificate over mTLS, or a browser's device-key token | anywhere | `GET` host, status, compositors, actions and library; log upload; invoking an action it holds the grant for |

The token files hold one `KEY=<hex>` line (`PUNKTFUNK_MGMT_TOKEN=…`), so a shell or a systemd
`EnvironmentFile` can source them. Send only the value after `=`. The admin file is owner-only:
your user on Linux, Administrators on Windows. To pin a token of your own, see
`PUNKTFUNK_MGMT_TOKEN` in [Configuration](/docs/configuration).

`GET /api/v1/health`, the spec and the reference UI need no credential. Errors come back as
`{"error": "<message>"}` with the HTTP status.

## A first call

On the host, as the user that runs it:

```sh
. ~/.config/punktfunk/mgmt-endpoint   # sets PUNKTFUNK_MGMT_URL
. ~/.config/punktfunk/mgmt-token      # sets PUNKTFUNK_MGMT_TOKEN
curl -k -H "Authorization: Bearer $PUNKTFUNK_MGMT_TOKEN" "$PUNKTFUNK_MGMT_URL/api/v1/status"
```

On Windows, from an elevated PowerShell:

```powershell
$t = (Get-Content "$env:ProgramData\punktfunk\mgmt-token") -replace '^PUNKTFUNK_MGMT_TOKEN=', ''
curl.exe -k -H "Authorization: Bearer $t" https://127.0.0.1:47990/api/v1/status
```

`-k` is needed because the certificate is self-signed and names no host. The SDK pins it instead.

For shell scripts, [`punktfunk-host ctl`](/docs/host-cli#ctl) wraps the common calls and handles
the token and certificate for you.

## Events

`GET /api/v1/events` is a Server-Sent Events stream of host lifecycle events: clients, sessions,
streams, pairing, displays, library, updates. [Events & hooks](/docs/automation) has the event
catalogue, the frame format, resume with `Last-Event-ID`, and the `?kinds=` filter.

## TypeScript SDK

`@punktfunk/host` is a typed client for every endpoint plus the event stream, with reconnect and
resume. It lives in [`sdk/`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk) and is
published to the unom npm registry:

```sh
echo '@punktfunk:registry=https://git.unom.io/api/packages/unom/npm/' >> .npmrc
bun add @punktfunk/host effect     # or: npm i @punktfunk/host effect
```

```ts
import { connect } from "@punktfunk/host";

const pf = await connect();
const host = await pf.api.getHostInfo();
console.log(`connected to ${host.hostname}`);

pf.events.on("pairing.pending", async (e) => {
  const pending = await pf.api.listPendingDevices();
  const match = pending.find((d) => d.fingerprint === e.device.fingerprint);
  if (match) await pf.api.approvePendingDevice(String(match.id), { payload: {} });
});
```

`connect()` needs no arguments on the host box. It resolves each setting in this order:

| Setting | Order |
|---|---|
| URL | `{ url }`, `PUNKTFUNK_MGMT_URL`, `<config>/mgmt-endpoint`, `https://127.0.0.1:47990` |
| Token | `{ token }`, `PUNKTFUNK_MGMT_TOKEN`, `PUNKTFUNK_PLUGIN_TOKEN`, `<config>/plugin-token` |
| Certificate pin | `{ ca }`, `PUNKTFUNK_MGMT_CA` (a path), `<config>/native-cert.pem`, `<config>/cert.pem` |

It never reads `mgmt-token` on its own. A script that needs admin routes, such as the pairing
approval above, sets `PUNKTFUNK_MGMT_TOKEN` or passes `{ token }`.

`pf.events.on()` takes an event kind, a `domain.*` prefix or `*`, plus `dropped` (you missed
events; re-read state over REST) and `unknown` (a kind newer than the SDK). It starts at the live
tail, and reconnects and resumes on its own.

Three entry points:

| Import | For |
|---|---|
| `@punktfunk/host` | Promise API: `connect()`, `pf.api.*`, `pf.events.on()`, `pf.request()` for an untyped call |
| `@punktfunk/host/effect` | The same as Effect services, `Stream` events and typed errors |
| `@punktfunk/host/core` | The Effect surface without Node, for a browser; `deviceKey()` signs in with a paired key |

Runnable examples, from a one-screen event tail to a USB passthrough script:
[`sdk/examples`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/examples). Run one from
`sdk/` with `bun install && bun examples/tail-events.ts`. To keep code running on the host, package
it as a plugin ([Writing plugins](/docs/developers/writing-plugins)) or start the script from your
own systemd unit or scheduled task.

## Reference

- [`/api`](/api) on this site: the interactive reference, built from
  [`api/openapi.json`](https://git.unom.io/unom/punktfunk/src/branch/main/api/openapi.json).
- On a running host: `/api/docs` (the same reference) and `/api/v1/openapi.json` (the spec).
- `punktfunk-host openapi` prints the spec of that binary.

## Changing the API

Handlers live in `crates/punktfunk-host/src/mgmt/`. Annotate a new handler with
`#[utoipa::path]` and add it to `api_router_parts` in `crates/punktfunk-host/src/mgmt.rs`.

1. Classify the route for the plugin and paired-device lanes: add a row to `EXPECTED` in
   `every_route_is_classified_for_the_plugin_and_cert_lanes` (`mgmt/tests.rs`), and to
   `plugin_may_access` or `cert_may_access` in `mgmt/auth.rs` if that lane may call it. An
   unclassified route fails the test.
2. Regenerate the spec and every copy of it:

   ```sh
   cargo run -p punktfunk-host -- openapi > api/openapi.json
   cp api/openapi.json docs-site/public/openapi.json
   (cd sdk && bun install && bun run gen)
   cargo test -p punktfunk-host mgmt::
   ```

3. Commit all three generated files. CI fails when `api/openapi.json` differs from what the binary
   serves, when the docs-site copy differs, or when `sdk/src/gen/punktfunk.ts` is stale. The web
   console regenerates its client on `bun run dev` and `bun run build`.
