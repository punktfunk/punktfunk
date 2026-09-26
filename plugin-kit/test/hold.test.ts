// The `/__hold` contract the host's launch stage calls.
import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { makeHoldHandler } from "../src/ui-server.js";

const launching = {
	seq: 5,
	ts_ms: 1700000000000,
	schema: 1,
	kind: "game.launching",
	game: {
		app: "steam:570",
		title: "Dota 2",
		client: "a1b2c3d4e5f6",
		fingerprint: "ab12",
		plane: "native",
		preset: { id: "3f9a0c11e2b4", name: "Docked" },
	},
};

const post = (body: unknown, signal?: AbortSignal) =>
	new Request("http://127.0.0.1/__hold", {
		method: "POST",
		body: JSON.stringify(body),
		...(signal ? { signal } : {}),
	});

describe("launch hold", () => {
	test("runs the stage's handler with the game and answers 204", async () => {
		const seen: string[] = [];
		const handle = makeHoldHandler({
			"game.launching": (game) =>
				Effect.sync(() => {
					seen.push(`${game.app} ${game.preset?.name}`);
				}),
		});
		const res = await handle(post(launching));
		expect(res.status).toBe(204);
		expect(seen).toEqual(["steam:570 Docked"]);
	});

	test("refuses what is not a held stage", async () => {
		const handle = makeHoldHandler({});
		expect((await handle(post(launching))).status).toBe(404);
		const running = { ...launching, kind: "game.running" };
		expect((await handle(post(running))).status).toBe(400);
		const get = new Request("http://127.0.0.1/__hold");
		expect((await handle(get)).status).toBe(405);
	});

	test("a failed handler answers 500 so the host logs it", async () => {
		const handle = makeHoldHandler({
			"game.launching": () => Effect.fail("disk full"),
		});
		expect((await handle(post(launching))).status).toBe(500);
	});

	test("the host giving up interrupts the handler", async () => {
		let finished = false;
		const handle = makeHoldHandler({
			"game.launching": () =>
				Effect.sleep("5 seconds").pipe(
					Effect.andThen(
						Effect.sync(() => {
							finished = true;
						}),
					),
				),
		});
		const abort = new AbortController();
		setTimeout(() => abort.abort(), 50);
		const res = await handle(post(launching, abort.signal));
		expect(res.status).toBe(500);
		expect(finished).toBe(false);
	});
});
