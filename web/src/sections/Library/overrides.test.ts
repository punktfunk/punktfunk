import { expect, test } from "bun:test";
import {
	applyPatch,
	gameEntry,
	HZ_RANGE,
	LINES_RANGE,
	profileEntry,
	profileStream,
	setList,
	storeOf,
	streamSummary,
	usageOf,
	wholeIn,
} from "./overrides";

test("a typed custom value is a whole number in the host's range", () => {
	expect(wholeIn("1200", LINES_RANGE)).toBe(1200);
	expect(wholeIn("75.4", HZ_RANGE)).toBe(75);
	expect(wholeIn("5000", LINES_RANGE)).toBe(4320);
	expect(wholeIn("10", HZ_RANGE)).toBe(24);
	expect(wholeIn("", HZ_RANGE)).toBeNull();
	expect(wholeIn(" ", HZ_RANGE)).toBeNull();
	expect(wholeIn("abc", LINES_RANGE)).toBeNull();
});

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

test("shared overrides sort by name in the UI language's order", () => {
	const doc = { sets: { a: { name: "Zed" }, b: { name: "Ärlig" } } };
	const ids = (locale: string) => setList(doc, locale).map((s) => s.id);
	// German files Ä beside A; Swedish files it after Z.
	expect(ids("de")).toEqual(["b", "a"]);
	expect(ids("sv")).toEqual(["a", "b"]);
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

const owner = {
	sets: { a1b2c3d4e5f6: { name: "1080p60", lines: 1080, hz: 60 } },
	games: { "steam:1": { set: "a1b2c3d4e5f6" } },
	sources: { lutris: { gamepad: "xbox360" } },
};

test("with nothing of its own a profile sees the owner's entry, a game's then its source's", () => {
	const game = profileStream("game", "steam:1", owner, {});
	expect(game.mine).toBeUndefined();
	expect(game.fallbackIsOwn).toBe(false);
	expect(game.setId).toBe("a1b2c3d4e5f6");
	expect(game.shown).toEqual({ lines: 1080, hz: 60 });
	expect(profileStream("game", "lutris:7", owner, undefined).shown).toEqual({
		gamepad: "xbox360",
	});
	expect(profileStream("game", "steam:9", owner, {}).shown).toEqual({});
});

test("a profile's own entry wins, and may name the owner's shared override", () => {
	const own = {
		games: { "steam:1": { none: true }, "steam:2": { set: "a1b2c3d4e5f6" } },
		sources: { lutris: { hz: 30 } },
	};
	expect(profileStream("game", "steam:1", owner, own)).toEqual({
		mine: { none: true },
		fallback: { set: "a1b2c3d4e5f6" },
		fallbackIsOwn: false,
		shown: {},
	});
	expect(profileStream("game", "steam:2", owner, own).shown).toEqual({
		lines: 1080,
		hz: 60,
	});
	// The profile's source entry beats the owner's, for the game and for the source.
	const viaSource = profileStream("game", "lutris:7", owner, own);
	expect(viaSource.mine).toBeUndefined();
	expect(viaSource.fallbackIsOwn).toBe(true);
	expect(viaSource.shown).toEqual({ hz: 30 });
	expect(profileStream("source", "lutris", owner, own).mine).toEqual({
		hz: 30,
	});
});

test("a profile's game with nothing of its own stores none, and null hands it back", () => {
	expect(profileEntry("game", {})).toEqual({ none: true });
	expect(profileEntry("game", { hz: 60 })).toEqual({ hz: 60 });
	expect(profileEntry("game", { set: "a" })).toEqual({ set: "a" });
	expect(profileEntry("game", null)).toEqual({});
	expect(profileEntry("source", {})).toEqual({});
	expect(profileEntry("source", null)).toEqual({});
});

test("a source is the id up to its first colon", () => {
	expect(storeOf("heroic:legendary:x")).toBe("heroic");
	expect(storeOf("bare")).toBe("bare");
});
