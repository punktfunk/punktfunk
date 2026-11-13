// Each plugin page is a sandboxed document with an opaque origin of its own, and the session cookie
// never counts on the plugin listener. A page's credential is its path, `/plugin-ui/<id>-<exp>-<mac>/`,
// which relative URLs inherit. It stays inside the first segment, in `[a-z0-9-]`, because published
// plugin-kit builds' `resolvePluginBase()` keeps exactly that segment as every API call's prefix.
import { createHmac } from "node:crypto";
import { sessionConfig, sessionEpoch } from "./auth";
import { safeEqualHex } from "./handoff";

/** The CSP `sandbox` every plugin listener response carries, framed or opened in its own tab. */
export const PLUGIN_PAGE_SANDBOX =
	"sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads";

/** CORS for a page's own requests, which come from `Origin: null`. Never with credentials. */
export const PLUGIN_PAGE_CORS: Readonly<Record<string, string>> = {
	"access-control-allow-origin": "*",
	"access-control-expose-headers": "*",
};

/** The answer to a preflight on a valid page path, echoing the headers it asked for. */
export const pluginPagePreflight = (
	askedHeaders: string | undefined,
): Record<string, string> => ({
	"access-control-allow-methods": "GET, HEAD, POST, PUT, PATCH, DELETE",
	...(askedHeaders ? { "access-control-allow-headers": askedHeaders } : {}),
	"access-control-max-age": "600",
});

const DAY_S = 86_400;
const PATH = /^\/plugin-ui\/([^/]+)(\/.*)?$/;
const SEGMENT = /^([a-z][a-z0-9-]*)-([0-9a-z]{1,11})-([0-9a-f]{32})$/;

const tag = (key: string, id: string, epoch: number, exp: string): string =>
	createHmac("sha256", key)
		.update(`pf-plugin-ui:v1:${id}:${epoch}:${exp}`)
		.digest("hex")
		.slice(0, 32);

/**
 * The first path segment of plugin `id`'s page. It lives a session's length past `nowMs`, rounded
 * up to a whole day so asset URLs stay cacheable, and the next logout's epoch ends it.
 */
export function pluginPageSegment(
	id: string,
	key: string,
	epoch: number,
	maxAgeS: number,
	nowMs: number,
): string {
	const exp = (Math.ceil((nowMs / 1000 + maxAgeS) / DAY_S) * DAY_S).toString(
		36,
	);
	return `${id}-${exp}-${tag(key, id, epoch, exp)}`;
}

/** A verified plugin page path: the plugin, the public prefix, and the path the plugin sees. */
export interface PluginPage {
	id: string;
	prefix: string;
	rest: string;
}

/** A page path's parts, its MAC not yet checked; `null` for a path of another shape or expired. */
export function pageParts(
	pathname: string,
	nowMs: number,
): (PluginPage & { exp: string; mac: string }) | null {
	const m = pathname.match(PATH);
	const seg = m?.[1]?.match(SEGMENT);
	const [, id, exp, mac] = seg ?? [];
	if (!m?.[1] || !id || !exp || !mac) return null;
	if (Number.parseInt(exp, 36) * 1000 <= nowMs) return null;
	return { id, exp, mac, prefix: `/plugin-ui/${m[1]}`, rest: m[2] ?? "/" };
}

/** The page `pathname` names, or `null` unless its segment holds for that plugin at `nowMs`. */
export function parsePluginPage(
	pathname: string,
	key: string,
	epoch: number,
	nowMs: number,
): PluginPage | null {
	const page = pageParts(pathname, nowMs);
	if (!page || !safeEqualHex(page.mac, tag(key, page.id, epoch, page.exp)))
		return null;
	return { id: page.id, prefix: page.prefix, rest: page.rest };
}

/** `/plugin-ui/<segment>/` for plugin `id`, under this console's session key and epoch. */
export function mintPluginPagePath(id: string, nowMs = Date.now()): string {
	const cfg = sessionConfig();
	const maxAge = cfg.maxAge ?? 0;
	return `/plugin-ui/${pluginPageSegment(id, cfg.password, sessionEpoch(), maxAge, nowMs)}/`;
}

/** {@link parsePluginPage} under this console's session key and epoch. */
export function pluginPage(
	pathname: string,
	nowMs = Date.now(),
): PluginPage | null {
	return parsePluginPage(
		pathname,
		sessionConfig().password,
		sessionEpoch(),
		nowMs,
	);
}
