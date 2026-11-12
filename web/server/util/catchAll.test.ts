// The `/api/**` passthrough forwards to the host with the admin bearer, so it must never reach a
// plugin's credential or page relay, however the path is spelled. Anything else still goes through.
import { afterAll, afterEach, beforeEach, expect, test } from "bun:test";
import { createApp, toWebHandler } from "h3";
import catchAll from "../routes/api/[...]";

/** Every path the fake host was asked for. */
const hits: string[] = [];
const host = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: (req) => {
		hits.push(new URL(req.url).pathname);
		return Response.json({});
	},
});
const handle = toWebHandler(createApp().use(catchAll));

beforeEach(() => {
	process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${host.port}`;
	process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
});
afterEach(() => {
	hits.length = 0;
	delete process.env.PUNKTFUNK_MGMT_URL;
	delete process.env.PUNKTFUNK_MGMT_TOKEN;
});
afterAll(() => host.stop(true));

const status = async (path: string) =>
	(await handle(new Request(`http://console${path}`))).status;

test("a plugin's credential and page relay are refused in every spelling", async () => {
	for (const path of [
		"/api/v1/plugins/x/ui-credential",
		"/api/v1/plugins/x/ui",
		"/api/v1/plugins/x/ui/",
		"/api/v1/plugins/x/ui/a.html",
		"/api/v1/plugins/x/UI/a",
		"/api/v1/plugins/x/ui/a/b?c=d",
		"/api//v1/plugins/x/ui/a",
		"/api/./v1/plugins/x/ui/a",
		"/api/v1/plugins/x/%75i",
		"/api/v1/plugins/x/%75i/a.html",
		"/api/v1/plugins/x/y/../ui/a",
	]) {
		expect(await status(path), path).toBe(403);
	}
	expect(hits).toEqual([]);
});

test("the plugin list and look-alike paths still pass", async () => {
	for (const path of [
		"/api/v1/plugins",
		"/api/v1/plugins/x",
		"/api/v1/plugins/x/ui-credentials-other",
		"/api/v1/plugins/x/uix/a",
		"/api/v1/plugins/ui/a",
	]) {
		expect(await status(path), path).toBe(200);
	}
	expect(hits).toHaveLength(5);
});
