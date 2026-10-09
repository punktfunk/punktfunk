// A plugin form's JSON Schema node, as the console draws it and the BFF grants from it. Both read
// a handed folder (`format: "pf:path"` or `"pf:path:write"`) through `flatten` and `handed` here.

export interface JsonSchemaNode {
	type?: string;
	title?: string;
	description?: string;
	default?: unknown;
	enum?: string[];
	/** `pf:path` / `pf:path:write`: a folder the console grants the plugin on save. */
	format?: string;
	properties?: Record<string, JsonSchemaNode>;
	items?: JsonSchemaNode;
	allOf?: JsonSchemaNode[];
}

/** Plugins built on an effect 4 beta nest a checked field's annotations (`Schema.Int`, `.check(...)`) under `allOf`. */
export const flatten = (node: JsonSchemaNode): JsonSchemaNode =>
	(node.allOf ?? []).reduce<JsonSchemaNode>(
		(acc, branch) => Object.assign(acc, branch),
		{ ...node },
	);

/** The access a handed-folder field asks for, or `null` for any other field. */
export const handed = (n: JsonSchemaNode): "read" | "write" | null =>
	n.format === "pf:path:write"
		? "write"
		: n.format === "pf:path"
			? "read"
			: null;
