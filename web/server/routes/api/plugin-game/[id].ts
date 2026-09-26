// GET/PUT /api/plugin-game/<id>?entry=<library id> — a plugin's section on one library entry's
// page (`/__game`), read server-side over loopback like `plugin-config`, so no plugin markup or
// secret reaches the console origin. A save grants the folders the operator typed into it and lets
// go of the ones taken out.
import {
	defineEventHandler,
	getQuery,
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

/** A library id: `<store>:<external id>`, the external part the provider's own. */
const validEntry = (v: unknown): v is string =>
	typeof v === "string" &&
	v.length <= 256 &&
	v.includes(":") &&
	![...v].some((c) => c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f);

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	const { entry } = getQuery(event);
	if (!id || !PLUGIN_ID_RE.test(id) || !validEntry(entry)) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin or library id" };
	}
	const method = event.method;
	if (method !== "GET" && method !== "PUT") {
		setResponseStatus(event, 405);
		return { error: "method not allowed" };
	}
	const path = `/__game?entry=${encodeURIComponent(entry)}`;
	const body =
		method === "PUT"
			? ((await readRawBody(event, false)) as Uint8Array | undefined)
			: undefined;
	const { res, access } =
		method === "PUT"
			? await putAndGrant(id, path, body, `game:${entry}`)
			: { res: await callPlugin(id, path, "GET"), access: undefined };
	if (!res) {
		setResponseStatus(event, 502);
		return { error: `plugin ${id} is not reachable` };
	}
	// The plugin has nothing for this entry: the page shows no tab. Marked, so the host API's own
	// 404 (under `bun run dev`, where these routes do not run) never reads as this.
	if (res.status === 404) {
		setResponseStatus(event, 404);
		return { error: "plugin has no section for this entry", noSection: true };
	}
	setResponseStatus(event, res.status);
	const json = await pluginJson(res, id);
	return access ? { ...(json as object), access } : json;
});
