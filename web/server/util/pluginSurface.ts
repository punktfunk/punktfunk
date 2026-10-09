// One request to a plugin's loopback surface on the console's behalf, answered same-origin as
// JSON, so no plugin markup or secret reaches the console origin. The routes under
// routes/api/plugin-* validate their own ids and hand the rest to `pluginSurface`.
import { type H3Event, readRawBody, setResponseStatus } from "h3";
import { putAndGrant } from "./handedPaths";
import { callPlugin, pluginJson } from "./pluginProxy";

type Method = "GET" | "PUT" | "POST";

/**
 * Forward the request's method and body to `path` on plugin `id` and relay the answer.
 *
 * `methods` is all that is forwarded (405 otherwise). A PUT with `grantForm` saves through
 * `putAndGrant` and merges its `access` outcome into the answer. `notFound` is the body a plugin's
 * 404 becomes: marked, so the host API's own 404 under `bun run dev` never reads as it. A plugin's
 * 401 refuses our secret, not the session, so it answers 502 like an unreachable plugin.
 */
export async function pluginSurface(
	event: H3Event,
	id: string,
	path: string,
	opts: {
		methods: readonly Method[];
		grantForm?: string;
		notFound?: Record<string, unknown>;
	},
): Promise<unknown> {
	const method = event.method as Method;
	if (!opts.methods.includes(method)) {
		setResponseStatus(event, 405);
		return { error: "method not allowed" };
	}
	// Read once: `readRawBody` drains the stream, and an empty PUT would save `{}`.
	const body =
		method === "GET"
			? undefined
			: ((await readRawBody(event, false)) as Uint8Array | undefined);
	const { res, access } =
		method === "PUT" && opts.grantForm
			? await putAndGrant(id, path, body, opts.grantForm)
			: {
					res: await callPlugin(id, path, {
						method,
						headers:
							method === "GET" ? {} : { "content-type": "application/json" },
						body,
					}),
					access: undefined,
				};
	if (!res || res.status === 401) {
		setResponseStatus(event, 502);
		return { error: `plugin ${id} is not reachable` };
	}
	if (res.status === 404 && opts.notFound) {
		setResponseStatus(event, 404);
		return opts.notFound;
	}
	setResponseStatus(event, res.status);
	const json = await pluginJson(res, id);
	return access ? { ...(json as object), access } : json;
}
