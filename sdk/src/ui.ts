// `servePluginUi` (plugin-ui-surface design §4) — the whole plugin side of a console-hosted UI in
// one call. A plugin serves its UI on a **loopback ephemeral port** behind a **per-boot secret**,
// registers `{title, ui:{port, secret, icon}}` with the host, and renews the lease on a timer; the
// web console reverse-proxies to it and grows a nav entry. The plugin author writes zero human auth,
// discovery, or TLS — all of that lives here.
//
//   import { definePlugin, servePluginUi } from "@punktfunk/host";
//
//   export default definePlugin({
//     name: "rom-manager",
//     main: async (pf) => {
//       const ui = await servePluginUi(pf, {
//         id: "rom-manager", title: "ROM Manager", icon: "gamepad-2",
//         staticDir: new URL("../dist/ui", import.meta.url),   // built SPA
//         fetch: (req) => appRouter(req),                       // plugin-local REST/SSE
//       });
//       try { await runEngineForever(); } finally { await ui.close(); }
//     },
//   });
//
// Design notes:
//   - **Runtime**: Bun (the scripting runner IS bun; a `node:http` lane is deferred — design Q1).
//   - **Registration uses `pf.request`, not `pf.api.*`** (design D7): under the packaged runner the
//     facade is built by the runner's *bundled* SDK copy, whose generated client may predate the
//     `/plugins` endpoints; the untyped request seam has existed since 0.1.0 and is skew-proof.
//   - **The host only ever dials 127.0.0.1:<port>** — we register a port, never an address (D5).
import { createHash, timingSafeEqual } from "node:crypto";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import type { Punktfunk } from "./index.js";

/** How often the lease is renewed (host TTL is 90 s — two missed ticks of slack). */
const DEFAULT_RENEW_MS = 30_000;

export interface PluginUiOptions {
	/**
	 * The plugin's registered id — its `definePlugin` name (`[a-z][a-z0-9-]*`). The console nav
	 * entry and the proxy path `/plugin-ui/<id>/**` key on this.
	 */
	id: string;
	/** Human-readable title for the console nav entry. */
	title: string;
	/** Optional plugin version (informational, shown in the console page header). */
	version?: string;
	/** Optional lucide icon name for the nav entry (`[a-z0-9-]`, e.g. `"gamepad-2"`). */
	icon?: string;
	/**
	 * What KIND of plugin this is (`[a-z][a-z0-9-]{0,31}`). The console groups and filters on it —
	 * and notably keeps `"library"` plugins **out of the nav**, because a scanner's entry point is
	 * the Library section's Game sources surface, not a sidebar item of its own. Six installed
	 * scanners would otherwise flood the sidebar.
	 *
	 * `@punktfunk/plugin-kit`'s `defineLibraryPlugin` sets this for you. Set it by hand only if you
	 * are building a library plugin without the kit — and omit it if your plugin wants a full page
	 * despite also syncing a library (rom-manager does).
	 */
	category?: string;
	/**
	 * Which console surfaces the plugin serves. `page`: a page the console opens and lists in the
	 * nav (the host assumes one when this is absent). `config`: `GET/PUT /__config`, the settings
	 * form. `game`: `GET/PUT /__game?entry=<id>`, a tab on each library entry's page. Sent only
	 * when set, so an older host ignores them.
	 */
	surfaces?: { page?: boolean; config?: boolean; game?: boolean };
	/**
	 * Stages this plugin holds, like `"game.launching"`. The host POSTs the event to `/__hold` and
	 * waits for a 2xx, up to `holdTimeoutMs` (default 30 000, at most 120 000). Needs `fetch` to
	 * answer `/__hold`; `@punktfunk/plugin-kit`'s `serveUi({ holds })` does.
	 */
	holds?: readonly string[];
	holdTimeoutMs?: number;
	/**
	 * Directory of the built SPA. Requests are served from here first (with an `index.html` SPA
	 * fallback for navigations); a static miss falls through to [`fetch`]. Accepts a filesystem
	 * path or a `file:` URL (`new URL("../dist/ui", import.meta.url)`).
	 */
	staticDir?: string | URL;
	/**
	 * The plugin's own dynamic handler (REST, SSE) — tried after a static miss. Paths arrive
	 * **prefix-stripped** (the console proxy has already removed `/plugin-ui/<id>`), so this sees
	 * `/`, `/api/scan`, … The original public prefix is on the `X-Forwarded-Prefix` header if you
	 * need absolute self-URLs. Return `undefined` to fall through to the SPA fallback.
	 */
	fetch?: (req: Request) => Response | Promise<Response | undefined> | undefined;
	/** Advanced: lease-renewal cadence in ms (default 30 000). Mainly for tests. */
	renewIntervalMs?: number;
}

export interface PluginUiHandle {
	/** The loopback port the UI is bound to. */
	readonly port: number;
	/** `http://127.0.0.1:<port>` — the base the console proxy dials. */
	readonly url: string;
	/** Deregister and stop the server (best-effort DELETE, then force-close). */
	close(): Promise<void>;
}

const warn = (m: string) => console.warn(`[punktfunk] servePluginUi: ${m}`);

/** A fresh per-boot secret: 32 random bytes as base64url (43 chars, `[A-Za-z0-9_-]`). */
const mintSecret = (): string => {
	const bytes = new Uint8Array(32);
	crypto.getRandomValues(bytes);
	return Buffer.from(bytes).toString("base64url");
};

