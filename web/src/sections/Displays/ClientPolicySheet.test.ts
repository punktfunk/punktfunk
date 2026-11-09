// A device's screen: what its row says in the summary, what the sheet offers, what a pick sends.
import { describe, expect, test } from "bun:test";
import type { ApiMonitorInfo } from "@/api/gen/model";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import {
	nextOverlay,
	overlaySummary,
	screenChoices,
} from "./ClientPolicySheet";

overwriteGetLocale(() => baseLocale);

const mon = (over: Partial<ApiMonitorInfo>): ApiMonitorInfo => ({
	connector: "DP-1",
	description: "Dell U2718Q",
	enabled: true,
	managed: false,
	mode: "2560x1440@120",
	primary: false,
	scale: 1,
	selected: false,
	x: 0,
	y: 0,
	...over,
});

describe("overlaySummary", () => {
	test("says own screen, a monitor, or nothing", () => {
		expect(overlaySummary({ screen: "own" })).toBe("Own screen");
		expect(overlaySummary({ screen: { monitor: "HDMI-A-1" } })).toBe(
			"Mirrors HDMI-A-1",
		);
		expect(overlaySummary({ capture_monitor: "DP-2" })).toBe("Mirrors DP-2");
		expect(overlaySummary({})).toBe("Follows host");
	});
});

describe("screenChoices", () => {
	const heads = [
		mon({ connector: "DP-1" }),
		mon({ connector: "HDMI-A-1", enabled: false }),
		mon({ connector: "Virtual-1", managed: true }),
	];

	test("own screen first, then each monitor that can be streamed", () => {
		expect(screenChoices(heads, undefined)).toEqual([
			["own", "Own screen"],
			["DP-1", "DP-1"],
		]);
	});

	test("the device's monitor stays listed when it is off", () => {
		expect(
			screenChoices(heads, { monitor: "HDMI-A-1" }).map(([id]) => id),
		).toEqual(["own", "DP-1", "HDMI-A-1"]);
	});
});

describe("nextOverlay", () => {
	const stored = {
		topology: "exclusive" as const,
		screen: { monitor: "DP-1" },
		capture_monitor: "DP-1",
		set_by: "device" as const,
	};

	test("Follow host drops the screen and its alias", () => {
		expect(nextOverlay(stored, { screen: null })).toEqual({
			topology: "exclusive",
		});
	});

	test("a pick writes screen, never capture_monitor", () => {
		expect(nextOverlay(stored, { screen: "own" })).toEqual({
			topology: "exclusive",
			screen: "own",
		});
	});

	test("another answer keeps the screen, read from the alias if need be", () => {
		expect(
			nextOverlay({ capture_monitor: "DP-2" }, { topology: "extend" }),
		).toEqual({ screen: { monitor: "DP-2" }, topology: "extend" });
	});
});
