import { expect, test } from "bun:test";
import { apiFetch, inSeat, seatUrl } from "./fetcher";

const KID = "0a1b2c3d4e5f";

test("a seat's call is the box's proxy path, and only /api/v1 moves", () => {
	expect(seatUrl("/api/v1/library/page?limit=1", KID)).toBe(
		`/api/v1/profiles/${KID}/proxy/library/page?limit=1`,
	);
	expect(seatUrl("/api/v1/library", null)).toBe("/api/v1/library");
	expect(seatUrl("/api/plugin-config/x", KID)).toBe("/api/plugin-config/x");
	expect(seatUrl("/_auth/ui-config", KID)).toBe("/_auth/ui-config");
});

test("inSeat aims a call and hands the next one back to the box", async () => {
	const originalFetch = globalThis.fetch;
	const urls: string[] = [];
	globalThis.fetch = (async (url: string) => {
		urls.push(url);
		return new Response("{}");
	}) as typeof fetch;
	try {
		await inSeat(KID, () => apiFetch("/api/v1/library"));
		await apiFetch("/api/v1/library");
		expect(urls).toEqual([
			`/api/v1/profiles/${KID}/proxy/library`,
			"/api/v1/library",
		]);
	} finally {
		globalThis.fetch = originalFetch;
	}
});

test("redirects only the auth middleware's 401", async () => {
	const originalFetch = globalThis.fetch;
	const originalSetTimeout = globalThis.setTimeout;
	const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
	let responseBody: unknown = {
		statusCode: 401,
		statusMessage: "password confirmation failed",
	};
	let redirects = 0;

	globalThis.fetch = (async () =>
		new Response(JSON.stringify(responseBody), {
			status: 401,
			statusText: "Unauthorized",
		})) as typeof fetch;
	globalThis.setTimeout = ((_: () => void) => {
		redirects += 1;
		return 1;
	}) as typeof setTimeout;
	Object.defineProperty(globalThis, "window", {
		configurable: true,
		value: {
			location: {
				pathname: "/automation",
				search: "",
				hash: "",
				href: "/automation",
			},
		},
	});

	try {
		await expect(apiFetch("/api/v1/hooks")).rejects.toMatchObject({
			status: 401,
		});
		expect(redirects).toBe(0);

		responseBody = { error: "unauthorized" };
		await expect(apiFetch("/api/v1/hooks")).rejects.toMatchObject({
			status: 401,
		});
		expect(redirects).toBe(1);
	} finally {
		globalThis.fetch = originalFetch;
		globalThis.setTimeout = originalSetTimeout;
		if (originalWindow) {
			Object.defineProperty(globalThis, "window", originalWindow);
		} else {
			Reflect.deleteProperty(globalThis, "window");
		}
	}
});