/** Resolve a request path to an absolute file inside `root`, or `null` if it escapes (traversal). */
const staticFile = (root: string, pathname: string): string | null => {
	let rel: string;
	try {
		rel = decodeURIComponent(pathname);
	} catch {
		return null; // malformed %-encoding
	}
	if (rel.endsWith("/")) rel += "index.html";
	if (!rel.startsWith("/")) rel = `/${rel}`;
	const abs = path.resolve(root, `.${rel}`);
	const rootAbs = path.resolve(root);
	if (abs !== rootAbs && !abs.startsWith(rootAbs + path.sep)) return null;
	return abs;
};

/**
 * Serve a plugin UI and register it with the host. Returns once the server is listening and the
 * first registration attempt has been made (a failed initial register is warned, not thrown — the
 * renewal loop keeps trying, so a momentarily-unreachable host doesn't take the plugin down).
 */
export const servePluginUi = async (
	pf: Punktfunk,
	opts: PluginUiOptions,
): Promise<PluginUiHandle> => {
	if (!/^[a-z][a-z0-9-]*$/.test(opts.id)) {
		throw new Error(
			`servePluginUi: id "${opts.id}" must be kebab-case ([a-z][a-z0-9-]*)`,
		);
	}
	if (typeof (globalThis as Record<string, unknown>).Bun === "undefined") {
		throw new Error(
			"servePluginUi requires the Bun runtime (the scripting runner is bun); a Node lane is not yet available",
		);
	}

	const root = opts.staticDir
		? typeof opts.staticDir === "string"
			? opts.staticDir
			: fileURLToPath(opts.staticDir)
		: undefined;

	// One per-boot secret; the console proxy must present it (as a bearer) on every request. Compared
	// constant-time against its SHA-256 (mirrors the host's `token_eq`), so no length/content timing.
	const secret = mintSecret();
	const secretHash = createHash("sha256").update(secret).digest();
	const authorized = (req: Request): boolean => {
		const header = req.headers.get("authorization");
		const presented = header?.startsWith("Bearer ") ? header.slice(7) : undefined;
		if (presented === undefined) return false;
		const presentedHash = createHash("sha256").update(presented).digest();
		return timingSafeEqual(presentedHash, secretHash);
	};

	const server = Bun.serve({
		hostname: "127.0.0.1", // loopback only — nothing off-box can reach it
		port: 0, // ephemeral: no port to configure or collide
		async fetch(req) {
			if (!authorized(req)) {
				return new Response("unauthorized", { status: 401 });
			}
			const pathname = new URL(req.url).pathname;
			// Built-in liveness — the console page probes this before mounting the iframe.
			if (pathname === "/__health") {
				return Response.json({ ok: true, id: opts.id, title: opts.title });
			}
			// 1) static asset
			if (root) {
				const file = staticFile(root, pathname);
				if (file) {
					const bf = Bun.file(file);
					if (await bf.exists()) return new Response(bf);
				}
			}
			// 2) the plugin's dynamic handler
			if (opts.fetch) {
				const res = await opts.fetch(req);
				if (res) return res;
			}
			// 3) SPA fallback: a navigation that matched no asset gets index.html
			if (
				root &&
				req.method === "GET" &&
				(req.headers.get("accept") ?? "").includes("text/html")
			) {
				const index = Bun.file(path.join(root, "index.html"));
				if (await index.exists()) return new Response(index);
			}
			return new Response("not found", { status: 404 });
		},
	});

	const port = server.port;
	if (port == null) throw new Error("Bun.serve did not report a bound port");
	const url = `http://127.0.0.1:${port}`;
	const body = {
		title: opts.title,
		...(opts.version !== undefined ? { version: opts.version } : {}),
		ui: {
			port,
			secret,
			...(opts.icon !== undefined ? { icon: opts.icon } : {}),
			...opts.surfaces,
		},
		// Sent through the UNTYPED `pf.request` below, so an older host simply ignores the unknown
		// field rather than rejecting the registration — no runner flag, no version gate.
		...(opts.category !== undefined ? { category: opts.category } : {}),
		...(opts.holds?.length ? { holds: opts.holds } : {}),
		...(opts.holdTimeoutMs !== undefined
			? { hold_timeout_ms: opts.holdTimeoutMs }
			: {}),
	};

	const register = () => pf.request("PUT", `/plugins/${opts.id}`, body);
	// Best-effort initial register: warn but keep the server up if the host is momentarily away.
	await register().catch((e) => warn(`initial registration failed: ${e}`));
	const timer = setInterval(() => {
		register().catch((e) => warn(`lease renewal failed: ${e}`));
	}, opts.renewIntervalMs ?? DEFAULT_RENEW_MS);
	// Don't let the renewal timer alone keep the process alive — the plugin's main loop owns lifetime.
	(timer as { unref?: () => void }).unref?.();

	return {
		port,
		url,
		async close() {
			clearInterval(timer);
			// Deregister promptly so the nav entry drops without waiting for the lease to expire.
			await pf.request("DELETE", `/plugins/${opts.id}`).catch(() => {});
			server.stop(true); // force-close (SSE/long-poll connections included)
		},
	};
};
