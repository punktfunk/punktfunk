import { describe, expect, test } from "bun:test";
import type { ApiDisplayInfo, ApiMonitorInfo } from "@/api/gen/model";
import { bounds, ghostBox, parseMode, snap, toBoxes } from "./DesktopMap";

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

const disp = (over: Partial<ApiDisplayInfo>): ApiDisplayInfo => ({
	backend: "kwin",
	display_index: 0,
	group: 0,
	mode: "3840x2160@120",
	sessions: 1,
	slot: 1,
	state: "active",
	topology: "extend",
	x: 2560,
	y: 0,
	...over,
});

describe("parseMode", () => {
	test.each([
		["2560x1440@120", { w: 2560, h: 1440 }],
		["1920x1080", { w: 1920, h: 1080 }],
	])("%s", (mode, expected) => {
		expect(parseMode(mode)).toEqual(expected);
	});

	// A head the host could not read a mode for has no size, so it cannot be drawn to scale.
	test.each(["", "unknown", "0x0", "x1080"])("%p has no size", (mode) => {
		expect(parseMode(mode)).toBeUndefined();
	});
});

describe("toBoxes", () => {
	// On KWin our own virtual displays also appear in the monitor list. Drawing both would
	// double every streaming screen on the map.
	test("a managed head is not drawn twice", () => {
		const boxes = toBoxes(
			[mon({}), mon({ connector: "pf-virtual-1", managed: true, x: 2560 })],
			[disp({})],
		);
		expect(boxes.map((b) => b.key)).toEqual(["mon-DP-1", "slot-1"]);
	});

	test("a head with an unreadable mode is skipped, not drawn at zero size", () => {
		expect(toBoxes([mon({ mode: "unknown" })], [])).toEqual([]);
	});

	// Only a display with an identity slot has a manual-layout key to store a position under.
	test("only an identity-slotted display can be dragged", () => {
		const [anon, keyed] = toBoxes(
			[],
			[disp({ slot: 1 }), disp({ slot: 2, identity_slot: 3 })],
		);
		expect(anon.draggable).toBeFalsy();
		expect(keyed.draggable).toBe(true);
	});

	test("preview dims the physical monitors, never the virtual screen", () => {
		const boxes = toBoxes([mon({})], [disp({})], { dimMonitors: true });
		expect(boxes.find((b) => b.kind === "monitor")?.dimmed).toBe(true);
		expect(boxes.find((b) => b.kind === "virtual")?.dimmed).toBeFalsy();
	});

	// A disabled head is real and still shown — it is why "why isn't my monitor here?" has an
	// answer — but it is not lit.
	test("a disabled head is dimmed on its own", () => {
		expect(toBoxes([mon({ enabled: false })], [])[0].dimmed).toBe(true);
	});
});

describe("bounds", () => {
	test("spans every box, including negative origins", () => {
		const boxes = toBoxes(
			[mon({ x: -1920, mode: "1920x1080" }), mon({ connector: "DP-2" })],
			[],
		);
		expect(bounds(boxes)).toEqual({ minX: -1920, minY: 0, w: 4480, h: 1440 });
	});
});

describe("snap", () => {
	const edges = [0, 2560];
	test("a near miss lands flush against a neighbour's edge", () => {
		expect(snap(2554, 1920, edges, 16)).toBe(2560);
	});
	test("the trailing edge snaps too, so a screen sits to the LEFT flush", () => {
		expect(snap(-1914, 1920, edges, 16)).toBe(-1920);
	});
	test("outside the tolerance the drag is left where it was put", () => {
		expect(snap(2400, 1920, edges, 16)).toBe(2400);
	});
	test("the nearest edge wins when two are in range", () => {
		expect(snap(30, 100, [0, 40], 50)).toBe(40);
	});
});

describe("ghostBox", () => {
	const heads = [
		mon({ connector: "DP-1", primary: true }),
		mon({ connector: "HDMI-1", mode: "1920x1080@60", x: 2560 }),
	];
	test("extend lands beside the monitors, sized like the main one", () => {
		expect(ghostBox(heads, "extend", false)).toMatchObject({
			x: 4480,
			y: 0,
			w: 2560,
			h: 1440,
		});
	});
	test("primary and exclusive land on the main monitor", () => {
		expect(ghostBox(heads, "exclusive", false)).toMatchObject({ x: 0, y: 0 });
		expect(ghostBox(heads, "primary", false)).toMatchObject({ x: 0, y: 0 });
	});
	// A mirrored monitor IS the screen, and `auto` is the host's call: no guess is drawn.
	test("none while mirroring, and none for an unresolved auto", () => {
		expect(ghostBox(heads, "extend", true)).toBeUndefined();
		expect(ghostBox(heads, "auto", false)).toBeUndefined();
	});
	test("a headless box gets the screen on its own", () => {
		expect(ghostBox([], "exclusive", false)).toMatchObject({ x: 0, y: 0 });
	});
});

describe("toBoxes keepLit", () => {
	test("a kept monitor stays lit while the rest dim", () => {
		const boxes = toBoxes(
			[mon({ connector: "DP-1" }), mon({ connector: "HDMI-1", x: 2560 })],
			[],
			{ dimMonitors: true, keepLit: ["dp-1"] },
		);
		expect(boxes.map((b) => b.dimmed)).toEqual([false, true]);
	});
});

describe("toBoxes groups", () => {
	const at = (bs: ReturnType<typeof toBoxes>) =>
		bs.map((b) => [b.key, b.x, b.y] as const);

	test("kept screens of separate devices never stack at the origin", () => {
		const boxes = toBoxes(
			[],
			[
				disp({
					slot: 1,
					group: 0,
					x: 0,
					topology: "exclusive",
					state: "lingering",
				}),
				disp({
					slot: 2,
					group: 1,
					x: 0,
					topology: "exclusive",
					state: "active",
				}),
				disp({
					slot: 3,
					group: 2,
					x: 0,
					topology: "exclusive",
					state: "lingering",
				}),
			],
		);
		// The live one first at its spot; each kept one right of the last, past a gap.
		expect(at(boxes)).toEqual([
			["slot-2", 0, 0],
			["slot-1", 3840 + 192, 0],
			["slot-3", 2 * (3840 + 192), 0],
		]);
	});

	test("the live screen-taking group stays over the monitors", () => {
		const boxes = toBoxes(
			[mon({ primary: true })],
			[disp({ x: 0, topology: "exclusive" })],
		);
		expect(at(boxes)).toEqual([
			["mon-DP-1", 0, 0],
			["slot-1", 0, 0],
		]);
	});

	test("an extension reported over a monitor moves flush beside it", () => {
		const boxes = toBoxes([mon({})], [disp({ x: 0 })]);
		expect(at(boxes)[1]).toEqual(["slot-1", 2560, 0]);
		expect(boxes[1].shift).toEqual({ x: 2560, y: 0 });
	});

	test("a group already beside the monitors stays where it was reported", () => {
		const boxes = toBoxes([mon({})], [disp({ x: 2560 })]);
		expect(at(boxes)[1]).toEqual(["slot-1", 2560, 0]);
		expect(boxes[1].shift).toEqual({ x: 0, y: 0 });
	});
});
