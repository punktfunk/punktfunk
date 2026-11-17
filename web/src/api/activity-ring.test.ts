import { describe, expect, test } from "bun:test";
import { type ActivityEntry, mergeActivity } from "./activity-ring";

const e = (seq: number): ActivityEntry => ({
	seq,
	ts_ms: seq * 1000,
	kind: "library.changed",
	data: {},
});

describe("mergeActivity", () => {
	test("a whole replay lands in one call, newest first", () => {
		// Oldest to newest, which is the order the host replays its ring in.
		const replay = Array.from({ length: 161 }, (_, i) => e(i + 1));
		const ring = mergeActivity([], replay);
		expect(ring.length).toBe(161);
		expect(ring[0]?.seq).toBe(161);
		expect(ring.at(-1)?.seq).toBe(1);
	});

	test("a seq already held is dropped — a reconnect re-delivers its cursor", () => {
		const held = mergeActivity([], [e(1), e(2)]);
		expect(mergeActivity(held, [e(2), e(3)]).map((x) => x.seq)).toEqual([
			3, 2, 1,
		]);
	});

	test("capped at the ring size, keeping the newest", () => {
		const ring = mergeActivity(
			[],
			Array.from({ length: 300 }, (_, i) => e(i + 1)),
			200,
		);
		expect(ring.length).toBe(200);
		expect(ring[0]?.seq).toBe(300);
		expect(ring.at(-1)?.seq).toBe(101);
	});

	test("nothing new returns the same array, so the store skips the render", () => {
		const held = mergeActivity([], [e(1)]);
		expect(mergeActivity(held, [e(1)])).toBe(held);
		expect(mergeActivity(held, [])).toBe(held);
	});
});
