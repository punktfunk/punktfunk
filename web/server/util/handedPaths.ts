// The folders an operator types into a plugin's form (`format: "pf:path"`, or `"pf:path:write"`),
// and the grant that follows a save. Only paths new in that save are granted: the form's old value
// came from the plugin, and a path the plugin filled in itself is not the operator's word. A save
// that drops a path lets the form go of it; the host keeps what another form or the operator holds.
import {
	type GrantRefusal,
	type HandedPath,
	keepAfterDrop,
	newlyHanded,
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

/** PUT a form's value to the plugin's `path`, then grant the paths this save handed over and let
 * `form` go of the ones it dropped. `ask` runs first when the save hands over a path, and a throw
 * from it saves nothing. The value before the save is read first; if that read fails, nothing is
 * granted or let go. A failed release leaves the grant in place. */
export async function putAndGrant(
	id: string,
	path: string,
	body: Uint8Array | undefined,
	form: string,
	ask: () => Promise<void>,
): Promise<{ res: Response | null; access?: GrantOutcome }> {
	const before = await callPlugin(id, path, { method: "GET" });
	const prior = before?.ok
		? ((await before.json().catch(() => null)) as {
				schema?: unknown;
				value?: unknown;
			} | null)
		: null;
	let value: unknown;
	try {
		value = body && JSON.parse(new TextDecoder().decode(body));
	} catch {
		value = undefined;
	}
	const handed =
		prior && value !== undefined
			? newlyHanded(prior.schema, prior.value, value)
			: [];
	if (handed.length) await ask();
	const res = await callPlugin(id, path, {
		method: "PUT",
		headers: { "content-type": "application/json" },
		body,
	});
	if (!res?.ok || !prior || value === undefined) return { res };
	const access = handed.length
		? await grantHandedPaths(id, handed, form)
		: undefined;
	const keep = keepAfterDrop(prior.schema, prior.value, value);
	if (keep) await accessPost(id, "release", { form, keep }).catch(() => null);
	return { res, ...(access ? { access } : {}) };
}
