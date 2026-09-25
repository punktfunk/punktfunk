// Server-side helper for the plugin-UI reverse proxy (plugin-ui-surface §5). The console proxies
// `/plugin-ui/<id>/**` to a plugin's loopback UI server, injecting the plugin's per-boot secret —
// which it fetches here, from the management API, **server-side only** (the secret never reaches the
// browser; the BFF additionally denylists the credential endpoint from the generic passthrough).
//
// The credential is cached briefly so a burst of iframe asset requests doesn't hammer the host. On a
// 401 from the plugin (its secret rotated on restart within the cache window) the proxy busts this
// cache and re-fetches once — see the route.
import { loopbackTls, mgmtToken, mgmtUrl } from "./auth";
import { consoleOriginPort, pluginOriginPort } from "./pluginOrigin";

/** A plugin id — its `definePlugin` name; the same shape the host validates. */
export const PLUGIN_ID_RE = /^[a-z][a-z0-9-]*$/;

/** The proxy credential for a plugin's loopback UI. */
export interface UiCredential {
	port: number;
	secret: string;
}

const TTL_MS = 15_000;
const cache = new Map<string, { cred: UiCredential | null; at: number }>();

/** Drop a cached credential (called when a plugin's secret proved stale). */
export function bustCredential(id: string): void {
	cache.delete(id);
}

/**
 * Is this a port we are willing to dial on the operator's behalf?
 *
 * The port is a REGISTRY value, not ours: a plugin declares it when it registers, so it is
 * attacker-chosen the moment anyone can write the registry — and both callers turn it straight into
 * a loopback fetch. Our OWN two listeners are the ports that must never be reachable that way: the
 * proxy would then dial itself, and `/plugin-ui/**` on the plugin origin recurses (each hop opening
 * another) until the process runs out. Nothing legitimate can name them anyway — they are bound.
 *
 * This is not a general SSRF cure: any loopback port a plugin may legitimately serve on is by
 * definition dialable. It closes the self-dial, and keeps a malformed value from being pasted into
 * a URL.
 */
export function isDialablePort(port: number): boolean {
	if (!Number.isInteger(port) || port < 1 || port > 65535) return false;
	return port !== consoleOriginPort() && port !== pluginOriginPort();
}

/**
 * Fetch `{port, secret}` for a plugin's UI from the management API (bearer, loopback). Returns
 * `null` when the plugin isn't registered / has no UI (a 404), or when the port it registered is one
 * we refuse to dial (see {@link isDialablePort}). Results are cached for {@link TTL_MS};
 * pass `bustCache` to force a fresh read (the stale-secret retry). Throws only on a missing mgmt
 * token (a deploy misconfig) — a transient upstream error resolves to `null` (treated as offline)
 * and is not cached.
 */
export async function fetchUiCredential(
	id: string,
	opts?: { bustCache?: boolean },
): Promise<UiCredential | null> {
	const now = Date.now();
	if (!opts?.bustCache) {
		const hit = cache.get(id);
		if (hit && now - hit.at < TTL_MS) return hit.cred;
	}

	const base = mgmtUrl();
	const token = mgmtToken();
	if (!token) {
		throw new Error(
			"management token not configured (PUNKTFUNK_MGMT_TOKEN / ~/.config/punktfunk/mgmt-token)",
		);
	}
	// The host serves the credential over HTTPS with its self-signed loopback cert; relax
	// verification for that one loopback hop only (the same scoping the /api BFF uses).
	const fetchOptions = loopbackTls(base) as RequestInit | undefined;
	const resp = await fetch(`${base}/api/v1/plugins/${id}/ui-credential`, {
		...fetchOptions,
		headers: { authorization: `Bearer ${token}` },
	});

	if (resp.ok) {
		const cred = (await resp.json()) as UiCredential;
		// A port we refuse to dial is treated exactly like "no UI registered" — the callers already
		// render that as offline, and the negative is cached so a planted entry can't be used to make
		// us re-ask the host on every asset request.
		if (!isDialablePort(cred.port)) {
			cache.set(id, { cred: null, at: now });
			return null;
		}
		cache.set(id, { cred, at: now });
		return cred;
	}
	if (resp.status === 404) {
		// Definitively not running / no UI — cache the negative so a dead iframe doesn't spin.
		cache.set(id, { cred: null, at: now });
		return null;
	}
	// Transient (401/5xx): don't cache, let the next request retry.
	return null;
}

/**
 * One request to a plugin's loopback surface (`/__config`, `/__game?entry=…`, `/__metadata/…`)
 * with its secret.
 * A 401 means the secret rotated inside the cache window, so it retries once with a fresh one.
 * `null` means unreachable. Callers read `body` before calling: a retry must not resend an
 * emptied stream.
 */
export async function callPlugin(
	id: string,
	path: string,
	method: "GET" | "PUT" | "POST",
	body?: Uint8Array,
): Promise<Response | null> {
	const attempt = async (bustCache: boolean): Promise<Response | null> => {
		const cred = await fetchUiCredential(id, { bustCache });
		if (!cred) return null;
		try {
			return await fetch(`http://127.0.0.1:${cred.port}${path}`, {
				method,
				headers: {
					authorization: `Bearer ${cred.secret}`,
					...(method !== "GET" ? { "content-type": "application/json" } : {}),
				},
				body: body as BodyInit | undefined,
			});
		} catch {
			return null;
		}
	};
	const res = await attempt(false);
	if (res?.status !== 401) return res;
	bustCredential(id);
	return attempt(true);
}

/** A plugin's answer as JSON, or its text wrapped as an error. */
export async function pluginJson(res: Response, id: string): Promise<unknown> {
	const text = await res.text();
	try {
		return JSON.parse(text) as unknown;
	} catch {
		return { error: text || `plugin ${id} answered ${res.status}` };
	}
}
