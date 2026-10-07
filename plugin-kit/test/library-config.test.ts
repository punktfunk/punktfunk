// The `__config` contract — the wire shape the console's generic settings drawer codes against,
// plus the JSON-Schema derivation's committed fixture (design M0/S2).
//
// The derivation fixture is not decoration: it is the record of WHICH schema shapes the generic
// form can render. If an effect upgrade changes any of it, this test fails and the console's form
// needs re-checking before the change ships — far cheaper than discovering it on a user's box.
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Effect, Layer, Schema } from "effect";
import { makeConfigService } from "../src/config.js";
import { pluginInfoLayer } from "../src/host-client.js";
import { deriveConfigJsonSchema, makeConfigHandler } from "../src/ui-server.js";

/** A representative scanner config: booleans, a string, a string array, a nested object, an enum. */
const ScannerConfig = Schema.Struct({
	enabled: Schema.Boolean.annotate({
		title: "Enable scanning",
		description: "Whether this source contributes titles.",
		default: true,
	}).pipe(
		Schema.withDecodingDefaultKey(Effect.succeed(true), {
			encodingStrategy: "omit",
		}),
	),
	root: Schema.optionalKey(
		Schema.String.annotate({
			title: "Launcher root",
			description: "Absolute path.",
		}),
	),
	extraRoots: Schema.Array(Schema.String)
		.annotate({ title: "Extra roots" })
		.pipe(
			Schema.withDecodingDefaultKey(
				Effect.succeed([] as ReadonlyArray<string>),
				{ encodingStrategy: "omit" },
			),
		),
	launchers: Schema.Struct({
		bigpicture: Schema.Boolean.annotate({
			title: "Big Picture",
			default: true,
		}),
		desktop: Schema.Boolean.annotate({ title: "Desktop", default: false }),
	}).pipe(
		Schema.withDecodingDefaultKey(
			Effect.succeed({ bigpicture: true, desktop: false }),
			{ encodingStrategy: "omit" },
		),
	),
	pollMinutes: Schema.Int.annotate({
		title: "Poll interval (minutes)",
		default: 15,
	}).pipe(
		Schema.withDecodingDefaultKey(Effect.succeed(15), {
			encodingStrategy: "omit",
		}),
	),
	artSource: Schema.Literals(["local", "cdn", "both"])
		.annotate({ title: "Art source", default: "both" })
		.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed("both" as const), {
				encodingStrategy: "omit",
			}),
		),
});

const props = (): Record<string, Record<string, unknown>> => {
	const doc = deriveConfigJsonSchema(ScannerConfig) as {
		schema: { properties: Record<string, Record<string, unknown>> };
	};
	return doc.schema.properties;
};

describe("S2 — JSON Schema derivation for __config", () => {
	test("derives a renderable form for every shape a scanner config uses", () => {
		const p = props();
		expect(p.enabled).toMatchObject({ type: "boolean" });
		expect(p.root).toMatchObject({ type: "string" });
		expect(p.extraRoots).toMatchObject({
			type: "array",
			items: { type: "string" },
		});
		// A nested object stays nested — the form renders a fieldset, not a JSON blob.
		expect(p.launchers).toMatchObject({
			type: "object",
			properties: {
				bigpicture: { type: "boolean" },
				desktop: { type: "boolean" },
			},
		});
		// A literal union derives a clean enum — prefer it over a union of strings.
		expect(p.artSource).toMatchObject({
			type: "string",
			enum: ["local", "cdn", "both"],
		});
	});

	test("annotations pass through — they are the ONLY source of labels and defaults", () => {
		const p = props();
		expect(p.enabled.title).toBe("Enable scanning");
		expect(p.enabled.description).toBe(
			"Whether this source contributes titles.",
		);
		// The derivation does NOT infer `default` from withDecodingDefaultKey, so an un-annotated
		// field shows the form no placeholder at all. Annotate every field.
		expect(p.enabled.default).toBe(true);
		expect(p.artSource.default).toBe("both");
		// A CHECKED schema (Int is String-plus-a-check) keeps its annotations on the property itself.
		expect(p.pollMinutes).toMatchObject({
			default: 15,
			title: "Poll interval (minutes)",
		});
	});

	test("a decoding default is an Effect, not a thunk — and it actually applies", () => {
		// The trap this pins: `withDecodingDefaultKey` takes an `Effect`, and passing a bare thunk
		// (`() => true`) type-checks against the derivation path but blows up at DECODE time with
		// "Not a valid effect". Deriving a schema is therefore NOT evidence that it works.
		expect(Schema.decodeUnknownSync(ScannerConfig)({})).toMatchObject({
			enabled: true,
			pollMinutes: 15,
			artSource: "both",
			launchers: { bigpicture: true, desktop: false },
		});
	});

	test("Schema.Int derives a plain integer — Schema.Number does NOT", () => {
		expect(props().pollMinutes).toMatchObject({ type: "integer" });
		// The trap, pinned: Schema.Number's ENCODED form admits "NaN"/"Infinity"/"-Infinity", so it
		// derives a four-way anyOf that no number input can render. Use Finite or Int.
		const bad = deriveConfigJsonSchema(Schema.Struct({ n: Schema.Number })) as {
			schema: { properties: { n: { anyOf?: unknown[] } } };
		};
		expect(Array.isArray(bad.schema.properties.n.anyOf)).toBe(true);
		const ok = deriveConfigJsonSchema(Schema.Struct({ n: Schema.Finite })) as {
			schema: { properties: { n: { type?: string } } };
		};
		expect(ok.schema.properties.n.type).toBe("number");
	});

	test("defaulted fields drop out of `required` — the raw file stays default-free", () => {
		const doc = deriveConfigJsonSchema(ScannerConfig) as {
			schema: { required?: string[] };
		};
		// Every field here either has a decoding default or is optionalKey, so nothing is required.
		expect(doc.schema.required ?? []).toEqual([]);
	});
});

