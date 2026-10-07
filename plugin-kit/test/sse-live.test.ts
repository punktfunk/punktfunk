// Regression: the production shape of sseRoute — a long-lived PubSub-backed stream
// (the engine's status feed) plus the keepalive. The original suite only covered a
// finite, self-driving stream, which hid the fact that nothing ever reached the wire.
import { describe, expect, test } from "bun:test";
import { Effect, Layer, PubSub, Stream } from "effect";
import { HttpRouter } from "effect/http";
import { httpApiEnv, sseRoute } from "../src/index.js";

/**
 * Read until the first bytes arrive or `ms` elapses. One sequential read at a time —
 * re-entering read() while a previous read is pending is a spec violation and silently
 * swallows data (which is exactly how this harness first lied about the ping path).
 */
const readSome = async (res: Response, ms: number): Promise<string> => {
	const reader = res.body?.getReader();
	if (!reader) return "";
	const decoder = new TextDecoder();
	let out = "";
	const timer = setTimeout(() => void reader.cancel().catch(() => {}), ms);
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			if (value) out += decoder.decode(value, { stream: true });
			if (out.length > 0) break;
		}
	} catch {
		// cancelled by the deadline
	} finally {
		clearTimeout(timer);
		await reader.cancel().catch(() => {});
	}
	return out;
};

describe("sseRoute (live, PubSub-backed)", () => {
	test("delivers frames published AFTER the request opened", async () => {
		const program = Effect.gen(function* () {
			const hub = yield* PubSub.unbounded<{ n: number }>();
			const routes = sseRoute("/api/events", Stream.fromPubSub(hub), {
				event: "status",
				pingSeconds: 0,
			});
			const { handler, dispose } = HttpRouter.toWebHandler(
				Layer.provide(routes, httpApiEnv),
			);
			const res = yield* Effect.promise(() =>
				handler(new Request("http://127.0.0.1/api/events")),
			);
			expect(res.status).toBe(200);
			// Publish only once the response is open — the real engine's pattern.
			setTimeout(() => {
				Effect.runFork(PubSub.publish(hub, { n: 1 }));
			}, 50);
			const body = yield* Effect.promise(() => readSome(res, 3000));
			yield* Effect.promise(() => dispose());
			return body;
		});
		const body = await Effect.runPromise(Effect.scoped(program));
		expect(body).toContain('event: status\ndata: {"n":1}');
	});

	test("emits a keepalive on an otherwise silent stream", async () => {
		const program = Effect.gen(function* () {
			const hub = yield* PubSub.unbounded<{ n: number }>();
			const routes = sseRoute("/api/events", Stream.fromPubSub(hub), {
				event: "status",
				pingSeconds: 1,
			});
			const { handler, dispose } = HttpRouter.toWebHandler(
				Layer.provide(routes, httpApiEnv),
			);
			const res = yield* Effect.promise(() =>
				handler(new Request("http://127.0.0.1/api/events")),
			);
			const body = yield* Effect.promise(() => readSome(res, 4000));
			yield* Effect.promise(() => dispose());
			return body;
		});
		const body = await Effect.runPromise(Effect.scoped(program));
		expect(body).toContain(": ping");
	});
});
