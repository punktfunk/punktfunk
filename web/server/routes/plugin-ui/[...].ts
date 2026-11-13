// /plugin-ui/<id>-<exp>-<mac>/** → a plugin's loopback UI server (plugin-ui-surface §5), on the
// plugin listener only. The gate (middleware/auth.ts) has checked the page path; it is checked
// again here, since the path names the plugin. We look up the plugin's `{port, secret}`
// server-side, inject the secret as a bearer, strip the browser's cookie, and stream the response
// through (SSE included). An HTML page is buffered to carry the theme receiver. The prefix the
// plugin sees keeps the credential segment. The plugin only ever gets dialed on 127.0.0.1.
//
// In `vite dev` a middleware in vite.config.ts handles `/plugin-ui` instead, with the same rules.
import {
	defineEventHandler,
	getProxyRequestHeaders,
	getRequestURL,
	readRawBody,
	sendWebResponse,
	setResponseStatus,
} from "h3";
import { personPluginPage, personUiCredential } from "../../util/personPlugins";
import { pluginPage } from "../../util/pluginPage";
import { callPlugin, injectThemeReceiver } from "../../util/pluginProxy";

export default defineEventHandler(async (event) => {
	const { pathname, search } = getRequestURL(event);
	// A person's page dials their seat host's plugin with the credential their session reads.
	const person = pluginPage(pathname) ? null : personPluginPage(pathname);
	const page = pluginPage(pathname) ?? person;
	if (!page) {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	const { id, prefix, rest } = page;

	// Forwardable request headers (h3 strips hop-by-hop + host); we set our own auth and drop the
	// session cookie so plugin code never sees it.
	const headers = getProxyRequestHeaders(event) as Record<string, string>;
	delete headers.cookie;
	delete headers.authorization;
	headers["x-forwarded-prefix"] = prefix;
	const method = event.method;
	// Only read a body for the methods that can carry one. `readRawBody` asserts a payload method,
	// so an OPTIONS the gate does not answer as a preflight would 405 before the plugin is dialed.
	const body = BODY_METHODS.has(method)
		? ((await readRawBody(event, false)) as Uint8Array | undefined)
		: undefined;

	let resp = await callPlugin(
		id,
		`${rest}${search}`,
		{ method, headers, body, redirect: "manual" },
		person ? (o) => personUiCredential(person.session, id, o) : undefined,
	);
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
