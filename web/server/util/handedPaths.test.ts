import { describe, expect, test } from "bun:test";
import { handedPaths, keepAfterDrop, newlyHanded } from "./handedPaths";

const schema = {
	schema: {
		type: "object",
		properties: {
			enabled: { type: "boolean" },
			paths: {
				type: "array",
				items: { type: "string", format: "pf:path:write" },
			},
			nested: {
				type: "object",
				properties: {
					// A checked schema keeps its format under `allOf`.
					logs: { allOf: [{ type: "string" }, { format: "pf:path" }] },
					title: { type: "string" },
				},
			},
		},
	},
};

describe("handed paths", () => {
	test("found along the schema, arrays and nesting included", () => {
		expect(
			handedPaths(schema, {
				enabled: true,
				paths: ["/home/a/.config/celeste", " ", "/home/a/saves"],
				nested: { logs: "/var/log/x", title: "/not/a/path" },
			}),
		).toEqual([
			{ path: "/home/a/.config/celeste", write: true },
			{ path: "/home/a/saves", write: true },
			{ path: "/var/log/x", write: false },
		]);
	});

	test("only what this save added is granted", () => {
		// The plugin filled in `planted` itself; the operator typed `typed`.
		const before = { paths: ["/home/a/planted"] };
		const after = { paths: ["/home/a/planted", "/home/a/typed"] };
		expect(newlyHanded(schema, before, after)).toEqual([
			{ path: "/home/a/typed", write: true },
		]);
	});

	test("a dropped path lets go of all but what is still handed", () => {
		const before = { paths: ["/home/a/one", "/home/a/two"] };
		expect(keepAfterDrop(schema, before, { paths: ["/home/a/two"] })).toEqual([
			"/home/a/two",
		]);
		const added = { paths: ["/home/a/one", "/home/a/two", "/home/a/three"] };
		expect(keepAfterDrop(schema, before, added)).toBeNull();
	});

	test("no schema, no paths", () => {
		expect(handedPaths(null, { paths: ["/x"] })).toEqual([]);
	});
});
