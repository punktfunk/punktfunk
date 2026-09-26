// The wire schemas must decode EXACTLY what the host emits — the JSON literals here are the
// Rust side's snapshot-test strings (crates/punktfunk-host/src/events.rs), the schema gate.
import { describe, expect, test } from "bun:test";
import { decodeHostEvent, kindMatches } from "../src/wire.js";

describe("wire", () => {
	test("decodes the host's snapshot frames", () => {
		const stream = decodeHostEvent(
			JSON.parse(
				'{"seq":4182,"ts_ms":1700000000000,"schema":1,"kind":"stream.started","stream":{"mode":"3840x2160@120","hdr":true,"client":"Living Room TV","app":"steam:570","plane":"native"}}',
			),
		);
		expect(stream._tag).toBe("Success");
		if (stream._tag === "Success" && stream.success.kind === "stream.started") {
			expect(stream.success.stream.mode).toBe("3840x2160@120");
			expect(stream.success.stream.app).toBe("steam:570");
		}

		const disc = decodeHostEvent(
			JSON.parse(
				'{"seq":1,"ts_ms":1700000000000,"schema":1,"kind":"client.disconnected","client":{"name":"Deck","fingerprint":"b1c2","plane":"gamestream"},"reason":"timeout"}',
			),
		);
		expect(disc._tag).toBe("Success");
		if (disc._tag === "Success" && disc.success.kind === "client.disconnected") {
			expect(disc.success.reason).toBe("timeout");
		}

		const stopping = decodeHostEvent(
			JSON.parse('{"seq":2,"ts_ms":1700000000000,"schema":1,"kind":"host.stopping"}'),
		);
		expect(stopping._tag).toBe("Success");
	});

	test("keeps the device and its preset on a game event", () => {
		const r = decodeHostEvent(
			JSON.parse(
				'{"seq":5,"ts_ms":1700000000000,"schema":1,"kind":"game.exited","game":{"app":"steam:504230","title":"Celeste","client":"Deck","fingerprint":"ab12cd","plane":"native","preset":{"id":"3f9a0c11e2b4","name":"Docked"}},"reason":"exited"}',
			),
		);
		expect(r._tag).toBe("Success");
		if (r._tag === "Success" && r.success.kind === "game.exited") {
			expect(r.success.game.fingerprint).toBe("ab12cd");
			expect(r.success.game.preset).toEqual({ id: "3f9a0c11e2b4", name: "Docked" });
		}
	});

	test("decodes the launch stage a plugin holds", () => {
		const r = decodeHostEvent(
			JSON.parse(
				'{"seq":5,"ts_ms":1700000000000,"schema":1,"kind":"game.launching","game":{"app":"steam:570","title":"Dota 2","store":"steam","client":"a1b2c3d4e5f6","fingerprint":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","plane":"native","preset":{"id":"3f9a0c11e2b4","name":"Docked"}}}',
			),
		);
		expect(r._tag).toBe("Success");
		if (r._tag === "Success" && r.success.kind === "game.launching") {
			expect(r.success.game.preset?.name).toBe("Docked");
		}
		for (const kind of ["store.changed", "plugins.changed"]) {
			const ev = decodeHostEvent({ seq: 1, ts_ms: 1, schema: 1, kind, id: "x" });
			expect(ev._tag).toBe("Success");
		}
	});

	test("tolerates unknown keys (additive-only wire)", () => {
		const r = decodeHostEvent({
			seq: 9,
			ts_ms: 1,
			schema: 1,
			kind: "library.changed",
			source: "manual",
			future_field: { anything: true },
		});
		expect(r._tag).toBe("Success");
	});

	test("unknown kinds fail decode (they ride the raw channel)", () => {
		const r = decodeHostEvent({
			seq: 9,
			ts_ms: 1,
			schema: 1,
			kind: "totally.new",
		});
		expect(r._tag).toBe("Failure");
	});

	test("kindMatches mirrors the host filter semantics", () => {
		expect(kindMatches("stream.started", "stream.started")).toBe(true);
		expect(kindMatches("stream.*", "stream.stopped")).toBe(true);
		expect(kindMatches("stream.*", "streamx.started")).toBe(false);
		expect(kindMatches("stream.started", "stream.stopped")).toBe(false);
	});
});
