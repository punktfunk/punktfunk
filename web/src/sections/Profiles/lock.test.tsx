// A profile's lock: whose lock a card shows, what it shows of a read, and the calls a lock makes.
import { afterEach, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { ApiError } from "@/api/fetcher";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import type { ProfileLock } from "@/api/gen/model/profileLock";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import {
	LockAction,
	LockChip,
	LockLines,
	lockCard,
	lockHolder,
	lockWith,
	signOutAsk,
	validPin,
} from "./lock";

overwriteGetLocale(() => baseLocale);

const profile = (
	id: string,
	more: Partial<ProfileAdmin> = {},
): ProfileAdmin => ({
	id,
	display_name: id,
	owner: false,
	home: "desktop",
	last_used_unix: 0,
	default: false,
	...more,
});
const OWNER = profile("enrico", { owner: true });
const LOCKED: ProfileLock = {
	locked: true,
	pin: false,
	failed_pins: 3,
	playing: [],
};
const coded = (status: number, code: string) =>
	new ApiError(status, { error: "The host's sentence.", code });

test("a sharer opens with the owner's lock, a desktop of its own with its own", () => {
	expect(lockHolder(OWNER, OWNER, false)).toEqual({
		id: "enrico",
		shared: false,
	});
	expect(lockHolder(profile("anna"), OWNER, false)).toEqual({
		id: "enrico",
		shared: true,
	});
	const doorSharer = profile("kid", {
		seat: { kind: "shared", state: "stopped", port: 1 },
	});
	expect(lockHolder(doorSharer, OWNER, false)?.id).toBe("enrico");
	const seat = profile("leon", {
		seat: { kind: "desktop", state: "ready", port: 1 },
	});
	expect(lockHolder(seat, OWNER, false)).toEqual({ id: "leon", shared: false });
	const turns = profile("mia", {
		seat: { kind: "desktop", state: "ready", port: 1, take_turns: true },
	});
	expect(lockHolder(turns, OWNER, false)?.id).toBe("mia");
	expect(lockHolder(seat, OWNER, true)).toBeNull();
});

test("a PIN is 4 to 8 ASCII digits", () => {
	expect(["1234", "12345678"].every(validPin)).toBe(true);
	expect(["123", "123456789", "12a4", "１２３４", ""].some(validPin)).toBe(
		false,
	);
});

test("a card shows the lock it reads, and nothing where the host has none", () => {
	expect(lockCard(undefined, { error: null })).toBeUndefined();
	expect(lockCard(undefined, { data: LOCKED, error: null })).toEqual({
		state: "locked",
		owner: undefined,
		failedPins: 3,
	});
	expect(lockCard("Enrico", { data: LOCKED, error: null })).toEqual({
		state: "locked",
		owner: "Enrico",
		failedPins: undefined,
	});
	const unread = coded(409, "profile_locks_unreadable");
	expect(lockCard(undefined, { error: unread })?.state).toBe("unreadable");
	expect(lockCard("Enrico", { error: unread })?.state).toBe("unreadable");
	for (const quiet of [
		coded(409, "profile_lock_own_account"),
		coded(409, "profile_lock_shared"),
		new ApiError(404, null),
	])
		expect(lockCard(undefined, { error: quiet })).toBeUndefined();
	const down = new ApiError(503, { error: "The host didn't answer." });
	expect(lockCard(undefined, { error: down })).toEqual({
		state: "failed",
		error: "The host didn't answer.",
	});
	expect(lockCard("Enrico", { error: down })).toBeUndefined();
});

test("an open card has one action, a locked one a status and its wrong PINs", () => {
	const open = renderToStaticMarkup(
		<LockAction lock={{ state: "open", onOpen: () => {} }} />,
	);
	expect(open).toContain('aria-label="Lock profile"');
	const sharer = renderToStaticMarkup(
		<LockAction lock={{ state: "open", owner: "Enrico", onOpen: () => {} }} />,
	);
	expect(sharer).toContain("Lock Enrico&#x27;s desktop");
	const locked = { state: "locked" as const, failedPins: 1, onOpen: () => {} };
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
	const unreadable = renderToStaticMarkup(
		<LockChip lock={{ state: "unreadable" }} />,
	);
	expect(unreadable).toContain("Locked");
	expect(unreadable).not.toContain("<button");
});

const realFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = realFetch;
});

/** Each call, answered by `reply(method, url)`. */
function answer(reply: (call: string) => [number, unknown?]): string[] {
	const calls: string[] = [];
	globalThis.fetch = (async (
		url: string | URL | Request,
		init?: RequestInit,
	) => {
		const call = `${init?.method ?? "GET"} ${String(url)}`;
		calls.push(call);
		const [status, body] = reply(call);
		return new Response(body === undefined ? null : JSON.stringify(body), {
			status,
		});
	}) as typeof fetch;
	return calls;
}

const OLD = "a".repeat(64);
const KEPT = "b".repeat(64);
const NEW = "c".repeat(64);

test("a lock sets the PIN, drops what it doesn't keep, then locks with the rest", async () => {
	const calls = answer(() => [200, LOCKED]);
	const seen: ProfileLock[] = [];
	await lockWith(
		"kid",
		[OLD, KEPT],
		{ devices: [KEPT, NEW], pin: "2468", password: "pw" },
		(l) => seen.push(l),
	);
	expect(calls).toEqual([
		"PUT /api/v1/profiles/kid/lock/pin",
		`DELETE /api/v1/profiles/kid/devices/${OLD}`,
		"POST /api/v1/profiles/kid/lock",
	]);
	expect(seen).toHaveLength(2);
});

test("a refused password changes nothing", async () => {
	const calls = answer(() => [401, { error: "Wrong password." }]);
	const failed = await lockWith(
		"kid",
		[OLD],
		{ devices: [], pin: "2468", password: "nope" },
		() => {},
	).catch((e) => e);
	expect((failed as ApiError).status).toBe(401);
	expect(calls).toEqual(["PUT /api/v1/profiles/kid/lock/pin"]);
});

test("a lock without a PIN never asks for the password", async () => {
	const calls = answer(() => [200, LOCKED]);
	await lockWith(
		"kid",
		[],
		{ devices: [KEPT], pin: "", password: "" },
		() => {},
	);
	expect(calls).toEqual(["POST /api/v1/profiles/kid/lock"]);
});

test("a sign-out says how the device gets back in, PIN or not", () => {
	const deck = {
		fingerprint: KEPT,
		name: "Deck",
		kind: "stay" as const,
		added_unix: 0,
		last_unix: 0,
	};
	const one = signOutAsk("Kid", false, deck);
	expect(one.title).toBe("Sign Deck out?");
	expect(one.description).toContain("until you add it back.");
	expect(signOutAsk("Kid", true, deck).description).toContain("types the PIN");
	const all = signOutAsk("Kid", true, null);
	expect(all.confirmLabel).toBe("Sign out everywhere");
	expect(all.description).toContain("only with the PIN");
	expect(all.destructive).toBe(true);
});
