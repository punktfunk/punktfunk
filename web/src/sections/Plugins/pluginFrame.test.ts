import { expect, test } from "bun:test";
import {
	frameAction,
	healthFailure,
	healthIntervalMs,
	type PluginBody,
	type PluginOrigin,
	pluginBody,
} from "./pluginFrame";

const settled = { isError: false, isPending: false };
const probing = { isError: false, isPending: true };
const probeFailed = { isError: true, isPending: false };
const healthy = { isError: false, isSuccess: true };
const asking = { isError: false, isSuccess: false };
const down = { isError: true, isSuccess: false };
const origin = "https://host:47993";

const bodyCases: [
	string,
	PluginOrigin,
	typeof settled,
	typeof healthy,
	PluginBody,
][] = [
	["no plugin listener", null, settled, healthy, "unavailable"],
	["no listener beats a failed probe", null, probeFailed, down, "unavailable"],
	[
		"certificate not trusted for the port",
		origin,
		probeFailed,
		healthy,
		"untrusted",
	],
	["untrusted beats offline", origin, probeFailed, down, "untrusted"],
	["plugin down", origin, settled, down, "offline"],
	["plugin down, probe still running", origin, probing, down, "offline"],
	[
		"plugin down before the config answers",
		undefined,
		probing,
		down,
		"offline",
	],
	["healthy and reachable", origin, settled, healthy, "frame"],
	["healthy, origin probe still running", origin, probing, healthy, "probing"],
	["healthy, origin unknown yet", undefined, probing, healthy, "probing"],
	["health not answered yet", origin, settled, asking, "probing"],
	// `vite dev` serves plugins from the console's origin and never runs the probe.
	["same-origin dev server", "", probing, healthy, "frame"],
];
for (const [name, o, reachable, health, want] of bodyCases) {
	test(`body: ${name}`, () => {
		expect(pluginBody(o, reachable, health)).toBe(want);
	});
}

test("a health answer fails on a redirect or a non-2xx status", () => {
	expect(healthFailure({ type: "basic", ok: true, status: 200 })).toBeNull();
	expect(healthFailure({ type: "basic", ok: false, status: 502 })).toBe(
		"health 502",
	);
	// A manual redirect reads as ok=false, status 0; the redirect is the cause.
	expect(healthFailure({ type: "opaqueredirect", ok: false, status: 0 })).toBe(
		"session expired",
	);
	expect(healthFailure({ type: "opaqueredirect", ok: true, status: 200 })).toBe(
		"session expired",
	);
});

test("a plugin that is down is probed every five seconds, a live one every twenty", () => {
	expect(healthIntervalMs("error")).toBe(5_000);
	expect(healthIntervalMs("success")).toBe(20_000);
	expect(healthIntervalMs("pending")).toBe(20_000);
});

const act = (data: unknown, fromFrame = true) => frameAction(fromFrame, data);

test("the frame asks for the palette", () => {
	expect(act({ type: "pf-ui:theme-request" })).toEqual({ kind: "theme" });
});

test("the frame moves the address bar, minus one leading slash", () => {
	const nav = (path: unknown) => act({ type: "pf-ui:navigate", path });
	expect(nav("/settings/audio")).toEqual({
		kind: "navigate",
		splat: "settings/audio",
	});
	expect(nav("settings")).toEqual({ kind: "navigate", splat: "settings" });
	expect(nav("/")).toEqual({ kind: "navigate", splat: "" });
	expect(nav("")).toEqual({ kind: "navigate", splat: "" });
	// pins current behaviour: only the first slash is stripped.
	expect(nav("//a")).toEqual({ kind: "navigate", splat: "/a" });
	expect(nav(5)).toBeNull();
	expect(nav(undefined)).toBeNull();
});

test("messages from another window are ignored", () => {
	expect(act({ type: "pf-ui:theme-request" }, false)).toBeNull();
	expect(act({ type: "pf-ui:navigate", path: "/x" }, false)).toBeNull();
});

test("anything that is not a known request is ignored", () => {
	expect(act({ type: "pf-ui:other" })).toBeNull();
	expect(act({})).toBeNull();
	expect(act(null)).toBeNull();
	expect(act(undefined)).toBeNull();
	expect(act("pf-ui:theme-request")).toBeNull();
	expect(act(42)).toBeNull();
});
