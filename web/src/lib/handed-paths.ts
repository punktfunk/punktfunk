// The folders an operator types into a plugin's form (`format: "pf:path"`, or `"pf:path:write"`).
// The console's server grants the ones a save adds (server/util/handedPaths.ts) and asks for the
// console password first; the form pages read the same rule, so they ask exactly then.
import { flatten, handed, type JsonSchemaNode } from "./schema-node";

export interface HandedPath {
	path: string;
	write: boolean;
}

/** A grant the host refused: its English, and the `code` and `params` that word it. */
export interface GrantRefusal {
	path: string;
	error: string;
	code?: string;
	params?: Record<string, string> | null;
}

/** Every handed path in `value`, found by walking `schema` (the kit's `{schema: {…}}` document). */
export function handedPaths(schema: unknown, value: unknown): HandedPath[] {
	const out = new Map<string, HandedPath>();
	const walk = (raw: JsonSchemaNode | undefined, v: unknown): void => {
		if (!raw) return;
		const n = flatten(raw);
		const access = handed(n);
		if (access) {
			const path = typeof v === "string" ? v.trim() : "";
			const write = access === "write";
			if (path) out.set(path, { path, write: write || !!out.get(path)?.write });
		} else if (n.type === "array" && Array.isArray(v)) {
			for (const item of v) walk(n.items, item);
		} else if (n.properties && v && typeof v === "object") {
			for (const [k, child] of Object.entries(n.properties))
				walk(child, (v as Record<string, unknown>)[k]);
		}
	};
	walk((schema as { schema?: JsonSchemaNode } | null)?.schema, value);
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
