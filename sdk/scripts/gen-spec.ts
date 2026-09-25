// The OpenAPI spec as `openapigen` needs to read it.
//
// utoipa writes an optional number as `type: ["integer", "null"]`. The generator turns that into
// `Never` when a `format` is present, and drops the `null` when only `minimum` is, so the SDK
// rejects any value the host sends. The same field written as `anyOf` with a null branch
// generates `number | null`. Nullable strings, booleans and arrays already generate correctly.

const NUMERIC = new Set(["integer", "number"]);
/** Keywords about the field rather than its value; they stay on the outer schema. */
const OUTER = new Set(["description", "title", "example", "examples", "default", "deprecated", "readOnly", "writeOnly"]);

export function normalize(node: unknown): unknown {
	if (Array.isArray(node)) return node.map(normalize);
	if (node === null || typeof node !== "object") return node;
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(node)) out[k] = normalize(v);
	const type = out.type;
	if (!Array.isArray(type) || type.length !== 2 || !type.includes("null")) return out;
	const value = type.find((t) => t !== "null");
	if (!NUMERIC.has(value)) return out;
	const inner: Record<string, unknown> = { type: value };
	const outer: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(out)) {
		if (k !== "type") (OUTER.has(k) ? outer : inner)[k] = v;
	}
	return { ...outer, anyOf: [inner, { type: "null" }] };
}

if (import.meta.main) {
	const [src, dst] = process.argv.slice(2);
	if (!src || !dst) throw new Error("usage: bun scripts/gen-spec.ts <openapi.json> <out.json>");
	// Components only: a parameter is sent as a string whatever its schema says.
	const spec = await Bun.file(src).json();
	spec.components = normalize(spec.components);
	await Bun.write(dst, JSON.stringify(spec));
}
