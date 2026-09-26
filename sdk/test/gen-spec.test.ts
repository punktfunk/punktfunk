import { expect, test } from "bun:test";
import { normalize } from "../scripts/gen-spec";

test("a nullable number becomes anyOf with a null branch", () => {
	const field = { type: ["integer", "null"], format: "int64", minimum: 0, description: "d" };
	expect(normalize(field)).toEqual({
		description: "d",
		anyOf: [{ type: "integer", format: "int64", minimum: 0 }, { type: "null" }],
	});
	expect(normalize({ type: ["string", "null"] })).toEqual({ type: ["string", "null"] });
});

test("the committed client decodes every optional body field", async () => {
	const client = await Bun.file(new URL("../src/gen/punktfunk.ts", import.meta.url)).text();
	const bodies = client.split("\n").filter((l) => /^export const \w+ = /.test(l) && !/^export const \w+Params = /.test(l));
	expect(bodies.filter((l) => l.includes("Schema.Never"))).toEqual([]);
});
