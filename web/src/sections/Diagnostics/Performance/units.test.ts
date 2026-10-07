import { describe, expect, test } from "bun:test";
import { dur, frameBudgetMs, latencyCeilingMs, msTick } from "./units";

describe("stats units", () => {
	test("microseconds read as milliseconds from 1 ms up", () => {
		expect(dur(8333)).toBe("8.3 ms");
		expect(dur(1000)).toBe("1.0 ms");
		expect(dur(450)).toBe("450 µs");
		expect(dur(15.4)).toBe("15 µs");
		expect(dur(Number.NaN)).toBe("—");
	});

	test("the frame budget follows the stream's rate", () => {
		expect(frameBudgetMs(120)).toBeCloseTo(8.333, 3);
		expect(frameBudgetMs(30)).toBeCloseTo(33.333, 3);
		expect(frameBudgetMs(0)).toBe(0);
	});

	test("an 8 ms stage sits under a 120 Hz frame and low on a 30 Hz one", () => {
		const at120 = latencyCeilingMs(8, frameBudgetMs(120));
		const at30 = latencyCeilingMs(8, frameBudgetMs(30));
		expect(8 / at120).toBeGreaterThan(0.75);
		expect(8 / at30).toBeLessThan(0.25);
		// Data above the budget still fits on the axis.
		expect(latencyCeilingMs(50, frameBudgetMs(120))).toBe(50);
		// An empty chart with no known rate still gets a usable axis.
		expect(latencyCeilingMs(0, 0)).toBe(1);
	});

	test("ticks carry a decimal only while it means something", () => {
		expect(msTick(2.5)).toBe("2.5");
		expect(msTick(12)).toBe("12");
		expect(msTick(0)).toBe("0");
	});
});
