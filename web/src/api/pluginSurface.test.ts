import { afterEach, expect, test } from "bun:test";
import { pluginSurface, refusalText } from "./pluginSurface";

const originalFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = originalFetch;
});

const answer = (status: number, body: unknown) => {
	globalThis.fetch = (async () =>
		new Response(JSON.stringify(body), { status })) as unknown as typeof fetch;
};

test("a refusal keeps its status and the route's 404 marker", async () => {
	answer(404, { error: "plugin serves no config surface", noConfig: true });
	const r = await pluginSurface("/api/plugin-config/x");
	expect(r).toEqual({
		ok: false,
		status: 404,
		body: { error: "plugin serves no config surface", noConfig: true },
	});
});

test("an answer comes back as its body", async () => {
	answer(200, { value: 1 });
	expect(await pluginSurface("/api/plugin-config/x")).toEqual({
		ok: true,
		status: 200,
		body: { value: 1 },
	});
});

test("the plugin's issue wins over the route's error, and h3's flag is not text", () => {
	expect(refusalText({ issue: "bad port", error: "refused" })).toBe("bad port");
	expect(refusalText({ error: "not reachable" })).toBe("not reachable");
	expect(refusalText({ error: true, statusMessage: "no token" })).toBe(
		"no token",
	);
});
