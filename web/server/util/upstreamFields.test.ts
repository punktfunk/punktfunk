// The password-gated BFF routes rebuild their upstream body from a field whitelist, so anything
// they forget to list is silently dropped on the way to the host. That is invisible from the
// console — the request succeeds, the setting just never lands.
//
// So: every property the OpenAPI request schema defines must be named in the route that forwards
// it. Textual on purpose — the thing worth pinning is the whitelist, not the handler.
import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const repo = join(import.meta.dir, "..", "..", "..");
const spec = JSON.parse(
	readFileSync(join(repo, "api", "openapi.json"), "utf8"),
) as {
	components: {
		schemas: Record<string, { properties?: Record<string, unknown> }>;
	};
};

const routes: { schema: string; file: string }[] = [
	{
		schema: "ArmNativePairing",
		file: "web/server/routes/api/v1/native/pair/arm.post.ts",
	},
	{
		schema: "ApprovePending",
		file: "web/server/routes/api/v1/native/pending/[id]/approve.post.ts",
	},
	{
		schema: "UpdateNativeAccess",
		file: "web/server/routes/api/v1/native/clients/[fingerprint].patch.ts",
	},
];

describe("BFF whitelists forward every field the API defines", () => {
	for (const { schema, file } of routes) {
		it(`${schema} -> ${file}`, () => {
			const props = Object.keys(
				spec.components.schemas[schema]?.properties ?? {},
			);
			expect(props.length).toBeGreaterThan(0);
			let source: string;
			try {
				source = readFileSync(join(repo, file), "utf8");
			} catch {
				// A route that does not exist forwards verbatim through the `/api/**` catch-all,
				// which cannot drop a field. Nothing to pin.
				return;
			}
			const missing = props.filter((p) => !source.includes(p));
			expect(missing).toEqual([]);
		});
	}
});
