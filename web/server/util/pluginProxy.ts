// Server-side helper for the plugin-UI reverse proxy (plugin-ui-surface §5). The console proxies
// `/plugin-ui/<id>/**` to a plugin's loopback UI server, injecting the plugin's per-boot secret —
// which it fetches here, from the management API, **server-side only** (the secret never reaches the
// browser; the BFF additionally denylists the credential endpoint from the generic passthrough).
//
// The credential is cached briefly so a burst of iframe asset requests doesn't hammer the host. On a
// 401 from the plugin (its secret rotated on restart within the cache window) `callPlugin` busts this
// cache and re-fetches once.
import { isError } from "h3";
import { mgmtFetch } from "./forward";
import { consoleOriginPort, pluginOriginPort } from "./pluginOrigin";

/** A plugin id — its `definePlugin` name; the same shape the host validates. */
export const PLUGIN_ID_RE = /^[a-z][a-z0-9-]*$/;

/**
 * A library id the host accepts: `<store>:<external id>`, both halves non-empty, at most 1024
 * UTF-8 bytes. Control characters are refused too. clients/shared/library-id-vectors.json pins it.
 */
export const validEntryId = (v: unknown): v is string => {
	if (typeof v !== "string") return false;
	const colon = v.indexOf(":");
	return (
		colon > 0 &&
		colon < v.length - 1 &&
		new TextEncoder().encode(v).length <= 1024 &&
		![...v].some((c) => c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f)
	);
};

/** The proxy credential for a plugin's loopback UI. */
export interface UiCredential {
	port: number;
	secret: string;
}

/**
 * Port 0: the plugin has no listener (a Windows AppContainer can bind nothing), and the host
 * relays to it over the plugin's own channel. The request goes to the management API under
 * `/plugins/<id>/ui/`, and the host adds the plugin's secret itself.
 */
export const viaHost = (cred: UiCredential): boolean => cred.port === 0;

/** Where the management API relays a request for this plugin's page. */
export const hostRelayPath = (id: string, pathAndQuery: string): string =>
	`/api/v1/plugins/${id}/ui${pathAndQuery}`;

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
 * Fetch `{port, secret}` for a plugin's UI from the management API. Returns `null` when the plugin
 * isn't registered / has no UI (a 404), or when the port it registered is one we refuse to dial
 * (see {@link isDialablePort}). Results are cached for {@link TTL_MS}; pass `bustCache` to force a
 * fresh read (the stale-secret retry). Throws only on a missing mgmt token (a deploy misconfig): an
 * unreachable host or any other upstream error resolves to `null` (offline) and is not cached.
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

	const resp = await mgmtFetch(`/api/v1/plugins/${id}/ui-credential`).catch(
		(e: unknown) => {
			if (isError(e) && e.statusCode === 503) throw e;
			return null;
		},
	);
	if (!resp) return null;
	if (resp.ok) {
		const cred = (await resp.json()) as UiCredential;
		// A port we refuse to dial is treated exactly like "no UI registered" — the callers already
		// render that as offline, and the negative is cached so a planted entry can't be used to make
		// us re-ask the host on every asset request.
		if (!viaHost(cred) && !isDialablePort(cred.port)) {
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
	// Transient (5xx): don't cache, let the next request retry.
	return null;
}

/** One request to a plugin. `body` and `headers` go through as given; the dial adds only the bearer. */
interface PluginRequest {
	method: string;
	headers?: Record<string, string>;
	body?: Uint8Array;
	redirect?: RequestRedirect;
}

/**
 * The only dial to a plugin's UI server: `pathAndQuery` on 127.0.0.1 with its secret, or through
 * the host's relay for a plugin with no listener.
 *
 * A 401 means the secret rotated inside the cache window, so it retries once with a fresh
 * credential, and keeps the first answer when the retry finds no plugin. A throw (the port died)
 * busts the credential. `null` means unreachable. Callers read `body` before calling: a retry must
 * not resend an emptied stream.
 */
export async function callPlugin(
	id: string,
	pathAndQuery: string,
	{ headers = {}, body, ...init }: PluginRequest,
): Promise<Response | null> {
	const attempt = async (bustCache: boolean): Promise<Response | null> => {
		// `null` also covers a port we refuse to dial (see isDialablePort).
		const cred = await fetchUiCredential(id, { bustCache });
		if (!cred) return null;
		const sent = { ...init, body: body as BodyInit | undefined };
		try {
			if (viaHost(cred)) {
				return await mgmtFetch(hostRelayPath(id, pathAndQuery), {
					...sent,
					headers,
				});
			}
			return await fetch(`http://127.0.0.1:${cred.port}${pathAndQuery}`, {
				...sent,
				headers: { ...headers, authorization: `Bearer ${cred.secret}` },
			});
		} catch {
			bustCredential(id);
			return null;
		}
	};
	const res = await attempt(false);
	if (res?.status !== 401) return res;
	return (await attempt(true)) ?? res;
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

/**
 * The frame's half of `pf-ui:theme`: asks the console for its palette, then sets each reply as
 * inline custom properties on `<html>`, which outrank plugin-kit's `:root` and `.dark`. Only the
 * parent is heard; the plugin origin's `frame-ancestors` makes that the console.
 */
const THEME_RECEIVER = `<script>(()=>{if(parent===window)return;const r=document.documentElement;addEventListener("message",e=>{const d=e.data;if(e.source!==parent||d?.type!=="pf-ui:theme")return;r.classList.toggle("dark",!!d.dark);r.style.colorScheme=d.dark?"dark":"light";for(const[k,v]of Object.entries(d.tokens??{}))if(k.startsWith("--")&&typeof v==="string")r.style.setProperty(k,v)});parent.postMessage({type:"pf-ui:theme-request"},"*")})()</script>`;

/** Put {@link THEME_RECEIVER} first in `<head>`, ahead of the stylesheets that would delay it. */
export const injectThemeReceiver = (html: string): string =>
	html.replace(/<head\b[^>]*>/i, (tag) => tag + THEME_RECEIVER);
