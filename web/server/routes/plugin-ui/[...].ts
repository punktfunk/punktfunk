// /plugin-ui/<id>/** → a plugin's loopback UI server (plugin-ui-surface §5). By the time we get
// here the gate (middleware/auth.ts) has confirmed a session — a plugin UI is reachable only by the
// logged-in operator, on the console's own origin, with no separate password. We look up the
// plugin's `{port, secret}` server-side, inject the secret as a bearer, strip the browser's cookie,
// and stream the response through (SSE included). An HTML page is buffered to carry the theme
// receiver. The plugin only ever gets dialed on 127.0.0.1.
//
// This route runs in the built Bun/Nitro server. In `vite dev` a small middleware in vite.config.ts
// handles `/plugin-ui` instead (it intercepts before this route, like the /api dev proxy).
import {
	defineEventHandler,
	getProxyRequestHeaders,
	getRequestURL,
	readRawBody,
	sendWebResponse,
	setResponseStatus,
} from "h3";
import { mgmtFetch } from "../../util/forward";
import {
	bustCredential,
	fetchUiCredential,
	hostRelayPath,
	injectThemeReceiver,
	PLUGIN_ID_RE,
	viaHost,
} from "../../util/pluginProxy";

export default defineEventHandler(async (event) => {
	const { pathname, search } = getRequestURL(event);
	// /plugin-ui/<id>/<rest…>
	const m = pathname.match(/^\/plugin-ui\/([^/]+)(\/.*)?$/);
	const id = m?.[1];
	if (!id || !PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 404);
		return { error: "not a valid plugin-ui path" };
	}
	const rest = m?.[2] ?? "/";
	const prefix = `/plugin-ui/${id}`;

	// Forwardable request headers (h3 strips hop-by-hop + host); we set our own auth and drop the
	// session cookie so plugin code never sees it.
	const headers = getProxyRequestHeaders(event) as Record<string, string>;
	delete headers.cookie;
	delete headers.authorization;
	headers["x-forwarded-prefix"] = prefix;
	const method = event.method;
	// Only read a body for the methods that can carry one. `readRawBody` asserts a payload method,
	// so calling it for OPTIONS (a plugin UI's CORS preflight, or any client probing Allow) threw
	// 405 out of the CONSOLE before the plugin was ever dialed.
	const body = BODY_METHODS.has(method)
		? ((await readRawBody(event, false)) as Uint8Array | undefined)
		: undefined;

	// One proxied attempt; `null` means the plugin is unreachable (unregistered, or its port died).
	const attempt = async (bustCache: boolean): Promise<Response | null> => {
		// `null` also covers a port we refuse to dial — the plugin declared it when it registered, so
		// it is not ours to trust (see isDialablePort in util/pluginProxy.ts).
		const cred = await fetchUiCredential(id, { bustCache });
		if (!cred) return null;
		try {
			// A plugin with no listener: the host relays, and adds the plugin's secret itself.
			if (viaHost(cred)) {
				return await mgmtFetch(hostRelayPath(id, `${rest}${search}`), {
					method,
					headers,
					body: body as BodyInit | undefined,
					redirect: "manual",
				});
			}
			return await fetch(`http://127.0.0.1:${cred.port}${rest}${search}`, {
				method,
				headers: { ...headers, authorization: `Bearer ${cred.secret}` },
				body: body as BodyInit | undefined,
				redirect: "manual",
			});
		} catch {
			// The port is dead (plugin crashed/restarted on a new port): drop the stale credential so
			// the next request re-resolves it.
			bustCredential(id);
			return null;
		}
	};

	let resp = await attempt(false);
	// Stale secret after a plugin restart (S7): the plugin rejects our cached secret — re-fetch once.
	if (resp?.status === 401) {
		const retry = await attempt(true);
		if (retry) resp = retry;
	}
	if (!resp) {
		setResponseStatus(event, 502);
		return { error: `plugin "${id}" is not running` };
	}
	// Every plugin page follows the console's theme, published plugins included.
	if (resp.body && resp.headers.get("content-type")?.includes("text/html")) {
		resp = new Response(injectThemeReceiver(await resp.text()), resp);
	}
	return sendWebResponse(event, sanitize(resp));
});

/** Methods that may carry a request body. Anything else (GET, HEAD, OPTIONS, TRACE) must not be
 * handed to `readRawBody`. */
const BODY_METHODS = new Set(["POST", "PUT", "PATCH", "DELETE"]);

/**
 * Rebuild a plugin's response before it goes out on the console's own origin.
 *
 * An ALLOWLIST, not a denylist. A plugin UI is proxied same-origin by design, so any header it
 * returns is asserted for the console itself — and the first version of this dropped four names it
 * had thought of. `Clear-Site-Data: "*"` from a plugin's error page was not one of them: the
 * browser would honour it for this origin and wipe `pf_session`, signing the operator out of the
 * console because a plugin 500'd. Same shape for a plugin-supplied `Content-Security-Policy`,
 * `X-Frame-Options` or `Access-Control-Allow-Origin` — all of which would speak for us.
 *
 * So: name what a plugin page legitimately needs, and drop the rest. Framing headers
 * (content-encoding/length, transfer-encoding) are deliberately absent — `fetch` already decoded
 * the body, so re-emitting the plugin's originals made compressed pages fail to decode; ours are
 * recomputed.
 */
const PLUGIN_HEADER_ALLOWLIST = new Set([
	"content-type",
	"cache-control",
	"etag",
	"last-modified",
	"expires",
	"vary",
	"content-language",
	"content-disposition",
	"accept-ranges",
	"content-range",
	"location", // its own redirects, within its own prefix
	"link", // preload hints for its own assets
	"x-forwarded-prefix",
]);

function sanitize(resp: Response): Response {
	const headers = new Headers();
	for (const [k, v] of resp.headers) {
		if (PLUGIN_HEADER_ALLOWLIST.has(k.toLowerCase())) headers.set(k, v);
	}
	// 204/304 must not carry a body — passing one through throws in the Response constructor.
	const bodyless = resp.status === 204 || resp.status === 304;
	return new Response(bodyless ? null : resp.body, {
		status: resp.status,
		statusText: resp.statusText,
		headers,
	});
}
