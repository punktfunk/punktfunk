import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
	GRANT_ALL,
	GRANT_CLIPBOARD,
	GRANT_GAMEPAD,
	GRANT_KEYBOARD,
	GRANT_LAUNCH,
	GRANT_MANAGE_GAMES,
	GRANT_MIC,
	GRANT_POINTER,
	GRANT_POWER,
	grantFields,
	levelOfMask,
	normalizeLegacyFull,
} from "./access";

// Written by punktfunk-core's grant_vectors_are_checked_in; Kotlin replays the same file.
const vectors = JSON.parse(
	readFileSync(
		join(
			import.meta.dir,
			"../../../../crates/core/punktfunk-core/testdata/grant-vectors.json",
		),
		"utf8",
	),
) as {
	bits: Record<string, number>;
	all: number;
	masks: { mask: number; normalized: number; level: string }[];
};

test("grant bits match core", () => {
	expect({
		GAMEPAD: GRANT_GAMEPAD,
		POINTER: GRANT_POINTER,
		KEYBOARD: GRANT_KEYBOARD,
		CLIPBOARD: GRANT_CLIPBOARD,
		MIC: GRANT_MIC,
		LAUNCH: GRANT_LAUNCH,
		POWER: GRANT_POWER,
		MANAGE_GAMES: GRANT_MANAGE_GAMES,
	}).toEqual(vectors.bits);
	expect(GRANT_ALL).toBe(vectors.all);
});

for (const c of vectors.masks) {
	test(`mask ${c.mask} reads as ${c.level}`, () => {
		expect(normalizeLegacyFull(c.mask)).toBe(c.normalized);
		expect(levelOfMask(c.mask)).toBe(c.level);
	});
}

test("grantFields omits the untouched default unless asked to be explicit", () => {
	const full = {
		grants: GRANT_ALL,
		expiry: "forever" as const,
		customHours: 4,
	};
	expect(grantFields(full)).toEqual({});
	expect(grantFields(full, true)).toEqual({
		grants: GRANT_ALL,
		until_disconnect: false,
	});
	expect(
		grantFields({ grants: GRANT_GAMEPAD, expiry: "4h", customHours: 4 }),
	).toEqual({
		grants: GRANT_GAMEPAD,
		expires_in_secs: 4 * 3600,
		until_disconnect: false,
	});
	expect(grantFields({ ...full, expiry: "session" })).toEqual({
		grants: GRANT_ALL,
		until_disconnect: true,
	});
});
