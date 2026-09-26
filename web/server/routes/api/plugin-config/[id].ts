// GET/PUT /api/plugin-config/<id> — a plugin's `__config`, readable from the CONSOLE origin.
//
// The Library section's "Game sources" settings drawer renders a form from a library plugin's
// `__config` (the kit's generic settings surface, so a scanner needs no SPA of its own). It fetched
// `/plugin-ui/<id>/__config` same-origin — and that stopped working the moment plugin UIs moved to
// their own origin (2026-08-05 review H-3): `middleware/auth.ts` answers 404 for `/plugin-ui/**` on
// the console origin, unconditionally and by design. The drawer is the only NON-IFRAME consumer of
// that path, so nothing else noticed, and settings silently failed to open for every library plugin.
//
// The fix is deliberately not "point the drawer at the plugin origin". That needs CORS plus
// cross-site cookies, and it would put a plugin-controlled response inside a credentialed
// cross-origin fetch — reopening the hole the split exists to close. What the drawer needs is DATA,
// not an embedded UI: this reads the JSON server-side over loopback and returns it same-origin, so
// no plugin HTML or JS is ever served from the console origin.
//
// Auth: `/api/**` is always session-gated (`isPublicPath`), so reaching here means a logged-in
// operator, and it answers 401 as JSON rather than redirecting — which is what a `fetch` needs. The
// plugin's per-boot secret stays server-side, exactly as in the `/plugin-ui` proxy.
import {
	defineEventHandler,
	getRouterParam,
	readRawBody,
	setResponseStatus,
} from "h3";
import { putAndGrant } from "../../../util/handedPaths";
import {
	callPlugin,
	PLUGIN_ID_RE,
	pluginJson,
} from "../../../util/pluginProxy";

/** `GET` reads schema + current value; `PUT` validates and saves. Nothing else is forwarded. */
const ALLOWED = new Set(["GET", "PUT"]);

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	// 400, not 404: the console reads a 404 from here as "this plugin serves no `__config`, its
	// settings are its own page". `PLUGIN_ID_RE` is deliberately narrower than the host's provider
	// rule (which allows `.` and `_`), so a listed source can land here with an id we won't dial —
	// and saying "no settings surface" about it would be a lie.
	if (!id || !PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin id" };
	}
	const method = event.method;
	if (!ALLOWED.has(method)) {
		setResponseStatus(event, 405);
		return { error: "method not allowed" };
	}
	// Read once: `readRawBody` drains the stream, and an empty PUT would save `{}`.
	const body =
		method === "PUT"
			? ((await readRawBody(event, false)) as Uint8Array | undefined)
			: undefined;
	const { res, access } =
		method === "PUT"
			? await putAndGrant(id, "/__config", body, "config")
			: { res: await callPlugin(id, "/__config", "GET"), access: undefined };
	if (!res) {
		setResponseStatus(event, 502);
		return { error: `plugin ${id} is not reachable` };
	}

	// A 404 here is the plugin declining to have a `__config` at all (`config` is optional on the
	// kit's `serveUi`), which the console renders as "settings live on this plugin's own page".
	// Marked in the body rather than left as a bare status: under `bun run dev` these routes do not
	// run and `/api` proxies to the management API, whose 404 for an unknown path would otherwise
	// read as that same claim about every source.
	if (res.status === 404) {
		setResponseStatus(event, 404);
		return { error: "plugin serves no config surface", noConfig: true };
	}
	setResponseStatus(event, res.status);
	const json = await pluginJson(res, id);
	return access ? { ...(json as object), access } : json;
});
