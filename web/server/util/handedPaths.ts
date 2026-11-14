// The grant that follows a plugin form's save. The browser names the folders the operator
// confirmed, read from the form it loaded (`GRANT_HEADER`); this grants those the saved value
// holds, at the access confirmed, and reads nothing from the plugin to decide. A save that drops a
// path lets the form go of it; the host keeps what another form or the operator holds.
import { createError } from "h3";
import type {
	GrantRefusal,
	HandedPath,
	Handover,
} from "../../src/lib/handed-paths";
import { mgmtFetch } from "./forward";
import { callPlugin } from "./pluginProxy";

export interface GrantOutcome {
	granted: string[];
	refused: GrantRefusal[];
}

const accessPost = (id: string, route: string, body: unknown) =>
	mgmtFetch(`/api/v1/plugin-access/${id}/${route}`, {
		method: "POST",
		headers: { "content-type": "application/json" },
		body: JSON.stringify(body),
	});

/** Grant each path to the plugin on the operator's lane, on behalf of `form`. The host refuses
 * what it would refuse a request (`~`, `~/.ssh`, the config dir, …); those come back in
 * `refused`. */
export async function grantHandedPaths(
	id: string,
	paths: HandedPath[],
	form: string,
): Promise<GrantOutcome> {
	const out: GrantOutcome = { granted: [], refused: [] };
	for (const p of paths) {
		try {
			const res = await accessPost(id, "decide", {
				path: p.path,
				decision: "allow",
				write: p.write,
				form,
			});
			if (res.ok) {
				out.granted.push(p.path);
			} else {
				const body = (await res.json().catch(() => null)) as {
					error?: string;
					code?: string;
					params?: Record<string, string> | null;
				} | null;
				out.refused.push({
					path: p.path,
					error: body?.error ?? `the host answered ${res.status}`,
					code: body?.code,
					params: body?.params,
				});
			}
		} catch {
			out.refused.push({ path: p.path, error: "the host is not reachable" });
		}
	}
	return out;
}

/** The {@link Handover} in `raw`, or `null` without one. One that doesn't parse throws 400. */
function readHandover(raw: string | undefined): Handover | null {
	if (raw === undefined) return null;
	let h: Partial<Handover> | null = null;
	try {
		h = JSON.parse(decodeURIComponent(raw));
	} catch {
		h = null;
	}
	const grant = h?.grant;
	const keep = h?.keep;
	if (
		!Array.isArray(grant) ||
		!grant.every(
			(p: Partial<HandedPath> | null) =>
				typeof p?.path === "string" && typeof p.write === "boolean",
		) ||
		!(
			keep === null ||
			(Array.isArray(keep) && keep.every((p) => typeof p === "string"))
		)
	)
		throw createError({
			statusCode: 400,
			statusMessage: "unreadable grant header",
		});
	return { grant, keep };
}

/** Every string `value` holds, trimmed as a handed path is. */
function strings(value: unknown, out = new Set<string>()): Set<string> {
	if (typeof value === "string") out.add(value.trim());
	else if (value && typeof value === "object")
		for (const v of Object.values(value)) strings(v, out);
	return out;
}

/** PUT a form's value to the plugin's `path`, then grant the paths `handover` (the raw
 * `GRANT_HEADER`) confirms that the value holds, and let `form` go of every path it no longer
 * keeps. `ask` runs first when there is a grant, and a throw from it saves nothing. A failed
 * release leaves the grant in place. */
export async function putAndGrant(
	id: string,
	path: string,
	body: Uint8Array | undefined,
	form: string,
	handover: string | undefined,
	ask: () => Promise<void>,
): Promise<{ res: Response | null; access?: GrantOutcome }> {
	const confirmed = readHandover(handover);
	let value: unknown;
	try {
		value = body && JSON.parse(new TextDecoder().decode(body));
	} catch {
		value = undefined;
	}
	const held = strings(value);
	const grant = confirmed?.grant.filter((p) => held.has(p.path)) ?? [];
	if (grant.length) await ask();
	const res = await callPlugin(id, path, {
		method: "PUT",
		headers: { "content-type": "application/json" },
		body,
	});
	if (!res?.ok || value === undefined) return { res };
	const access = grant.length
		? await grantHandedPaths(id, grant, form)
		: undefined;
	const keep = confirmed?.keep?.filter((p) => held.has(p));
	if (keep) await accessPost(id, "release", { form, keep }).catch(() => null);
	return { res, ...(access ? { access } : {}) };
}
