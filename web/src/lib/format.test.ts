import { describe, expect, test } from "bun:test";
import { fmtAgo, fmtClockDuration, fmtSpan } from "./format";

describe("fmtClockDuration", () => {
	test("reads seconds as m:ss and clamps below zero", () => {
		expect(fmtClockDuration(0)).toBe("0:00");
		expect(fmtClockDuration(65.9)).toBe("1:05");
		expect(fmtClockDuration(-3)).toBe("0:00");
		expect(fmtClockDuration(3725)).toBe("62:05");
	});

	test("switches to h:mm from an hour on when asked", () => {
		expect(fmtClockDuration(3599, { hours: true })).toBe("59:59");
		expect(fmtClockDuration(3600, { hours: true })).toBe("1:00");
		expect(fmtClockDuration(3725, { hours: true })).toBe("1:02");
	});
});

describe("fmtSpan", () => {
	test("minutes under an hour, hours and minutes after", () => {
		expect(fmtSpan(59)).toBe("0 min");
		expect(fmtSpan(2_520)).toBe("42 min");
		expect(fmtSpan(3_600)).toBe("1 hr");
		expect(fmtSpan(4_330)).toBe("1 hr 12 min");
	});
});

describe("fmtAgo", () => {
	test("picks the largest whole unit, and now under a minute", () => {
		const now = 1_000_000;
		expect(fmtAgo(now - 30, now)).toBe("now");
		expect(fmtAgo(now - 125, now)).toBe("2 minutes ago");
		expect(fmtAgo(now - 2 * 3600 - 59, now)).toBe("2 hours ago");
		expect(fmtAgo(now - 86_400, now)).toBe("yesterday");
	});
});
