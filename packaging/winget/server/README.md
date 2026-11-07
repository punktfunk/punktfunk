# Punktfunk winget REST source

A self-hosted [winget REST source](https://github.com/microsoft/winget-cli-restsource) serving the
Punktfunk Windows host, so users can install and upgrade with `winget` instead of downloading a
`setup.exe` by hand.

```powershell
# one-time, from an ELEVATED prompt (winget requires admin to add a source)
winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest

winget install unom.PunktfunkHost                 # silent, wizard defaults
winget install unom.PunktfunkHost --interactive   # the full wizard
winget upgrade unom.PunktfunkHost
```

## Why self-hosted rather than the community repo

`microsoft/winget-pkgs` gates submissions on its `Binary-Validation-Error` /
`Validation-Defender-Error` checks, which need a publicly trusted signing cert. That blocker is
gone — the host installer is now signed with Azure Artifact Signing (see `packaging/windows/README.md`)
— but a self-hosted source is still the right call: it has no such gate, can carry `Agreements`
(verified-developers-only upstream), and can serve channels the community repo would never accept.

## What it implements

The winget client consumes exactly three endpoints. The reference implementation's other twenty
(`/packages/**`) are its **admin** API for mutating a CosmosDB; a catalogue generated at release
time has nothing to mutate, so they are not implemented.

| Endpoint | Notes |
| --- | --- |
| `GET /information` | Source identity, `ServerSupportedVersions`, declared capabilities. |
| `POST /manifestSearch` | `Query` / `Inclusions` (OR) / `Filters` (AND) / `FetchAllManifests`. **204** on no match — not an empty 200. |
| `GET /packageManifests/{id}` | Full manifest; honours `?Version=`. Identifier match is case-insensitive. |
| `GET /healthz` | Not part of the spec — liveness for compose. |

`NormalizedPackageNameAndPublisher` is deliberately declared **unsupported**. winget derives it
client-side from ARP entries with its own normalization (case folding plus stripping legal suffixes,
punctuation and version-like tails); claiming support would mean reimplementing that exactly, and a
near-miss silently mis-correlates an installed host. Correlation rides on `ProductCode` instead,
which Inno gives us exactly (`<AppId>_is1`).

## Layout

| File | Role |
| --- | --- |
| `handler.mjs` | The three endpoints. Pure `(Request, catalogue) -> Response`, no I/O, no deps. |
| `server.mjs` | node:http front. Loads the catalogue, reloads on mtime change. |
| `build-data.mjs` | Generates `data/data.json` from the published release manifests. Build-time only. |
| `test.mjs` | 30 checks driving `handler.mjs` directly. |
| `compose.production.yml` | The unom-1 service. |

## How it runs

```
Gitea releases (manifests per stable tag)
        │  build-data.mjs   ← walks ALL releases, no local state   [CI, Linux job]
        ▼
   data/data.json (~8 KB)   ← rsynced to unom-1 on each stable tag
        ▼
   bun + server.mjs on unom-1:3240  ←  edge Caddy TLS  ←  winget.punktfunk.unom.io
```

Stock `oven/bun` image with the two `.mjs` files bind-mounted — the same shape as the flatpak server
(stock `caddy:2-alpine` + a bind-mounted Caddyfile). There is no image to build, publish or pull, so
a code change deploys by scp plus `docker compose up -d`.

The server has **zero dependencies**: it reads a generated JSON file. Parsing YAML and talking to
Gitea is `build-data.mjs`'s job, and that runs in CI. Only that build step needs `node_modules`.

**Content and config deploy separately**, matching the flatpak repo:

- `deploy-services.yml` (`winget` job) ships `compose.production.yml` + the two `.mjs` files.
- `windows-host.yml` (`winget-source` job) builds and ships `data/data.json` on each stable tag.

`server.mjs` reloads the catalogue when its mtime changes, so publishing a release needs no restart.
A half-written file mid-rsync keeps the previous catalogue rather than taking the source down.

### Why the catalogue is derived from releases

`build-data.mjs` walks the Gitea releases rather than reading local files. That is what makes
`winget install --version` and `winget upgrade` work: winget resolves both against the version
*list*, so a source that only knew the newest release could neither pin an older version nor show an
upgrade path from one.

It also means the source holds no state — re-running the build reproduces the same `data.json`, so a
lost deployment is one command away and nothing can drift.

## Local development

```bash
npm install
npm run build:local     # data/data.json from ../*.yaml (single version — fine for dev)
npm test                # 30 checks, no network, no server
npm start               # http://localhost:3240
```

`npm test` is the gate that matters. A malformed response does not fail loudly — winget reports "no
package found", so a shape regression looks like a missing package rather than a broken source. CI
runs it before anything reaches the box.

Note a real `winget source add` will not accept a local instance: it requires HTTPS with a publicly
trusted certificate, which is what the edge vhost provides in production.

## First-time setup

1. **DNS** — `winget.punktfunk.unom.io` → the unom-1 hcloud box, in the `unom.io` Cloudflare zone
   (DNS-only, same as `docs`). ✅ *done*
2. **Caddy vhost** — in **`unom/infra`, `caddy/Caddyfile`**, next to the existing
   `docs.punktfunk.unom.io` block. ✅ *done*

   ```caddyfile
   winget.punktfunk.unom.io {
       import security_headers
       reverse_proxy localhost:3240
   }
   ```

   ⚠ **Not by hand on the box.** `~/caddy/Caddyfile` on unom-1 looks like the config but is a copy
   that `deploy-all.sh` rsyncs over from `unom/infra`, and there is no `.git` there to warn you. A
   vhost added only on the box survives until the next deploy and no longer: this one was
   hand-added on 2026-07-26, and the 2026-07-31 hardening commit rewrote the file from the repo's
   own copy and deleted it — its message says "all 7 vhosts" when there were 8 live.

   Until the vhost exists the hostname resolves but the TLS handshake fails — Caddy has no
   certificate for a name it does not serve. That is the expected state on first setup, not a
   broken deploy, but it is also exactly how the clobber presents later: an `internal_error`
   (alert 80) with no peer certificate, which winget reports as
   `WINHTTP_CALLBACK_STATUS_FLAG_SECURITY_CHANNEL_ERROR` / `0x8a15003b` and which looks like a
   client bug. Diagnose by SNI, not by port 80 — Caddy 308s *every* Host to https, including names
   it has never heard of, so a redirect there proves nothing:

   ```bash
   openssl s_client -connect winget.punktfunk.unom.io:443 \
     -servername winget.punktfunk.unom.io </dev/null 2>&1 | grep -E '^subject=|alert'
   ```
3. Run `deploy-services.yml` to place the compose file and start the container. It serves 503 until
   a catalogue exists — expected, and visible on `/healthz`.
4. Publish a stable tag, or build and ship the catalogue by hand:

```bash
cd packaging/winget/server && npm install && npm run build && npm test
scp data/data.json <deploy-user>@<unom-1>:~/unom-winget/data/data.json
```

Then verify from anywhere:

```bash
curl -s https://winget.punktfunk.unom.io/information | jq .
curl -s -X POST https://winget.punktfunk.unom.io/manifestSearch \
  -H 'content-type: application/json' -d '{"FetchAllManifests":true}' | jq '.Data[].PackageIdentifier'
```

> Releases published before winget support shipped have no manifests attached, so `npm run build`
> skips them. Until the first stable tag lands with them, build with `--local ..` to serve the
> checked-in manifests, or backfill the manifests onto an existing release.
