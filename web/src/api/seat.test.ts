import { afterEach, expect, test } from "bun:test";
import { MutationObserver, QueryClient } from "@tanstack/react-query";
import type { CatalogEntry } from "@/api/gen/model";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { apiFetch } from "./fetcher";
import { isFullSeat, scopeChoices, seatChoices, seatClient } from "./seat";
import { installable } from "./store";

const KID = "0a1b2c3d4e5f";
const realFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = realFetch;
});

/** Every URL the console asks for, answered with an empty list. */
function recordFetch(): string[] {
	const urls: string[] = [];
	globalThis.fetch = (async (url: string | URL | Request) => {
		urls.push(String(url));
		return new Response("[]");
	}) as typeof fetch;
	return urls;
}

const profile = (over: Partial<ProfileAdmin>): ProfileAdmin =>
	({
		id: KID,
		display_name: "Kid",
		owner: false,
		default: false,
		home: "own",
		last_used_unix: 0,
		...over,
	}) as ProfileAdmin;

const seat = {
	port: 9778,
	state: "ready",
	kind: "desktop",
} as ProfileAdmin["seat"];

test("a Windows host's profiles with a seat have a library of their own", () => {
	const owner = profile({ id: "o".repeat(32), owner: true });
	const shared = profile({ id: "s".repeat(32), display_name: "Guest" });
	const kid = profile({ seat });
	expect(seatChoices([owner, shared, kid], "windows")).toEqual([
		{ id: KID, name: "Kid" },
	]);
	expect(isFullSeat(profile({ seat, owner: true }), "windows")).toBe(false);
	// A Linux host's seat shares the box's host until it has its own.
	expect(seatChoices([owner, kid], "linux")).toEqual([]);
	expect(seatChoices(undefined, undefined)).toEqual([]);
});

test("on a door the owner and the seats of their own have a library, the rest share the owner's", () => {
	const owner = profile({
		id: "o".repeat(32),
		owner: true,
		display_name: "Enrico",
	});
	const shared = profile({
		id: "s".repeat(32),
		display_name: "Guest",
		seat: { port: 9778, state: "ready", kind: "shared" },
	});
	const desktop = profile({
		seat: { port: 9779, state: "ready", kind: "desktop" },
	});
	expect(seatChoices([owner, shared, desktop], "linux", true)).toEqual([
		{ id: "o".repeat(32), name: "Enrico" },
		{ id: KID, name: "Kid" },
	]);
	// Off a door the owner is the box, and the same list offers nothing on Linux.
	expect(seatChoices([owner, shared, desktop], "linux")).toEqual([]);
});

test("a seat on a person's own account is marked with its host's platform", () => {
	const owner = profile({ id: "o".repeat(32), owner: true });
	const made = profile({ id: "m".repeat(32), display_name: "Made", seat });
	const mia = profile({ seat: { ...seat, account: "mia" } as typeof seat });
	expect(seatChoices([owner, made, mia], "linux", true)).toEqual([
		{ id: "o".repeat(32), name: "Kid" },
		{ id: "m".repeat(32), name: "Made" },
		{ id: KID, name: "Kid", own: "linux" },
	]);
	expect(seatChoices([made, mia], "windows")).toEqual([
		{ id: "m".repeat(32), name: "Made" },
		{ id: KID, name: "Kid", own: "windows" },
	]);
});

const OWNER = "4f1c3a9b0e27";
const owner = profile({ id: OWNER, owner: true, display_name: "Enrico" });
const sharer = profile({ id: "0b7d5e2a91c4", display_name: "Anna" });
const turns = profile({
	id: "1c2d3e4f5a6b",
	display_name: "Tom",
	seat: { ...seat, take_turns: true } as typeof seat,
});
const own = profile({
	id: "3d5f7b9a1c3e",
	display_name: "Oma",
	seat: { ...seat, account: "oma" } as typeof seat,
});
const made = profile({ display_name: "Kid", seat });

