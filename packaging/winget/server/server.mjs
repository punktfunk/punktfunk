// HTTP front for the winget REST source. Runs on unom-1 under a stock `oven/bun` image with this
// file and the data bind-mounted — same shape as the flatpak server (stock caddy:2-alpine + a
// bind-mounted Caddyfile), so there is no image to build, publish or pull.
//
// Plain HTTP on :3240. Caddy on the same box terminates TLS for winget.punktfunk.unom.io and
// reverse-proxies here — that TLS is not optional decoration: `winget source add` refuses anything
// but HTTPS with a publicly trusted certificate.
//
// Zero dependencies on purpose. The catalogue is a generated JSON file; parsing YAML and talking to
// Gitea is build-data.mjs's job, which runs in CI. Nothing here needs node_modules, so the deploy
// is two files and `docker compose up -d`.
import { readFileSync, statSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { respond } from "./handler.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const DATA_PATH = resolve(process.env.PF_WINGET_DATA ?? `${HERE}/data/data.json`);
const PORT = Number(process.env.PORT ?? 3240);

// Reload on mtime change rather than caching for the process lifetime. Publishing a release copies
// a new data.json onto the box; without this it would serve the old catalogue until someone
// remembered to restart the container. A stat per request is free, and it
// keeps "deploy" and "publish" decoupled the way the flatpak repo already is.
let cache = { mtimeMs: 0, data: null, error: null };

function catalogue() {
  let mtimeMs;
  try {
    mtimeMs = statSync(DATA_PATH).mtimeMs;
  } catch (e) {
    // Keep serving the last good catalogue if the file goes missing mid-flight (e.g. a partial
    // copy) — a transient publish must not take the source down.
    if (cache.data) return cache.data;
    cache.error = `cannot stat ${DATA_PATH}: ${e.message}`;
    return null;
  }
  if (cache.data && mtimeMs === cache.mtimeMs) return cache.data;
  try {
    const parsed = JSON.parse(readFileSync(DATA_PATH, "utf8"));
    if (!Array.isArray(parsed?.packages)) throw new Error("no `packages` array");
    cache = { mtimeMs, data: parsed, error: null };
    console.log(`[winget] loaded ${parsed.packages.length} package(s) from ${DATA_PATH}`);
    return parsed;
  } catch (e) {
    // A half-written file during the copy parses as garbage; prefer stale-but-valid over an outage.
    if (cache.data) {
      console.error(`[winget] reload failed, keeping previous catalogue: ${e.message}`);
      return cache.data;
    }
    cache.error = `cannot parse ${DATA_PATH}: ${e.message}`;
    return null;
  }
}

/** node:http request -> WHATWG Request, so handler.mjs stays runtime-agnostic. */
function toRequest(req) {
  const url = `http://${req.headers.host ?? "localhost"}${req.url}`;
  const init = { method: req.method, headers: req.headers };
  if (req.method !== "GET" && req.method !== "HEAD") {
    init.body = req;
    init.duplex = "half"; // required when a stream is used as a body
  }
  return new Request(url, init);
}

const server = createServer(async (req, res) => {
  // Liveness for compose/monitoring — deliberately outside the winget routes, so a missing or
  // unparseable catalogue answers 503 here without involving a winget route.
  if (req.url === "/healthz") {
    const data = catalogue();
    const ok = !!data;
    res.writeHead(ok ? 200 : 503, { "content-type": "application/json; charset=utf-8" });
    res.end(JSON.stringify(ok ? { status: "ok", packages: data.packages.length } : { status: "error", error: cache.error }));
    return;
  }

  const data = catalogue();
  if (!data) {
    res.writeHead(503, { "content-type": "application/json; charset=utf-8" });
    res.end(JSON.stringify({ ErrorCode: 503, ErrorMessage: cache.error ?? "catalogue unavailable" }));
    return;
  }

  let response;
  try {
    response = await respond(toRequest(req), data);
  } catch (e) {
    res.writeHead(500, { "content-type": "application/json; charset=utf-8" });
    res.end(JSON.stringify({ ErrorCode: 500, ErrorMessage: String(e?.message ?? e) }));
    return;
  }

  const headers = Object.fromEntries(response.headers.entries());
  const body = response.status === 204 ? null : await response.text();
  res.writeHead(response.status, headers);
  res.end(body ?? undefined);
});

server.listen(PORT, () => {
  console.log(`[winget] REST source on :${PORT}, data=${DATA_PATH}`);
  // Warm the cache at boot so a bad mount is loud immediately, not on the first user request.
  if (!catalogue()) console.error(`[winget] WARNING: ${cache.error}`);
});

for (const sig of ["SIGTERM", "SIGINT"]) {
  process.on(sig, () => server.close(() => process.exit(0)));
}
