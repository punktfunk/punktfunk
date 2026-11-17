// Calls to the management API go through `mgmtFetch`, the two relays aside, so its error mapping
// is the one the console sees: 503 with no host token, 502 for an unreachable host or a rejected
// token.
import { afterAll, afterEach, describe, expect, test } from "bun:test";
import { mgmtFetch } from "./forward";
import { fetchUiCredential } from "./pluginProxy";

const upstream = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: (req) =>
		new URL(req.url).pathname === "/reject"
			? new Response("", { status: 401 })
			: Response.json({ authorization: req.headers.get("authorization") }),
});
const dead = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: () => new Response(),
});
const deadUrl = `http://127.0.0.1:${dead.port}`;
dead.stop(true);

afterAll(() => upstream.stop(true));
afterEach(() => {
	delete process.env.PUNKTFUNK_MGMT_URL;
	delete process.env.PUNKTFUNK_MGMT_TOKEN;
});

const status = (p: Promise<unknown>) =>
	p.then(
		() => 0,
		(e: { statusCode?: number }) => e.statusCode,
	);

describe("mgmtFetch", () => {
	test("sends the host token", async () => {
		process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${upstream.port}`;
		process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
		const res = await mgmtFetch("/any");
		expect(await res.json()).toEqual({ authorization: "Bearer t0k" });
	});

	test("answers 503 without a token and 502 for a rejected one", async () => {
		process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${upstream.port}`;
		expect(await status(mgmtFetch("/any"))).toBe(503);
		process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
		expect(await status(mgmtFetch("/reject"))).toBe(502);
	});

	test("answers 502 when the host is not listening", async () => {
		process.env.PUNKTFUNK_MGMT_URL = deadUrl;
		process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
		expect(await status(mgmtFetch("/any"))).toBe(502);
	});
});

describe("fetchUiCredential", () => {
	test("reads an unreachable host as an offline plugin", async () => {
		process.env.PUNKTFUNK_MGMT_URL = deadUrl;
		process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
		expect(await fetchUiCredential("steam", { bustCache: true })).toBeNull();
	});
});