describe("__config wire contract", () => {
	const withService = async <A>(
		use: (
			handler: (req: Request) => Promise<Response>,
			file: string,
		) => Promise<A>,
	): Promise<A> => {
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-kit-cfg-"));
		const prev = process.env.PUNKTFUNK_CONFIG_DIR;
		process.env.PUNKTFUNK_CONFIG_DIR = dir;
		try {
			const service = await Effect.runPromise(
				makeConfigService({ schema: ScannerConfig }).pipe(
					Effect.provide(
						Layer.mergeAll(
							pluginInfoLayer({ name: "steam", version: "0.1.0" }),
						),
					),
				),
			);
			return await use(
				makeConfigHandler({ schema: ScannerConfig, service }),
				service.path,
			);
		} finally {
			if (prev === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
			else process.env.PUNKTFUNK_CONFIG_DIR = prev;
			fs.rmSync(dir, { recursive: true, force: true });
		}
	};

	test("GET answers {schema, value} with an absent file reading as empty", async () => {
		await withService(async (handler) => {
			const res = await handler(new Request("http://x/__config"));
			expect(res.status).toBe(200);
			const body = (await res.json()) as { schema: unknown; value: unknown };
			// Both keys are ALWAYS present and never `undefined` — the console decodes this shape,
			// and an omitted-vs-null field is the wire trap that bit the rom-manager 0.3.1 release.
			expect(body).toHaveProperty("schema");
			expect(body).toHaveProperty("value");
			expect(body.schema).not.toBeNull();
			// A missing config file is an EMPTY config, not an error.
			expect(body.value).toEqual({});
		});
	});

	test("PUT validates by decode, persists RAW, and never bakes in defaults", async () => {
		await withService(async (handler, file) => {
			const res = await handler(
				new Request("http://x/__config", {
					method: "PUT",
					body: JSON.stringify({ enabled: false }),
				}),
			);
			expect(res.status).toBe(200);
			// The file holds exactly what was authored — the five defaulted fields are NOT written,
			// which is what keeps a future change to a default from being silently pinned.
			expect(JSON.parse(fs.readFileSync(file, "utf8"))).toEqual({
				enabled: false,
			});
			const get = (await (
				await handler(new Request("http://x/__config"))
			).json()) as { value: unknown };
			expect(get.value).toEqual({ enabled: false });
		});
	});

	test("PUT rejects a body the schema refuses, with the issue, and writes nothing", async () => {
		await withService(async (handler, file) => {
			const res = await handler(
				new Request("http://x/__config", {
					method: "PUT",
					body: JSON.stringify({ enabled: "yes please" }),
				}),
			);
			expect(res.status).toBe(400);
			const body = (await res.json()) as { error: string; issue: string };
			expect(body.error).toBe("config rejected");
			expect(body.issue.length).toBeGreaterThan(0);
			expect(fs.existsSync(file)).toBe(false);
		});
	});

	test("PUT rejects a non-JSON body", async () => {
		await withService(async (handler) => {
			const res = await handler(
				new Request("http://x/__config", { method: "PUT", body: "not json" }),
			);
			expect(res.status).toBe(400);
			expect(((await res.json()) as { error: string }).error).toBe(
				"body must be JSON",
			);
		});
	});

	test("other methods are refused", async () => {
		await withService(async (handler) => {
			const res = await handler(
				new Request("http://x/__config", { method: "DELETE" }),
			);
			expect(res.status).toBe(405);
		});
	});
});
