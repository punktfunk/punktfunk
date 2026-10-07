// Spike 2 (plan Phase 0): prove the browser-side client strategy for the console proxy
// prefix (`/plugin-ui/<id>/...`), headless via AtomRegistry (no React needed).
//
// The console serves the plugin SPA under a path prefix; the HttpApi contract uses
// absolute endpoint paths ("/api/status"). This spike pins down how the derived client
// must be configured so requests keep the prefix:
//   - transformClient + HttpClientRequest.prependUrl(prefix)  → expected to work
//   - baseUrl with a path prefix                              → documented behavior
// Also verifies the nested `withDecodingDefaultKey` config-defaults pattern the kit's
// Config service relies on.
import { describe, expect, test } from "bun:test";
import { Effect, Layer, Schema } from "effect";
import { HttpClient, HttpClientRequest, HttpClientResponse } from "effect/http";
import { HttpApi, HttpApiEndpoint, HttpApiGroup } from "effect/http-api";
import { AtomHttpApi, AtomRegistry } from "effect/reactivity";

const Pong = Schema.Struct({ ok: Schema.Boolean });

const api = HttpApi.make("spike").add(
	HttpApiGroup.make("spike").add(
		HttpApiEndpoint.get("ping", "/api/ping", { success: Pong }),
	),
);

const captureClient = (captured: Array<string>) =>
	HttpClient.make((request) => {
		captured.push(request.url);
		return Effect.succeed(
			HttpClientResponse.fromWeb(request, Response.json({ ok: true })),
		);
	});

const PREFIX = "http://plugin.local/plugin-ui/rom-manager";

describe("spike 2: client prefix through the console proxy", () => {
	test("transformClient + prependUrl keeps the path prefix", async () => {
		const captured: Array<string> = [];
		class Api extends AtomHttpApi.Service<Api>()("SpikeApiPrepend", {
			api,
			httpClient: Layer.succeed(HttpClient.HttpClient)(captureClient(captured)),
			transformClient: HttpClient.mapRequest(
				HttpClientRequest.prependUrl(PREFIX),
			),
		}) {}

		const registry = AtomRegistry.make();
		const result = await Effect.runPromise(
			AtomRegistry.getResult(registry, Api.query("spike", "ping", {})),
		);
		expect(result).toEqual({ ok: true });
		expect(captured).toHaveLength(1);
		expect(captured[0]).toBe(`${PREFIX}/api/ping`);
	});

	test("documents baseUrl behavior with a path-prefix base", async () => {
		const captured: Array<string> = [];
		class Api extends AtomHttpApi.Service<Api>()("SpikeApiBaseUrl", {
			api,
			httpClient: Layer.succeed(HttpClient.HttpClient)(captureClient(captured)),
			baseUrl: PREFIX,
		}) {}

		const registry = AtomRegistry.make();
		await Effect.runPromise(
			AtomRegistry.getResult(registry, Api.query("spike", "ping", {})),
		);
		expect(captured).toHaveLength(1);
		// If this equals `${PREFIX}/api/ping`, baseUrl would also be fine; if the prefix is
		// dropped (URL-resolution semantics for absolute paths), transformClient is the way.
		// Either way the assertion records the actual behavior for the kit docs.
		console.log("baseUrl produced:", captured[0]);
		expect(captured[0]).toContain("/api/ping");
	});
});

describe("spike 2b: nested withDecodingDefaultKey config defaults", () => {
	// withDecodingDefaultKey wraps the schema in optionalKey itself; the default is an
	// Effect producing the ENCODED value. `encodingStrategy: "omit"` makes encode drop
	// defaulted keys again — the raw-round-trip behavior the kit Config service wants.
	const SyncCfg = Schema.Struct({
		pollMinutes: Schema.Number.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed(15), {
				encodingStrategy: "omit",
			}),
		),
		watch: Schema.Boolean.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed(true), {
				encodingStrategy: "omit",
			}),
		),
	});
	const Cfg = Schema.Struct({
		roots: Schema.Array(Schema.String).pipe(
			Schema.withDecodingDefaultKey(Effect.succeed([]), {
				encodingStrategy: "omit",
			}),
		),
		sync: SyncCfg.pipe(
			Schema.withDecodingDefaultKey(Effect.succeed({}), {
				encodingStrategy: "omit",
			}),
		),
	});

	test("empty raw file decodes to full defaults (nested)", () => {
		const decoded = Schema.decodeUnknownSync(Cfg)({});
		expect(decoded).toEqual({
			roots: [],
			sync: { pollMinutes: 15, watch: true },
		});
	});

	test("partially-authored raw keeps authored values, fills the rest", () => {
		const decoded = Schema.decodeUnknownSync(Cfg)({
			roots: ["/roms"],
			sync: { pollMinutes: 5 },
		});
		expect(decoded).toEqual({
			roots: ["/roms"],
			sync: { pollMinutes: 5, watch: true },
		});
	});

	test("unknown keys in the raw file are tolerated (legacy ui/devEntry)", () => {
		const decoded = Schema.decodeUnknownSync(Cfg)({
			ui: { port: 5885 },
			devEntry: true,
		} as unknown);
		expect(decoded.roots).toEqual([]);
	});
});
