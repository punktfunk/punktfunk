// Spike 1 (plan Phase 0): prove that an `effect/http-api` HttpApi can serve as the
// plugin-local API behind the SDK's `servePluginUi` on Bun, using ONLY effect-core layers
// (no @effect/platform-node / platform-bun) — the riskiest seam of the plugin-kit design.
//
// Validates:
//   1. HttpApi + HttpApiBuilder.group + HttpRouter.toWebHandler answer plain fetch Requests.
//   2. The handler slots into servePluginUi's `fetch` contract: /api/* handled, everything
//      else falls through (returns undefined) to the static/404 path.
//   3. The real servePluginUi server (loopback, per-boot bearer secret, __health) proxies
//      into the HttpApi handler end-to-end.
import { describe, expect, test } from "bun:test";
import type { Punktfunk } from "@punktfunk/host";
import { servePluginUi } from "@punktfunk/host";
import { Effect, Layer, Schema } from "effect";
import * as FileSystem from "effect/FileSystem";
import { Etag, HttpPlatform, HttpRouter } from "effect/http";
import {
	HttpApi,
	HttpApiBuilder,
	HttpApiEndpoint,
	HttpApiGroup,
} from "effect/http-api";
import * as Path from "effect/Path";

const Pong = Schema.Struct({ ok: Schema.Boolean, source: Schema.String });
const EchoIn = Schema.Struct({ msg: Schema.String });
const EchoOut = Schema.Struct({ echoed: Schema.String });

const api = HttpApi.make("spike").add(
	HttpApiGroup.make("spike")
		.add(HttpApiEndpoint.get("ping", "/api/ping", { success: Pong }))
		.add(
			HttpApiEndpoint.post("echo", "/api/echo", {
				payload: EchoIn,
				success: EchoOut,
			}),
		),
);

const groupLive = HttpApiBuilder.group(api, "spike", (handlers) =>
	handlers
		.handle("ping", () => Effect.succeed({ ok: true, source: "httpapi" }))
		.handle("echo", ({ payload }) => Effect.succeed({ echoed: payload.msg })),
);

// Core-only environment for HttpApiBuilder: no platform package needed. FileSystem is
// provideMerge'd so both HttpPlatform.layer and HttpApiBuilder see it satisfied.
const env = Layer.provideMerge(
	Layer.mergeAll(Etag.layerWeak, Path.layer, HttpPlatform.layer),
	FileSystem.layerNoop({}),
);

const appLayer = HttpApiBuilder.layer(api).pipe(
	Layer.provide(groupLive),
	Layer.provide(env),
);

describe("spike 1: HttpApi via toWebHandler on Bun", () => {
	test("handles fetch-shaped requests directly", async () => {
		const { handler, dispose } = HttpRouter.toWebHandler(appLayer);
		try {
			const ping = await handler(new Request("http://127.0.0.1/api/ping"));
			expect(ping.status).toBe(200);
			expect(await ping.json()).toEqual({ ok: true, source: "httpapi" });

			const echo = await handler(
				new Request("http://127.0.0.1/api/echo", {
					method: "POST",
					headers: { "content-type": "application/json" },
					body: JSON.stringify({ msg: "hello" }),
				}),
			);
			expect(echo.status).toBe(200);
			expect(await echo.json()).toEqual({ echoed: "hello" });

			// Schema validation is live: bad payload is rejected, not 500.
			const bad = await handler(
				new Request("http://127.0.0.1/api/echo", {
					method: "POST",
					headers: { "content-type": "application/json" },
					body: JSON.stringify({ nope: 1 }),
				}),
			);
			expect(bad.status).toBeGreaterThanOrEqual(400);
			expect(bad.status).toBeLessThan(500);
		} finally {
			await dispose();
		}
	});

	test("end-to-end behind servePluginUi (loopback + bearer secret)", async () => {
		const { handler, dispose } = HttpRouter.toWebHandler(appLayer);
		const registrations: Array<{
			method: string;
			path: string;
			body: unknown;
		}> = [];
		// servePluginUi only touches pf.request — a recording stub is a faithful host.
		const pf = {
			request: async (method: string, path: string, body?: unknown) => {
				registrations.push({ method, path, body });
				return undefined;
			},
		} as unknown as Punktfunk;

		const kitFetch = async (req: Request): Promise<Response | undefined> => {
			const url = new URL(req.url);
			if (!url.pathname.startsWith("/api/")) return undefined; // static/404 fallthrough
			return handler(req);
		};

		const ui = await servePluginUi(pf, {
			id: "spike",
			title: "Spike",
			fetch: kitFetch,
		});
		try {
			const reg = registrations.find(
				(r) => r.method === "PUT" && r.path === "/plugins/spike",
			);
			expect(reg).toBeDefined();
			// Not `reg?.body`: the optional chain undoes the assertion above — if `reg` were
			// undefined the `.ui` access would throw a TypeError instead of failing this test
			// readably. The `expect` is what guarantees it, so assert it to the type system too.
			if (!reg) throw new Error("registration not found");
			const secret = (reg.body as { ui: { secret: string } }).ui.secret;
			expect(secret.length).toBeGreaterThanOrEqual(16);
			const auth = { authorization: `Bearer ${secret}` };

			// Health endpoint is served by servePluginUi itself.
			const health = await fetch(`http://127.0.0.1:${ui.port}/__health`, {
				headers: auth,
			});
			expect(health.status).toBe(200);

			// HttpApi endpoint through the real server.
			const ping = await fetch(`http://127.0.0.1:${ui.port}/api/ping`, {
				headers: auth,
			});
			expect(ping.status).toBe(200);
			expect(await ping.json()).toEqual({ ok: true, source: "httpapi" });

			// Wrong secret is rejected before reaching the handler.
			const denied = await fetch(`http://127.0.0.1:${ui.port}/api/ping`, {
				headers: { authorization: "Bearer nope-nope-nope-nope" },
			});
			expect(denied.status).toBe(401);

			// Non-/api path falls through past our fetch (no staticDir here → 404).
			const missing = await fetch(`http://127.0.0.1:${ui.port}/somewhere`, {
				headers: auth,
			});
			expect(missing.status).toBe(404);
		} finally {
			await ui.close();
			await dispose();
		}
	});
});
