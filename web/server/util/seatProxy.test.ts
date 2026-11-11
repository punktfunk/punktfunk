import { describe, expect, test } from "bun:test";
import { encodeRest, isPluginPrivate, seatCall, seatPath } from "./seatProxy";

const ID = "0a1b2c3d4e5f";
const base = `/api/v1/profiles/${ID}/proxy`;

describe("seatCall", () => {
	test("names the profile and the path on its host", () => {
		expect(seatCall(`${base}/library/page`)).toEqual({
			id: ID,
			rest: "library/page",
		});
	});

	test("resolves dots and escapes, so the gate reads what the host reads", () => {
		expect(seatCall(`${base}/library/./custom`)?.rest).toBe("library/custom");
		expect(seatCall(`${base}/x/../store/install`)?.rest).toBe("store/install");
		expect(seatCall(`${base}/%2e%2e/store/install`)).toBeNull();
	});

	test("refuses a path that is not a seat's", () => {
		expect(seatCall(`/api/v1/profiles/${ID}/proxy`)).toBeNull();
		expect(seatCall(`/api/v1/profiles/${ID}/proxy/`)).toBeNull();
		expect(seatCall(`/api/v1/profiles/bad.id/proxy/library`)).toBeNull();
		expect(seatCall(`${base}/../../library`)).toBeNull();
	});

	test("refuses a segment that hides a slash, so no dot segment reaches the host", () => {
		expect(
			seatCall(`${base}/x%2F..%2F..%2F..%2F..%2Fupdate%2Fapply`),
		).toBeNull();
		expect(
			seatCall(`${base}/x%2f..%2fplugins%2fsteam%2fui-credential`),
		).toBeNull();
		expect(seatCall(`${base}/store%2Finstall`)).toBeNull();
		expect(seatCall(`${base}/a%2F%2Fb`)).toBeNull();
		expect(seatCall(`${base}/%252e%252e/x`)?.rest).toBe("%2e%2e/x");
	});

	test("keeps a question mark inside its segment, escaped, so it starts no query", () => {
		const call = seatCall(`${base}/store/install%3F`);
		expect(call?.rest).toBe("store/install?");
		expect(call && encodeRest(call.rest)).toBe("store/install%3F");
	});

	test("forwards the resolved path, each segment escaped once", () => {
		const call = seatCall(`${base}/store/sources/my%20src`);
		expect(call && seatPath(call.id, encodeRest(call.rest))).toBe(
			`${base}/store/sources/my%20src`,
		);
	});
});

test("a plugin's credential and page relay are refused whichever way they are spelled", () => {
	for (const rest of [
		"plugins/steam/ui-credential",
		"plugins/steam/ui",
		"plugins/steam/ui/a.html",
		"plugins/steam/UI/a",
		seatCall(`${base}/plugins/a/../steam/ui-credential`)?.rest,
		seatCall(`${base}/plugins/steam/ui/`)?.rest,
		seatCall(`${base}/plugins/steam/%75i/a.html`)?.rest,
	]) {
		expect(isPluginPrivate(rest ?? ""), rest).toBe(true);
	}
	for (const rest of [
		"plugins",
		"plugins/steam",
		"plugins/steam/ui-credentials-other",
		"plugins/steam/uix/a",
	]) {
		expect(isPluginPrivate(rest), rest).toBe(false);
	}
});
