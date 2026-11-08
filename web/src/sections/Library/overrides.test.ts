import { expect, test } from "bun:test";
import {
	applyPatch,
	gameEntry,
	storeOf,
	streamSummary,
	usageOf,
} from "./overrides";

test("the badge names the shared override, else sums up the values", () => {
	expect(streamSummary({ name: "4K", lines: 2160, hz: 60 })).toBe("4K");
	expect(streamSummary({ lines: 1080, hz: 60 })).toBe("1080p60");
	expect(streamSummary({ lines: 2160 })).toBe("4K");
	expect(streamSummary({ hz: 60 })).toBe("60 Hz");
	expect(streamSummary({ lines: 1440, hdr: false, gamepad: "dualsense" })).toBe(
		"1440p · HDR off · DualSense",
	);
	expect(streamSummary({ hdr: true, gamepad: "newpad" })).toBe(
		"HDR on · newpad",
	);
	expect(streamSummary({})).toBeNull();
	expect(streamSummary(null)).toBeNull();
});

test("a field edit writes the whole entry, and clearing the height drops the screen option", () => {
	const base = { lines: 1080, beyond_screen: true, hz: 60 };
	expect(applyPatch(base, { hz: null })).toEqual({
		lines: 1080,
		beyond_screen: true,
	});
	expect(applyPatch(base, { lines: null })).toEqual({ hz: 60 });
	expect(applyPatch({}, { beyond_screen: true })).toEqual({});
	expect(applyPatch({ hdr: true }, { hdr: false })).toEqual({ hdr: false });
});

test("a game under a source's entry writes none for nothing, so it skips the source", () => {
	const doc = { sources: { lutris: { gamepad: "xbox360" } } };
	expect(gameEntry(doc, "lutris:4", {})).toEqual({ none: true });
	expect(gameEntry(doc, "lutris:4", { hz: 60 })).toEqual({ hz: 60 });
	expect(gameEntry(doc, "lutris:4", { set: "a" })).toEqual({ set: "a" });
	expect(gameEntry(doc, "steam:70", {})).toEqual({});
	expect(gameEntry(undefined, "lutris:4", {})).toEqual({});
});

test("Same as the source clears the game's entry, where Device decides opts out", () => {
	const doc = { sources: { lutris: { gamepad: "xbox360" } } };
	expect(gameEntry(doc, "lutris:4", null)).toEqual({});
	expect(gameEntry(doc, "lutris:4", {})).toEqual({ none: true });
});

test("usage counts the games and sources naming a set", () => {
	const doc = {
		games: { "steam:1": { set: "a" }, "steam:2": { set: "b" }, "steam:3": {} },
		sources: { lutris: { set: "a" } },
	};
	expect(usageOf(doc, "a")).toEqual({ games: 1, sources: 1 });
	expect(usageOf(doc, "c")).toEqual({ games: 0, sources: 0 });
	expect(usageOf(undefined, "a")).toEqual({ games: 0, sources: 0 });
});

test("a source is the id up to its first colon", () => {
	expect(storeOf("heroic:legendary:x")).toBe("heroic");
	expect(storeOf("bare")).toBe("bare");
});
