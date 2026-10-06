import { afterEach, expect, test } from "bun:test";
import { MutationObserver, QueryClient } from "@tanstack/react-query";
import type { ProfileAdmin } from "@/api/gen/model/profileAdmin";
import { apiFetch } from "./fetcher";
import { isFullSeat, seatChoices, seatClient } from "./seat";

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

const seat = { port: 9778, state: "ready" } as ProfileAdmin["seat"];

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
