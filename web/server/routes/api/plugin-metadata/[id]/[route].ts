// /api/plugin-metadata/<id>/<route> — an Art & Metadata source's `/__metadata/<route>` (status,
// match, search, images), read server-side over loopback like `plugin-game`, so no plugin markup or
// secret reaches the console origin. A pick is stored by the host, not through here.
import {
	defineEventHandler,
	getQuery,
	getRouterParam,
	readRawBody,
	setResponseStatus,
} from "h3";
import {
	callPlugin,
	PLUGIN_ID_RE,
	pluginJson,
} from "../../../../util/pluginProxy";

/** The routes the kit serves, and their methods. Nothing else is forwarded. */
const ROUTES: Record<string, ReadonlyArray<"GET" | "PUT" | "POST">> = {
	status: ["GET"],
	match: ["GET", "PUT"],
	search: ["POST"],
	images: ["GET"],
};

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	const route = getRouterParam(event, "route");
	if (
		!id ||
		!PLUGIN_ID_RE.test(id) ||
		!route ||
		!Object.hasOwn(ROUTES, route)
	) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin id or route" };
	}
	const method = event.method as "GET" | "PUT" | "POST";
	if (!ROUTES[route]?.includes(method)) {
		setResponseStatus(event, 405);
		return { error: "method not allowed" };
	}
	const query = new URLSearchParams();
	const { entry, kind } = getQuery(event);
	if (typeof entry === "string") query.set("entry", entry);
	if (typeof kind === "string") query.set("kind", kind);
	const qs = query.toString();
	const body =
		method === "GET"
			? undefined
			: ((await readRawBody(event, false)) as Uint8Array | undefined);
	const res = await callPlugin(
		id,
		`/__metadata/${route}${qs ? `?${qs}` : ""}`,
		method,
		body,
	);
	if (!res) {
		setResponseStatus(event, 502);
		return { error: `plugin ${id} is not reachable` };
	}
	setResponseStatus(event, res.status);
	return pluginJson(res, id);
});
