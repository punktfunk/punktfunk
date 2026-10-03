// The `/__install` contract and the progress reporter's rules.
import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import {
	type DownloadRow,
	InstallRefused,
	makeDownloadReporter,
	makeInstallHandler,
	NotMyTitle,
	type SendResult,
} from "../src/downloads.js";

const post = (body: unknown) =>
	new Request("http://127.0.0.1/__install", {
		method: "POST",
		body: JSON.stringify(body),
	});

const ask = (action: string) => ({
	app: "custom:abc",
	external_id: "romm/x/1",
	action,
});

describe("install handler", () => {
	const seen: string[] = [];
	const handle = makeInstallHandler({
		start: (a) => Effect.sync(() => void seen.push(`start ${a.externalId}`)),
		pause: () => Effect.void,
		cancel: (a) => Effect.fail(new NotMyTitle({ externalId: a.externalId })),
		uninstall: () =>
			Effect.fail(new InstallRefused({ message: "Nothing to remove." })),
	});

	test("starting and pausing answer 202 with the title passed through", async () => {
		expect((await handle(post(ask("start")))).status).toBe(202);
		expect(seen).toEqual(["start romm/x/1"]);
		expect((await handle(post(ask("pause")))).status).toBe(202);
	});

	test("a refusal answers 409 with its sentence, a stranger's title 404", async () => {
		const res = await handle(post(ask("uninstall")));
		expect(res.status).toBe(409);
		expect(await res.json()).toEqual({ message: "Nothing to remove." });
		expect((await handle(post(ask("cancel")))).status).toBe(404);
	});

	test("refuses a malformed ask", async () => {
		expect(
			(await handle(post({ ...ask("start"), action: "nuke" }))).status,
		).toBe(400);
		expect((await handle(post({ action: "start" }))).status).toBe(400);
		const get = new Request("http://127.0.0.1/__install");
		expect((await handle(get)).status).toBe(405);
	});

	test("any other failure answers 500", async () => {
		const failing = makeInstallHandler({
			start: () => Effect.fail("disk full"),
			pause: () => Effect.void,
			cancel: () => Effect.void,
			uninstall: () => Effect.void,
		});
		expect((await failing(post(ask("start")))).status).toBe(500);
	});
});

const row = (state: DownloadRow["state"], doneBytes = 0): DownloadRow => ({
	externalId: "a",
	state,
	doneBytes,
	totalBytes: 100,
});

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe("download reporter", () => {
	test("batches changes inside the throttle and sends a finished row once", async () => {
		const sent: ReadonlyArray<DownloadRow>[] = [];
		const r = makeDownloadReporter(
			async (rows) => {
				sent.push(rows);
				return "ok";
			},
			{ throttleMs: 40, heartbeatMs: 10_000 },
		);
		r.set(row("downloading", 10));
		r.set(row("downloading", 20));
		await sleep(10);
		expect(sent).toHaveLength(1);
		expect(sent[0]?.[0]?.doneBytes).toBe(20);
		r.set(row("done", 100));
		await sleep(60);
		expect(sent.at(-1)?.[0]?.state).toBe("done");
		const count = sent.length;
		await r.flush();
		expect(sent).toHaveLength(count);
		expect(r.supported()).toBe(true);
		r.close();
	});

	test("an old host's 404 stops reporting for good", async () => {
		let calls = 0;
		const r = makeDownloadReporter(
			async (): Promise<SendResult> => {
				calls++;
				return "unsupported";
			},
			{ throttleMs: 1, heartbeatMs: 10_000 },
		);
		r.set(row("downloading"));
		await sleep(10);
		expect(r.supported()).toBe(false);
		r.set(row("downloading", 5));
		await sleep(10);
		expect(calls).toBe(1);
		r.close();
	});

	test("restates a live row on the heartbeat and keeps a row the host didn't take", async () => {
		const states: string[] = [];
		let fail = true;
		const r = makeDownloadReporter(
			async (rows) => {
				states.push(rows.map((x) => x.state).join());
				if (fail) {
					fail = false;
					return "failed";
				}
				return "ok";
			},
			{ throttleMs: 1, heartbeatMs: 20 },
		);
		r.set(row("downloading"));
		await sleep(70);
		expect(states.length).toBeGreaterThanOrEqual(3);
		expect(new Set(states)).toEqual(new Set(["downloading"]));
		r.close();
	});
});
