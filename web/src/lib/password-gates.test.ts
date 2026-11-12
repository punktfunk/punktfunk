import { describe, expect, test } from "bun:test";
import { touchesPlane, widensAccess } from "./password-gates";

const NOW = 1_000_000;
const guest = {
	grants: 0x01,
	expires_unix: NOW + 3600,
	until_disconnect: false,
};
const full = { grants: null, expires_unix: null, until_disconnect: false };

describe("widensAccess", () => {
	test.each([
		["a grant it lacks", guest, { grants: 0x21 }],
		["the screen choice", { ...full, grants: 0xff }, { grants: 0x1ff }],
		["a later deadline", guest, { expires_in_secs: 7200 }],
		["no deadline", guest, { clear_expiry: true }],
		[
			"a lapsed guest any time back",
			{ ...guest, expires_unix: NOW - 1 },
			{ expires_in_secs: 60 },
		],
		[
			"past its last session",
			{ ...guest, until_disconnect: true },
			{ until_disconnect: false },
		],
		["a device the host can't name", undefined, {}],
		["a mask of the wrong type", guest, { grants: "255" }],
		["a time of the wrong type", guest, { expires_in_secs: 1.5 }],
		["a switch of the wrong type", guest, { clear_expiry: "yes" }],
	])("widens: %s", (_, stored, patch) => {
		expect(widensAccess(stored, patch, NOW)).toBe(true);
	});

	test.each([
		["fewer grants", full, { grants: 0x01 }],
		["every grant on a full device", full, { grants: 0x1ff }],
		["the same grants", guest, { grants: 0x01 }],
		["an earlier deadline", guest, { expires_in_secs: 60 }],
		["expire now", guest, { expires_in_secs: 0 }],
		["a deadline on a permanent device", full, { expires_in_secs: 3600 }],
		["clearing no deadline", full, { clear_expiry: true }],
		["ending with its session", guest, { until_disconnect: true }],
		["nothing", guest, {}],
	])("narrows: %s", (_, stored, patch) => {
		expect(widensAccess(stored, patch, NOW)).toBe(false);
	});
});

test("a plane setting is touched even when reset", () => {
	expect(touchesPlane({ clipboard: null })).toBe(true);
	expect(touchesPlane({ max_fps: 60 })).toBe(false);
});
