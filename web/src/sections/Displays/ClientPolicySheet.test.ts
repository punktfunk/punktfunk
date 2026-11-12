// A device's screen: what its row says in the summary, what the sheet offers and shows picked,
// what a pick sends, who decides it.
import { describe, expect, test } from "bun:test";
import type { ApiMonitorInfo } from "@/api/gen/model";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import {
	nextOverlay,
	overlaySummary,
	screenAnswer,
	screenChoices,
	screenDecider,
	screenPatch,
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

	test("with roles, the three roles take own screen's place", () => {
		expect(screenChoices(heads, undefined, true)).toEqual([
			["only", "Only screen"],
			["main", "Main screen"],
			["second", "Second screen"],
			["DP-1", "DP-1"],
		]);
	});
});

describe("screenAnswer", () => {
	const host = {
		topology: "primary" as const,
		auto: "extend" as const,
		pin: null,
	};

	test("a monitor row shows its monitor, a full pair its role", () => {
		expect(
			screenAnswer(
				{ screen: { monitor: "DP-1" }, topology: "exclusive" },
				true,
				host,
			),
		).toBe("DP-1");
		expect(
			screenAnswer({ screen: "own", topology: "exclusive" }, true, host),
		).toBe("only");
		expect(screenAnswer({ screen: "own", topology: "auto" }, true, host)).toBe(
			"second",
		);
		expect(screenAnswer({}, true, host)).toBe("follow");
	});

	test("half a pair reads through the host's topology", () => {
		expect(screenAnswer({ screen: "own" }, true, host)).toBe("main");
		expect(screenAnswer({ topology: "extend" }, true, host)).toBe("second");
	});

	test("a topology alone follows a host that streams a monitor", () => {
		expect(
			screenAnswer({ topology: "extend" }, true, { ...host, pin: "DP-2" }),
		).toBe("follow");
		expect(
			screenAnswer({ screen: "own" }, true, { ...host, pin: "DP-2" }),
		).toBe("main");
	});

	test("without roles the screen is asked alone", () => {
		expect(
			screenAnswer({ screen: "own", topology: "extend" }, false, host),
		).toBe("own");
		expect(screenAnswer({ topology: "extend" }, false, host)).toBe("follow");
	});
});

describe("screenPatch", () => {
	test("a role writes the pair, a monitor drops the topology, Follow host clears both", () => {
		expect(screenPatch("only")).toEqual({
			screen: "own",
			topology: "exclusive",
		});
		expect(screenPatch("main")).toEqual({ screen: "own", topology: "primary" });
		expect(screenPatch("second")).toEqual({
			screen: "own",
			topology: "extend",
		});
		expect(screenPatch("DP-1")).toEqual({
			screen: { monitor: "DP-1" },
			topology: null,
		});
		expect(screenPatch("follow")).toEqual({ screen: null, topology: null });
		expect(screenPatch("own")).toEqual({ screen: "own" });
	});

	test("laid over a stored row, it sends the same two fields as before", () => {
		const stored = {
			screen: { monitor: "DP-1" },
			set_by: "device" as const,
			keep_alive: { mode: "off" as const },
		};
		expect(nextOverlay(stored, screenPatch("second"))).toEqual({
			screen: "own",
			topology: "extend",
			keep_alive: { mode: "off" },
		});
		expect(nextOverlay(stored, screenPatch("follow"))).toEqual({
			keep_alive: { mode: "off" },
		});
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

describe("screenDecider", () => {
	test("a device without Choose its screen is the operator's call", () => {
		expect(screenDecider(false, "device", 0xff)).toBe("operator");
		expect(screenDecider(false, undefined, 0x01)).toBe("operator");
	});

	test("a granted device's own pick says so", () => {
		expect(screenDecider(false, "device", 0x1ff)).toBe("device");
		expect(screenDecider(false, "device", null)).toBe("device");
		expect(screenDecider(false, undefined, 0x100)).toBeUndefined();
	});

	test("the env pin outranks both; a host without access reads as before", () => {
		expect(screenDecider(true, "device", 0)).toBe("env");
		expect(screenDecider(false, "device", undefined)).toBe("device");
	});
});
