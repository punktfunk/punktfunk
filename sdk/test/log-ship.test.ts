// The log shipper's contract: what it recovers from a formatted line, that it tees rather than
// swallows, that a POST failure neither loses lines nor spins, and that it stays bounded.
import { afterEach, describe, expect, test } from "bun:test";
import { __test, installLogShipper } from "../src/log-ship.js";

const { toLines } = __test;

const TOKEN = "ship-token";

interface Captured {
	entries: { ts_ms: number; level: string; source: string; msg: string }[];
}

/** A host that records every batch, answering with whatever `status()` says. */
const mockHost = (status: () => number = () => 204) => {
	const batches: Captured[] = [];
	const auth: (string | null)[] = [];
	const server = Bun.serve({
		port: 0,
		fetch: async (req) => {
			const url = new URL(req.url);
			if (url.pathname !== "/api/v1/plugins/logs") {
				return new Response("not found", { status: 404 });
			}
			auth.push(req.headers.get("authorization"));
			batches.push((await req.json()) as Captured);
			const s = status();
			return new Response(s === 204 ? null : "nope", { status: s });
		},
	});
	return {
		batches,
		auth,
		url: `http://127.0.0.1:${server.port}`,
		stop: () => server.stop(true),
	};
};

/**
 * Run `body` with a shipper installed, always restoring the console.
 *
 * The console is swapped for a recorder BEFORE the shipper installs, so `seen` is what the shipper
 * teed through to "stdout" — and the suite stays readable, since a test that logs 2000 lines would
 * otherwise print all 2000.
 */
const withShipper = async <T>(
	url: string,
	body: (
		s: ReturnType<typeof installLogShipper>,
		seen: string[],
	) => Promise<T>,
): Promise<T> => {
	const seen: string[] = [];
	const real = { log: console.log, warn: console.warn, error: console.error };
	const record = (...a: unknown[]) => {
		seen.push(String(a[0]));
	};
	console.log = record;
	console.warn = record;
	console.error = record;
	const shipper = installLogShipper({
		connect: { url, token: TOKEN },
		// Long enough that only explicit flushes fire — the tests drive the timing.
		intervalMs: 60_000,
	});
	try {
		return await body(shipper, seen);
	} finally {
		shipper.stop();
		console.log = real.log;
		console.warn = real.warn;
		console.error = real.error;
	}
};

describe("parsing a formatted line", () => {
	test("recovers the plugin name and timestamp plugin-kit's format flattened", () => {
		const [line] = toLines(
			"log",
			["2026-08-03T10:11:12.345Z [virtualhere] holding nothing"],
		);
		expect(line?.source).toBe("virtualhere");
		expect(line?.level).toBe("INFO");
		expect(line?.msg).toBe("holding nothing");
		expect(line?.ts_ms).toBe(Date.parse("2026-08-03T10:11:12.345Z"));
	});

	test("an explicit level in the line beats the console method", () => {
		// The shipper keeps the line's spelling ("WARNING"); the host maps it to WARN.
		const [line] = toLines("log", [
			"2026-08-03T10:11:12.345Z [virtualhere] WARNING: vhclient failed (ETIMEDOUT)",
		]);
		expect(line?.level).toBe("WARNING");
		expect(line?.msg).toBe("vhclient failed (ETIMEDOUT)");
	});

	test("the runner's own error lines keep their severity", () => {
		const [line] = toLines("error", [
			"2026-08-03T10:11:12.345Z [virtualhere] failed: VhIpcError: no such binary",
		]);
		expect(line?.source).toBe("virtualhere");
		expect(line?.level).toBe("ERROR");
	});

	test("an unstamped call is kept, not dropped", () => {
		// A plugin reaching for bare console.error is exactly the case that must not be lost.
		const [line] = toLines("error", ["boom", { code: 7 }]);
		expect(line?.source).toBe("runner");
		expect(line?.level).toBe("ERROR");
		expect(line?.msg).toContain("boom");
		expect(line?.msg).toContain("7");
	});

	test("a multi-line message becomes one entry per line", () => {
		const lines = toLines("log", [
			"2026-08-03T10:11:12.345Z [x] failed\n  at foo\n  at bar",
		]);
		expect(lines.map((l) => l.msg)).toEqual(["failed", "  at foo", "  at bar"]);
		// Every fragment keeps the original stamp, so the trace cannot interleave with other units.
		expect(new Set(lines.map((l) => l.ts_ms)).size).toBe(1);
	});

	test("a pathological dump is capped and says so", () => {
		const lines = toLines("log", [
			`2026-08-03T10:11:12.345Z [x] ${"line\n".repeat(200)}`,
		]);
		expect(lines.length).toBeLessThanOrEqual(41);
		expect(lines.at(-1)?.msg).toContain("more line(s) suppressed");
	});
});

