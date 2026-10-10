# punktfunk web — management console

The browser UI for the punktfunk host's **management REST API** (`crates/host/punktfunk-host/src/mgmt.rs`,
OpenAPI at `api/openapi.json`). It shows live status, host capabilities, paired
clients, the pairing-PIN flow, and session controls.

Stack: **TanStack Start** (full SSR) on **Bun** via **Nitro v2** (`bun` preset) · **React
Query** through **orval** codegen from the OpenAPI spec · **[`@unom/ui`](https://git.unom.io/unom/ui)**
— the shared punktfunk/unom design system the marketing site + docs are built on (Tailwind v4,
animated components on the violet brand over dark chrome) ·
**Paraglide** i18n (en/de). Package manager + runtime: **Bun**.

The `@unom` registry mapping lives in [`.npmrc`](.npmrc); the auth token comes from
`~/.npmrc` (or a CI secret).

## Develop

```sh
# from web/  — Bun is the toolchain (https://bun.sh)
bun install               # runs `prepare` → codegen (orval + paraglide)
bun run dev               # http://localhost:47992

# The dev server proxies /api → https://127.0.0.1:47990 (the host's mgmt API; it serves HTTPS
# with the host's self-signed identity cert — the dev proxy uses `secure: false`).
# Point it elsewhere: PUNKTFUNK_MGMT_URL=https://<host>:47990 bun run dev
```

Start a host with the management API up:

```sh
# from the repo root — `serve` brings up the native punktfunk/1 plane + the mgmt API (the console
# only needs the mgmt API; add --gamestream too if you also want the Moonlight surface):
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE \
  cargo run -rp punktfunk-host -- serve
# loopback :47990, no token (a token is mandatory for non-loopback binds).
```

The management token is **server-side only** — set `PUNKTFUNK_MGMT_TOKEN` in the console's
environment and the BFF injects it when proxying (`server/routes/api/[...].ts`). It never reaches
the browser, so there is no token field in the UI; the browser only ever holds the session cookie.

## Build & run (Nitro + Bun)

The console runs on **bun** (`Bun.serve` is a Bun API — node can't run it): Nitro's `bun` preset
plus a custom entry (`nitro-entry/bun-https.mjs`) that calls `Bun.serve({ tls })`, so it serves
**HTTPS (HTTP/1.1 over TLS)** with the **host's own identity cert** (the cert native clients already
pin). One trust anchor across the data plane, the mgmt API, and this console. (No HTTP/2 — `Bun.serve`
has no h2 server — and no HTTP/3, which a browser won't speak against this self-signed, no-SAN host
cert; a browser-trusted, SAN-matching cert + a fronting server would be needed, out of scope for a
LAN console.)

```sh
bun run build             # → .output/  (Nitro `bun` preset + our Bun.serve TLS entry)
PORT=47992 HOST=0.0.0.0 \
  PUNKTFUNK_UI_PASSWORD=… PUNKTFUNK_MGMT_TOKEN=… \
  PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990 \
  PUNKTFUNK_UI_TLS_CERT=~/.config/punktfunk/cert.pem \
  PUNKTFUNK_UI_TLS_KEY=~/.config/punktfunk/key.pem PUNKTFUNK_UI_SECURE=1 \
  bun run start           # = bun run .output/server/index.mjs
# PUNKTFUNK_UI_TLS_* unset ⇒ plain HTTP (local dev); both set ⇒ HTTPS (HTTP/1.1 over TLS).
# Naming cert.pem/key.pem serves native-cert.pem/native-key.pem instead when both sit beside them
# (the identity split — nitro-entry/tls-paths.mjs); the legacy pair is the fallback, not the target.
# The host's self-signed mgmt cert is accepted only for the proxy's loopback hop, scoped in code
# (Bun per-request TLS: server/routes/api/[...].ts) — no process-wide NODE_TLS_REJECT_UNAUTHORIZED.
# See .env.example.
bun run lint              # tsc --noEmit
```

The built **Nitro bun server** SSR-renders the app and is the only thing exposed on the LAN.
Run it on the same box as the host; it serves the console over HTTPS on `:47992` (or `$PORT`).

## Auth (backend-for-frontend)

Single-user, login-gated. Config via env (see `.env.example`):

- The console requires a **login**, checked against `PUNKTFUNK_UI_PASSWORD_HASH` (salted
  argon2id, `Bun.password`). On success the server sets a **sealed session cookie** (h3
  `useSession`, AES-GCM). `server/middleware/auth.ts` gates *every* request — pages redirect to
  `/login`, `/api` returns 401 — and **fails closed** (503) when no password is configured.
- `PUNKTFUNK_UI_PASSWORD` (clear) still works for one release and is also the reset path: the
  first sign-in with it rewrites the password file as a hash, 0600, temp-file-and-rename.
- The **bearer-token admin surface of the management API is loopback-only** — the host honors a
  bearer token only from a loopback peer, so the admin API is never LAN-exposed. The web server
  holds `PUNKTFUNK_MGMT_TOKEN` server-side and injects it when proxying `/api/**` →
  `PUNKTFUNK_MGMT_URL` (loopback; `server/routes/api/[...].ts`). **The token never reaches the
  browser**; the browser only ever holds the session cookie. (The host *also* binds the
  **read-only** surface — host status + the game library — to the LAN so paired native clients can
  fetch it directly over mTLS; that path uses client certs, not the token, and never touches this
  console.)

So: `browser ──password──▶ web server (session cookie) ──mgmt token, server-side──▶ mgmt API`.
Run the host with a matching token: `cargo run -rp punktfunk-host -- serve` +
`PUNKTFUNK_MGMT_TOKEN=…` (or `--mgmt-token …`). `vite dev` has no gate (localhost-only) and
proxies straight to the loopback mgmt API.

> Toolchain notes (load-bearing): TanStack Start's `start-plugin-core` peer-requires
> **Vite ≥ 7** — on Vite 6 the build's prerender/post-build hook silently doesn't run.
> `@vitejs/plugin-react` must match Vite (v5 ↔ Vite 7, v6 ↔ Vite 8); it's **required even
> for dev** (TanStack Start's dev mode needs the React Refresh runtime, else a blank
> screen). Nitro is the server target — without it `vite build` only emits client+SSR
> bundles, no deployable server. The Nitro `bun` preset makes `.output/server/index.mjs`
> Bun-runnable.

## Codegen

Generated code is **not committed** (gitignored) — reproduced from sources:

- `bun run codegen` — regenerate the API client (orval) + i18n runtime (paraglide). Runs on
  `bun install` (`prepare`) and before `dev`/`build` (`pre*` for orval; the Vite plugin
  compiles paraglide on dev/build).
- After a management-API change, regenerate the spec on the Rust side first:
  `cargo run -p punktfunk-host -- openapi > api/openapi.json`, then `bun run api:gen`.

## Layout

```
src/
  routes/            file-based routes (index=dashboard, host, clients, pairing, settings)
  components/
    app-shell.tsx    sidebar nav (brand lens + wordmark) + language switcher
    brand-mark/wordmark/logo.tsx   punktfunk lens mark + wordmark (shared with the site/docs)
    ui/              @unom/ui-backed primitives (button, input, label, card; badge/table/skeleton)
    query-state.tsx  loading/error wrapper (401 → the session is gone, re-login)
  api/
    fetcher.ts       orval mutator: base URL, bearer token, JSON, throwing ApiError
    gen/             GENERATED react-query hooks + models (orval)
  lib/i18n.ts        reactive Paraglide locale hook
  paraglide/         GENERATED i18n runtime (paraglide)
messages/{en,de}.json   translation sources
```
