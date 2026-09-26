// The folders an operator types into a plugin's form (`format: "pf:path"`, or `"pf:path:write"`),
// and the grant that follows a save. Only paths new in that save are granted: the form's old value
// came from the plugin, and a path the plugin filled in itself is not the operator's word. A save
// that drops a path lets the form go of it; the host keeps what another form or the operator holds.
import { loopbackTls, mgmtToken, mgmtUrl } from "./auth";
import { callPlugin } from "./pluginProxy";

interface Node {
	type?: string;
	format?: string;
	properties?: Record<string, Node>;
	items?: Node;
	allOf?: Node[];
}

export interface HandedPath {
	path: string;
	write: boolean;
}

export interface GrantOutcome {
	granted: string[];
	refused: { path: string; error: string }[];
}

// A checked schema nests its annotations under `allOf`.
const flatten = (n: Node): Node =>
	(n.allOf ?? []).reduce<Node>((acc, b) => Object.assign(acc, b), { ...n });

/** Every handed path in `value`, found by walking `schema` (the kit's `{schema: {…}}` document). */
export function handedPaths(schema: unknown, value: unknown): HandedPath[] {
	const out = new Map<string, HandedPath>();
	const walk = (raw: Node | undefined, v: unknown): void => {
		if (!raw) return;
		const n = flatten(raw);
		if (n.format === "pf:path" || n.format === "pf:path:write") {
			const path = typeof v === "string" ? v.trim() : "";
			const write = n.format === "pf:path:write";
			if (path) out.set(path, { path, write: write || !!out.get(path)?.write });
		} else if (n.type === "array" && Array.isArray(v)) {
			for (const item of v) walk(n.items, item);
		} else if (n.properties && v && typeof v === "object") {
			for (const [k, child] of Object.entries(n.properties))
				walk(child, (v as Record<string, unknown>)[k]);
		}
	};
	walk((schema as { schema?: Node } | null)?.schema, value);
	return [...out.values()];
}

/** The handed paths `after` holds that `before` did not. */
export function newlyHanded(
	schema: unknown,
	before: unknown,
	after: unknown,
): HandedPath[] {
	const had = new Set(handedPaths(schema, before).map((p) => p.path));
	return handedPaths(schema, after).filter((p) => !had.has(p.path));
}

/** What `after` still hands over when it dropped a path `before` handed, else `null`. */
export function keepAfterDrop(
	schema: unknown,
	before: unknown,
	after: unknown,
): string[] | null {
	const kept = handedPaths(schema, after).map((p) => p.path);
	const dropped = handedPaths(schema, before).some(
		(p) => !kept.includes(p.path),
	);
	return dropped ? kept : null;
}

const accessPost = (id: string, route: string, body: unknown) => {
	const base = mgmtUrl();
	return fetch(`${base}/api/v1/plugin-access/${id}/${route}`, {
		...(loopbackTls(base) as RequestInit | undefined),
		method: "POST",
		headers: {
			authorization: `Bearer ${mgmtToken()}`,
			"content-type": "application/json",
		},
		body: JSON.stringify(body),
	});
};

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
				} | null;
				out.refused.push({
					path: p.path,
					error: body?.error ?? `the host answered ${res.status}`,
				});
			}
		} catch {
			out.refused.push({ path: p.path, error: "the host is not reachable" });
		}
	}
	return out;
}

/** PUT a form's value to the plugin's `path`, then grant the paths this save handed over and let
 * `form` go of the ones it dropped. The value before the save is read first; if that read fails,
 * nothing is granted or let go. A failed release leaves the grant in place. */
export async function putAndGrant(
	id: string,
	path: string,
	body: Uint8Array | undefined,
	form: string,
): Promise<{ res: Response | null; access?: GrantOutcome }> {
	const before = await callPlugin(id, path, "GET");
	const prior = before?.ok
		? ((await before.json().catch(() => null)) as {
				schema?: unknown;
				value?: unknown;
			} | null)
		: null;
	const res = await callPlugin(id, path, "PUT", body);
	if (!res?.ok || !prior || !body) return { res };
	let value: unknown;
	try {
		value = JSON.parse(new TextDecoder().decode(body));
	} catch {
		return { res };
	}
	const handed = newlyHanded(prior.schema, prior.value, value);
	const access = handed.length
		? await grantHandedPaths(id, handed, form)
		: undefined;
	const keep = keepAfterDrop(prior.schema, prior.value, value);
	if (keep) await accessPost(id, "release", { form, keep }).catch(() => null);
	return { res, ...(access ? { access } : {}) };
}
