import { describe, expect, test } from "bun:test";
import { classifyFrame, SseAuthError, SseParser, sseFrames } from "../src/sse.js";
import type { ResolvedConfig } from "../src/config.js";
import { staticBearer } from "../src/credential.js";

describe("SseParser", () => {
	test("parses frames split across arbitrary chunks, skipping comments", () => {
		const p = new SseParser();
		let frames = p.push("id: 4\nevent: library.ch");
		expect(frames.length).toBe(0);
		frames = p.push('anged\ndata: {"seq":4}\n\n: keep-alive\n\nid: 5\n');
		expect(frames.length).toBe(1);
		expect(frames[0]).toEqual({ event: "library.changed", data: '{"seq":4}', id: "4" });
		frames = p.push("data: x\n\n");
		expect(frames.length).toBe(1);
		expect(frames[0]?.id).toBe("5");
		expect(frames[0]?.event).toBe("message");
	});

	test("joins multi-line data and handles CRLF", () => {
		const p = new SseParser();
		const frames = p.push("data: a\r\ndata: b\r\n\r\n");
		expect(frames[0]?.data).toBe("a\nb");
	});
});

const cfgFor = (port: number, token = "t"): ResolvedConfig => ({
	url: `http://127.0.0.1:${port}`,
	token,
	credential: staticBearer(token),
	fetch,
});

describe("sseFrames", () => {
	test("reads frames, reconnects with Last-Event-ID after a server close", async () => {
		const lastEventIds: Array<string | null> = [];
		let connection = 0;
		const server = Bun.serve({
			port: 0,
			fetch(req) {
				lastEventIds.push(req.headers.get("last-event-id"));
				connection += 1;
				const first = connection === 1;
				const body = new ReadableStream({
					start(controller) {
						const enc = new TextEncoder();
						if (first) {
							controller.enqueue(enc.encode('id: 1\nevent: library.changed\ndata: {"seq":1}\n\n'));
							controller.close(); // server closes → client must reconnect
						} else {
							controller.enqueue(enc.encode('id: 2\nevent: library.changed\ndata: {"seq":2}\n\n'));
							// stay open
						}
					},
				});
				return new Response(body, { headers: { "content-type": "text/event-stream" } });
			},
		});
		try {
			const gen = sseFrames(cfgFor(server.port as number), { onWarning: () => {} });
			const f1 = await gen.next();
			expect(f1.value?.id).toBe("1");
			const f2 = await gen.next(); // spans the reconnect
			expect(f2.value?.id).toBe("2");
			await gen.return(undefined);
			// No `since` = live-tail-only: the first connect carries the beyond-tip cursor,
			// the reconnect carries the last REAL id.
			expect(lastEventIds[0]).toBe(String(Number.MAX_SAFE_INTEGER));
			expect(lastEventIds[1]).toBe("1");
		} finally {
			server.stop(true);
		}
	});

	test("401 is terminal (no retry loop)", async () => {
		const server = Bun.serve({
			port: 0,
			fetch: () => new Response("{}", { status: 401 }),
		});
		try {
			const gen = sseFrames(cfgFor(server.port as number), { onWarning: () => {} });
			await expect(gen.next()).rejects.toBeInstanceOf(SseAuthError);
		} finally {
			server.stop(true);
		}
	});

	test("the signal ends a stream backing off from a gone host", async () => {
		const server = Bun.serve({ port: 0, fetch: () => new Response("") });
		const port = server.port as number;
		server.stop(true);
		const stop = new AbortController();
		const warnings: string[] = [];
		const gen = sseFrames(cfgFor(port), {
			onWarning: (m) => {
				warnings.push(m);
				stop.abort();
			},
			signal: stop.signal,
		});
		expect((await gen.next()).done).toBe(true);
		expect(warnings).toHaveLength(1);
	});
});

describe("classifyFrame", () => {
	const frame = (event: string, data: string) => ({ event, data });
	test("sorts every frame the host sends into one of five classes", () => {
		expect(classifyFrame(frame("dropped", "")).tag).toBe("dropped");
		expect(classifyFrame(frame("live", "")).tag).toBe("live");
		expect(classifyFrame(frame("x", "{not json")).tag).toBe("garbled");
		expect(
			classifyFrame(frame("later.kind", '{"seq":1,"ts_ms":1,"schema":1,"kind":"later.kind"}')),
		).toEqual({
			tag: "unknown",
			json: { seq: 1, ts_ms: 1, schema: 1, kind: "later.kind" },
		});
		const ev = classifyFrame(
			frame("host.stopping", '{"seq":2,"ts_ms":1,"schema":1,"kind":"host.stopping"}'),
		);
		expect(ev.tag === "event" && ev.event.kind).toBe("host.stopping");
	});
});
