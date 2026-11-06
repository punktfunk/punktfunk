// The signed-in devices of a profile on a person's own Linux account, and the Add profile refusal.
import { afterEach, expect, test } from "bun:test";
import { QueryClient } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { ApiError } from "@/api/fetcher";
import type { SignedInDevice } from "@/api/gen/model/signedInDevice";
import {
	createProfile,
	getListProfileDevicesQueryKey,
} from "@/api/gen/profiles/profiles";
import { apiErrorMessage } from "@/lib/errors";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { DevicesList, signOut } from "./devices";

overwriteGetLocale(() => baseLocale);

const ID = "5a7c9e1b3d5f";
const device = (
	kind: SignedInDevice["kind"],
	name: string,
	fingerprint = kind.repeat(64).slice(0, 64),
): SignedInDevice => ({
	fingerprint,
	name,
	kind,
	added_unix: 1_700_000_000,
	last_unix: 1_700_000_600,
});
const PHONE = device("key", "Mia's iPhone");
const DECK = device("stay", "Living room Deck");
const LAPTOP = device("for_now", "Laptop");

const realFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = realFetch;
});

/** Each call the console makes, answered with `status` and `body`. `seen` runs as it arrives. */
function answer(status: number, body?: unknown, seen = () => {}): string[] {
	const calls: string[] = [];
	globalThis.fetch = (async (
		url: string | URL | Request,
		init?: RequestInit,
	) => {
		calls.push(`${init?.method ?? "GET"} ${String(url)}`);
		seen();
		return new Response(body === undefined ? null : JSON.stringify(body), {
			status,
		});
	}) as typeof fetch;
	return calls;
}

const list = (
	devices: SignedInDevice[] | undefined,
	error: string | null = null,
) =>
	renderToStaticMarkup(
		<DevicesList
			account="mia"
			devices={devices}
			error={error}
			onSignOut={() => {}}
			onSignOutAll={() => {}}
		/>,
	);

const seeded = () => {
	const qc = new QueryClient();
	qc.setQueryData(getListProfileDevicesQueryKey(ID), [PHONE, DECK, LAPTOP]);
	return qc;
};
const cached = (qc: QueryClient) =>
	qc.getQueryData<SignedInDevice[]>(getListProfileDevicesQueryKey(ID));

test("each device says how it gets back on, and who opens the desktop", () => {
	const html = list([PHONE, DECK, LAPTOP]);
	expect(html).toContain("Unlocks with a fingerprint or face");
	expect(html).toContain("Stays signed in");
	expect(html).toContain("Signed in for now");
	expect(html).toContain("Only mia&#x27;s password or a device they signed in");
	expect(html).toContain("This machine never keeps the password.");
	expect(html.match(/>Sign out</g)).toHaveLength(3);
	expect(html).toContain("Sign out everywhere");
});

test("an empty list says so in one sentence and offers no sign-out", () => {
	const html = list([]);
	expect(html).toContain("No device is signed in.");
	expect(html).not.toContain("Sign out");
});

test("a list the host can't read shows its sentence", () => {
	const html = list(undefined, "Couldn't reach the seat supervisor.");
	expect(html).toContain("Couldn&#x27;t reach the seat supervisor.");
	expect(html).not.toContain("Sign out");
});

test("signing one device out drops its row before the host answers", async () => {
	const qc = seeded();
	let during: SignedInDevice[] | undefined;
	const calls = answer(204, undefined, () => {
		during = cached(qc);
	});
	await signOut(qc, ID, DECK.fingerprint);
	expect(during).toEqual([PHONE, LAPTOP]);
	expect(cached(qc)).toEqual([PHONE, LAPTOP]);
	expect(calls).toEqual([
		`DELETE /api/v1/profiles/${ID}/devices/${DECK.fingerprint}`,
	]);
});

test("signing out everywhere empties the list with one call", async () => {
	const calls = answer(204);
	const qc = seeded();
	await signOut(qc, ID, null);
	expect(cached(qc)).toEqual([]);
	expect(calls).toEqual([`DELETE /api/v1/profiles/${ID}/devices`]);
});

test("a 503 puts the rows back and carries the host's sentence", async () => {
	const sentence = "Couldn't reach the seat supervisor. Try again in a moment.";
	const qc = seeded();
	const during: (SignedInDevice[] | undefined)[] = [];
	answer(503, { error: sentence }, () => during.push(cached(qc)));
	const failed = await signOut(qc, ID, PHONE.fingerprint).catch((e) => e);
	expect(failed).toBeInstanceOf(ApiError);
	expect(apiErrorMessage(failed)).toBe(sentence);
	expect(cached(qc)).toEqual([PHONE, DECK, LAPTOP]);
	const all = await signOut(qc, ID, null).catch((e) => e);
	expect(apiErrorMessage(all)).toBe(sentence);
	expect(cached(qc)).toEqual([PHONE, DECK, LAPTOP]);
	expect(during).toEqual([[DECK, LAPTOP], []]);
});

test("Add profile's lockout refusal reaches the page as the host's sentence", async () => {
	const sentence = "Too many wrong passwords. Try again in 12 minutes.";
	const calls = answer(429, { error: sentence });
	const failed = await createProfile({
		display_name: "Mia",
		seat: true,
		seat_account: "mia",
		seat_password: "typed",
	}).catch((e) => e);
	expect(failed).toBeInstanceOf(ApiError);
	expect((failed as ApiError).status).toBe(429);
	expect(apiErrorMessage(failed)).toBe(sentence);
	expect(calls).toEqual(["POST /api/v1/profiles"]);
});