describe("shipping", () => {
	let stopHost: (() => void) | undefined;
	afterEach(() => {
		stopHost?.();
		stopHost = undefined;
	});

	test("tees: stdout still gets the line, and the host gets it too", async () => {
		const host = mockHost();
		stopHost = host.stop;
		const seen = await withShipper(host.url, async (shipper, seen) => {
			console.log("2026-08-03T10:11:12.345Z [virtualhere] hello");
			await shipper.flush();
			return seen;
		});

		// The original console still ran — journald/foreground output is never traded away.
		expect(seen).toEqual(["2026-08-03T10:11:12.345Z [virtualhere] hello"]);
		expect(host.batches).toHaveLength(1);
		expect(host.batches[0]?.entries[0]).toMatchObject({
			source: "virtualhere",
			level: "INFO",
			msg: "hello",
		});
		expect(host.auth[0]).toBe(`Bearer ${TOKEN}`);
	});

	test("a 5xx keeps the lines for the next flush", async () => {
		let status = 500;
		const host = mockHost(() => status);
		stopHost = host.stop;
		await withShipper(host.url, async (shipper) => {
			console.log("2026-08-03T10:11:12.345Z [x] keep me");
			await shipper.flush();
			expect(host.batches).toHaveLength(1);
			status = 204;
			await shipper.flush();
		});
		// Re-sent rather than dropped: the host being down is not the line's fault.
		expect(host.batches).toHaveLength(2);
		expect(host.batches[1]?.entries[0]?.msg).toBe("keep me");
	});

	test("a 4xx drops the batch instead of retrying forever", async () => {
		const host = mockHost(() => 400);
		stopHost = host.stop;
		await withShipper(host.url, async (shipper) => {
			console.log("2026-08-03T10:11:12.345Z [x] malformed");
			await shipper.flush();
			await shipper.flush();
		});
		// A shape the host rejects cannot be fixed by sending it again.
		expect(host.batches).toHaveLength(1);
	});

	test("a line logged WHILE a POST is in flight is not lost", async () => {
		// A plugin logging during the few ms of a POST is the normal case under load, not an edge
		// one. `shipping` guards flush re-entry only; it must never gate `enqueue`.
		let release: (() => void) | undefined;
		const held = new Promise<void>((r) => {
			release = r;
		});
		let arrived: (() => void) | undefined;
		// Resolves once the server actually has the request — i.e. the shipper is genuinely mid-POST.
		// Logging merely "after calling flush()" proves nothing: flush yields at its own awaits long
		// before the fetch starts, so the line would land in the pre-send queue and the test would
		// pass against the broken code.
		const received = new Promise<void>((r) => {
			arrived = r;
		});
		const batches: Captured[] = [];
		const server = Bun.serve({
			port: 0,
			fetch: async (req) => {
				batches.push((await req.json()) as Captured);
				arrived?.();
				await held; // hold this POST open
				return new Response(null, { status: 204 });
			},
		});
		stopHost = () => server.stop(true);

		await withShipper(`http://127.0.0.1:${server.port}`, async (shipper) => {
			console.log("2026-08-03T10:11:12.345Z [x] before");
			const inFlight = shipper.flush();
			await received;
			// The POST is open right now; this line must still ship.
			console.log("2026-08-03T10:11:12.345Z [x] during");
			release?.();
			await inFlight;
			await shipper.flush();
		});

		const all = batches.flatMap((b) => b.entries.map((e) => e.msg));
		expect(all).toContain("before");
		expect(all).toContain("during");
	});

	test("an explicit flush waits for an in-flight one instead of no-opping", async () => {
		// This is the shutdown path. The runner flushes once more after its units' finalizers have
		// run, and those last lines are the ones that say whether the shutdown WAS clean. If a
		// periodic flush happened to be mid-POST, an explicit flush that simply returned would
		// leave them unsent — and the window is widest exactly when the host is slow, which is when
		// the logs matter most.
		let release: (() => void) | undefined;
		const held = new Promise<void>((r) => {
			release = r;
		});
		let arrived: (() => void) | undefined;
		const received = new Promise<void>((r) => {
			arrived = r;
		});
		let first = true;
		const batches: Captured[] = [];
		const server = Bun.serve({
			port: 0,
			fetch: async (req) => {
				batches.push((await req.json()) as Captured);
				if (first) {
					first = false;
					arrived?.();
					await held;
				}
				return new Response(null, { status: 204 });
			},
		});
		stopHost = () => server.stop(true);

		await withShipper(`http://127.0.0.1:${server.port}`, async (shipper) => {
			console.log("2026-08-03T10:11:12.345Z [x] first");
			const slow = shipper.flush();
			await received;
			console.log("2026-08-03T10:11:12.345Z [x] shutdown line");
			release?.();
			// The shutdown flush: must not return until the tail is actually sent.
			await shipper.flush();
			await slow;
		});

		const all = batches.flatMap((b) => b.entries.map((e) => e.msg));
		expect(all).toContain("shutdown line");
	});

	test("overlapping flushes do not double-send", async () => {
		// The timer can fire while a slow POST is still open. Two concurrent flushes would splice
		// disjoint batches out of one queue and deliver them out of order.
		const host = mockHost();
		stopHost = host.stop;
		await withShipper(host.url, async (shipper) => {
			console.log("2026-08-03T10:11:12.345Z [x] one");
			await Promise.all([shipper.flush(), shipper.flush()]);
		});
		expect(host.batches).toHaveLength(1);
		expect(host.batches[0]?.entries).toHaveLength(1);
	});

	test("an unreachable host neither throws into the caller nor grows without bound", async () => {
		// Nothing listening on this port.
		await withShipper("http://127.0.0.1:1", async (shipper) => {
			for (let i = 0; i < 2_000; i++) {
				console.log(`2026-08-03T10:11:12.345Z [x] line ${i}`);
			}
			// The whole point: console.log above must not have thrown, and flush must not reject.
			await shipper.flush();
		});
	});

	test("overflow is announced, not silently swallowed", async () => {
		const host = mockHost();
		stopHost = host.stop;
		await withShipper(host.url, async (shipper) => {
			// Overrun QUEUE_LIMIT while the shipper has had no chance to drain.
			for (let i = 0; i < 1_200; i++) {
				console.log(`2026-08-03T10:11:12.345Z [x] line ${i}`);
			}
			await shipper.flush();
		});
		const first = host.batches[0]?.entries[0];
		expect(first?.level).toBe("WARN");
		expect(first?.msg).toContain("dropped");
		// The tail is what survived — the oldest lines are the ones that went.
		expect(host.batches[0]?.entries[1]?.msg).toBe("line 200");
	});

	test("stop() puts the real console back", async () => {
		const host = mockHost();
		stopHost = host.stop;
		const before = console.log;
		const shipper = installLogShipper({
			connect: { url: host.url, token: TOKEN },
			intervalMs: 60_000,
		});
		expect(console.log).not.toBe(before);
		shipper.stop();
		expect(console.log).toBe(before);
	});
});
