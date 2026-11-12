// A profile's lock: what a card shows from the profile list, and the one call a lock makes.
import { afterEach, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { LockSummary } from "@/api/gen/model/lockSummary";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileLock } from "@/api/gen/model/profileLock";
import type { SignedInDevice } from "@/api/gen/model/signedInDevice";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { DevicesList } from "./devices";
import {
	LockAction,
	LockChip,
	LockLines,
	lockCard,
	lockWith,
	signOutAsk,
	validPin,
} from "./lock";

overwriteGetLocale(() => baseLocale);

const profile = (
	id: string,
	lock?: LockSummary,
	more: Partial<ProfileAdmin> = {},
): ProfileAdmin => ({
	id,
	display_name: id.charAt(0).toUpperCase() + id.slice(1),
	owner: false,
	home: "desktop",
	last_used_unix: 0,
	default: false,
	lock,
	...more,
});
const LOCKED: LockSummary = { locked: true, pin: false, failed_pins: 3 };
const OWNER = profile("enrico", LOCKED, { owner: true });
const SHARER = profile("anna", { ...LOCKED, shared_with: "enrico" });
const OWN_ACCOUNT = profile("oma");

test("a card reads its lock from the list, and a sharer names the owner's", () => {
	const list = [OWNER, SHARER, OWN_ACCOUNT];
	expect(lockCard(OWNER, list)).toEqual({
		locked: true,
		holder: "enrico",
		failedPins: 3,
	});
	expect(lockCard(SHARER, list)).toEqual({
		locked: true,
		holder: "enrico",
		owner: "Enrico",
	});
	expect(lockCard(OWN_ACCOUNT, list)).toBeUndefined();
});

test("a PIN is 4 to 8 ASCII digits", () => {
	expect(["1234", "12345678"].every(validPin)).toBe(true);
	expect(["123", "123456789", "12a4", "１２３４", ""].some(validPin)).toBe(
		false,
	);
});

test("an open card has one action, a locked one a status and its wrong PINs", () => {
	const open = { locked: false, holder: "kid", onOpen: () => {} };
	expect(renderToStaticMarkup(<LockAction lock={open} />)).toContain(
		'aria-label="Lock profile"',
	);
	expect(
		renderToStaticMarkup(<LockAction lock={{ ...open, owner: "Enrico" }} />),
	).toContain("Lock Enrico&#x27;s desktop");
	expect(renderToStaticMarkup(<LockChip lock={open} />)).toBe("");
	const locked = { ...open, locked: true, failedPins: 1 };
	expect(renderToStaticMarkup(<LockAction lock={locked} />)).toBe("");
	expect(renderToStaticMarkup(<LockChip lock={locked} />)).toContain("<button");
	expect(renderToStaticMarkup(<LockLines lock={locked} />)).toContain(
		"1 wrong PIN in the last 15 minutes",
	);
	expect(
		renderToStaticMarkup(<LockLines lock={{ ...locked, failedPins: 3 }} />),
	).toContain("3 wrong PINs in the last 15 minutes");
	expect(
		renderToStaticMarkup(<LockLines lock={{ ...locked, owner: "Enrico" }} />),
	).toContain("Opens with Enrico&#x27;s lock.");
});

const realFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = realFetch;
});

/** Each call with its body, answered with `status`. */
function answer(status: number, body: unknown): [string, unknown][] {
	const calls: [string, unknown][] = [];
	globalThis.fetch = (async (
		url: string | URL | Request,
		init?: RequestInit,
	) => {
		const sent = typeof init?.body === "string" ? JSON.parse(init.body) : null;
		calls.push([`${init?.method ?? "GET"} ${String(url)}`, sent]);
		return new Response(JSON.stringify(body), { status });
	}) as typeof fetch;
	return calls;
}

const DECK = "b".repeat(64);
const ANSWER: ProfileLock = { ...LOCKED, playing: [] };

test("a lock is one call, and only a PIN brings the password along", async () => {
	const plain = answer(200, ANSWER);
	await lockWith("kid", { devices: [DECK], pin: "", password: "" });
	expect(plain).toEqual([
		["POST /api/v1/profiles/kid/lock", { devices: [DECK] }],
	]);
	const pinned = answer(200, ANSWER);
	await lockWith("kid", { devices: [], pin: "2468", password: "pw" });
	expect(pinned).toEqual([
		[
			"POST /api/v1/profiles/kid/lock",
			{ devices: [], pin: "2468", password: "pw" },
		],
	]);
});

const device = (more: Partial<SignedInDevice>): SignedInDevice => ({
	fingerprint: DECK,
	name: "Deck",
	kind: "stay",
	added_unix: 0,
	last_unix: 0,
	moonlight: false,
	...more,
});

test("a locked profile's list says who added a device and tags Moonlight", () => {
	const html = renderToStaticMarkup(
		<DevicesList
			flush
			devices={[
				device({ kind: "added" }),
				device({
					fingerprint: "c".repeat(64),
					name: "Shield",
					moonlight: true,
				}),
			]}
			error={null}
			onSignOut={() => {}}
			onSignOutAll={() => {}}
		/>,
	);
	expect(html).toContain("Added by you");
	expect(html).toContain("Stays signed in · Moonlight");
});

test("a sign-out says how the device gets back in, PIN or not", () => {
	const deck = device({});
	const one = signOutAsk("Kid", false, deck);
	expect(one.title).toBe("Sign Deck out?");
	expect(one.description).toContain("until you add it back.");
	expect(signOutAsk("Kid", true, deck).description).toContain("types the PIN");
	const all = signOutAsk("Kid", true, null);
	expect(all.confirmLabel).toBe("Sign out everywhere");
	expect(all.description).toContain("only with the PIN");
	expect(all.destructive).toBe(true);
});
