// sseRoute: frame format + integration with the toWebHandler pipeline.
import { describe, expect, test } from "bun:test";
import { Layer, Schedule, Stream } from "effect";
import { HttpRouter } from "effect/http";
import { httpApiEnv, sseRoute } from "../src/index.js";

describe("sseRoute", () => {
	test("streams event frames in SSE wire format", async () => {
		const stream = Stream.fromSchedule(Schedule.spaced("5 millis")).pipe(
			Stream.map((n) => ({ tick: Number(n) })),
			Stream.take(3),
		);
		const routes = sseRoute("/api/events", stream, {
			event: "status",
			pingSeconds: 0,
		});
		const { handler, dispose } = HttpRouter.toWebHandler(
			Layer.provide(routes, httpApiEnv),
		);
		try {
			const res = await handler(new Request("http://127.0.0.1/api/events"));
			expect(res.status).toBe(200);
			expect(res.headers.get("content-type")).toContain("text/event-stream");
			const text = await res.text();
			expect(text).toContain('event: status\ndata: {"tick":0}\n\n');
			expect(text).toContain('event: status\ndata: {"tick":2}\n\n');
		} finally {
			await dispose();
		}
	});
});