test("the library offers the owner and then every profile, each seen as itself", () => {
	const all = [owner, sharer, turns, made, own];
	expect(scopeChoices("library", all, "windows")).toEqual([
		{ id: null, name: "Enrico", seat: null },
		{ id: sharer.id, name: "Anna", seat: null, as: sharer.id },
		{ id: turns.id, name: "Tom", seat: null, as: turns.id },
		{ id: KID, name: "Kid", seat: null, as: KID },
		{ id: own.id, name: "Oma", seat: null, as: own.id },
	]);
	// A door has no library of its own: every view is read from the owner's host.
	const door = scopeChoices("library", [owner, sharer], "linux", true);
	const ownerSeat = { id: OWNER, name: "Enrico" };
	expect(door).toEqual([
		{ id: null, name: "Enrico", seat: ownerSeat },
		{ id: sharer.id, name: "Anna", seat: ownerSeat, as: sharer.id },
	]);
	// One person: one choice, so the title stays plain.
	expect(scopeChoices("library", [owner], "linux")).toHaveLength(1);
	expect(scopeChoices("library", undefined, undefined)).toHaveLength(1);
});

test("plugins and automation offer the owner and the seats with a host of their own", () => {
	const all = [owner, sharer, turns, made, own];
	const seats = [
		{ id: null, name: "Enrico", seat: null },
		{ id: KID, name: "Kid", seat: { id: KID, name: "Kid" } },
		{
			id: own.id,
			name: "Oma",
			seat: { id: own.id, name: "Oma", own: "windows" },
		},
	];
	expect(scopeChoices("plugins", all, "windows")).toEqual(seats);
	expect(scopeChoices("automation", all, "windows")).toEqual(seats);
	// On a door the owner's plugins are on its seat; its hooks the door forwards itself.
	const desk = profile({
		id: "7e1f3b5d9a2c",
		display_name: "Leon",
		seat: { port: 9779, state: "ready", kind: "desktop" },
	});
	const door = [{ ...owner, seat: { ...seat, kind: "shared" } }, desk];
	expect(scopeChoices("plugins", door, "linux", true)[0]?.seat).toEqual({
		id: OWNER,
		name: "Enrico",
	});
	expect(scopeChoices("automation", door, "linux", true)[0]?.seat).toBeNull();
	expect(scopeChoices("automation", door, "linux", true)).toHaveLength(2);
});

const entry = (id: string, tier: string): CatalogEntry =>
	({ id, tier, source: "unom", pkg: id }) as CatalogEntry;

test("a seat on a person's own account offers only the plugins its host installs", () => {
	const all = [entry("romm", "verified"), entry("theirs", "external")];
	expect(installable(all, null)).toEqual(all);
	expect(installable(all, "windows").map((e) => e.id)).toEqual(["romm"]);
	expect(installable(all, "linux")).toEqual([]);
	expect(installable(undefined, null)).toEqual([]);
});

test("a seat's cache asks the seat's host and the box's never does", async () => {
	const urls = recordFetch();
	const root = new QueryClient();
	const kid = seatClient(KID, root);
	const list = (c: QueryClient) =>
		c.fetchQuery({
			queryKey: ["/api/v1/library"],
			queryFn: () => apiFetch("/api/v1/library"),
		});
	await Promise.all([list(root), list(kid)]);
	expect(urls.sort()).toEqual([
		"/api/v1/library",
		`/api/v1/profiles/${KID}/proxy/library`,
	]);
	// The same key holds one entry in each cache, so neither answers for the other.
	expect(root.getQueryCache().getAll()).toHaveLength(1);
	expect(kid.getQueryCache().getAll()).toHaveLength(1);
	// The call after a seat's is the box's again.
	await apiFetch("/api/v1/library");
	expect(urls.at(-1)).toBe("/api/v1/library");
});

test("a seat's writes go to the seat's host", async () => {
	const urls = recordFetch();
	const kid = seatClient(KID, new QueryClient());
	await new MutationObserver(kid, {
		mutationFn: () => apiFetch("/api/v1/library/custom", { method: "POST" }),
	}).mutate();
	expect(urls).toEqual([`/api/v1/profiles/${KID}/proxy/library/custom`]);
});

test("a seat's cache is kept and takes the box's defaults", () => {
	const root = new QueryClient({
		defaultOptions: { queries: { staleTime: 7 } },
	});
	const kid = seatClient("a".repeat(32), root);
	expect(seatClient("a".repeat(32), root)).toBe(kid);
	expect(kid).not.toBe(root);
	expect(kid.getDefaultOptions().queries?.staleTime).toBe(7);
});
