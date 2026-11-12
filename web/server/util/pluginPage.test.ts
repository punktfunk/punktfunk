// A plugin page's path is its only credential on the plugin listener: minted per plugin, ended by
// expiry or logout, and never replaced by the session cookie. Every page is sandboxed.
import { afterAll, afterEach, beforeEach, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	createApp,
	defineEventHandler,
	getRequestURL,
	toWebHandler,
	useSession,
} from "h3";
import gate from "../middleware/auth";
import pluginUi from "../routes/plugin-ui/[...]";
import { sessionConfig, sessionEpoch } from "./auth";
import {
	mintPluginPagePath,
	parsePluginPage,
	pluginPageSegment,
} from "./pluginPage";

const KEY = "k".repeat(40);
const DAY_MS = 86_400_000;
const WEEK_S = 7 * 86_400;
const NOW = Date.UTC(2026, 10, 12, 15, 30);

const seg = (id: string, epoch = 1, now = NOW) =>
	pluginPageSegment(id, KEY, epoch, WEEK_S, now);
const parse = (path: string, epoch = 1, now = NOW) =>
	parsePluginPage(path, KEY, epoch, now);

test("a minted segment names its plugin, its public prefix and the plugin's own path", () => {
	const s = seg("rom-manager");
	expect(s).toMatch(/^rom-manager-[0-9a-z]+-[0-9a-f]{32}$/);
	expect(parse(`/plugin-ui/${s}/api/scan`)).toEqual({
		id: "rom-manager",
		prefix: `/plugin-ui/${s}`,
		rest: "/api/scan",
	});
	expect(parse(`/plugin-ui/${s}`)?.rest).toBe("/");
});

test("it lives a session's length, rounded up to a whole day", () => {
	const s = seg("a");
	expect(parse(`/plugin-ui/${s}/`, 1, NOW + 7 * DAY_MS)).not.toBeNull();
	expect(parse(`/plugin-ui/${s}/`, 1, NOW + 8 * DAY_MS)).toBeNull();
	// The same day mints the same path, so a page's asset URLs stay cached.
	expect(seg("a", 1, NOW + 60_000)).toBe(s);
});

test("another plugin, a tampered tag or expiry, another key, or a logout refuse it", () => {
	const s = seg("a");
	const [, exp, mac] = s.split("-");
	const flip = (c: string) => (c === "0" ? "1" : "0");
	for (const path of [
		`/plugin-ui/b-${exp}-${mac}/__config`,
		`/plugin-ui/a-${exp}-${flip(mac?.[0] ?? "")}${mac?.slice(1)}/`,
		`/plugin-ui/a-${Number.parseInt(exp ?? "", 36) + 86_400}-${mac}/`,
	]) {
		expect(parse(path), path).toBeNull();
	}
	expect(
		parsePluginPage(`/plugin-ui/${s}/`, "o".repeat(40), 1, NOW),
	).toBeNull();
	expect(parse(`/plugin-ui/${s}/`, 2)).toBeNull();
});

test("a bare or malformed plugin path is refused", () => {
	for (const path of [
		"/plugin-ui/a/",
		"/plugin-ui/a/__config",
		"/plugin-ui/",
		"/plugin-ui",
		`/plugin-ui/${seg("a").toUpperCase()}/`,
		`/plugin-ui/x/${seg("a")}/`,
		`/api/${seg("a")}/`,
	]) {
		expect(parse(path), path).toBeNull();
	}
});

// ── through the gate and the proxy ─────────────────────────────────────────────────────────────

/** What each plugin was asked: the plugin is the secret the host handed out for it. */
const seen: {
	plugin: string;
	path: string;
	prefix: string | null;
	cookie: string | null;
}[] = [];
const plugin = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: (req) => {
		const url = new URL(req.url);
		seen.push({
			plugin: req.headers.get("authorization") ?? "",
			path: url.pathname,
			prefix: req.headers.get("x-forwarded-prefix"),
			cookie: req.headers.get("cookie"),
		});
		const own = {
			"content-security-policy": "default-src *",
			"access-control-allow-origin": "https://elsewhere.test",
		};
		return url.pathname === "/"
			? new Response("<html><head></head><body>page</body></html>", {
					headers: { ...own, "content-type": "text/html" },
				})
			: Response.json({ apiKey: "plugin config" }, { headers: own });
	},
});
const host = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: (req) => {
		const id = new URL(req.url).pathname.match(
			/plugins\/([a-z]+)\/ui-credential$/,
		)?.[1];
		return id
			? Response.json({ port: plugin.port, secret: `secret-${id}` })
			: new Response("no", { status: 404 });
	},
});
const consoleOk = defineEventHandler((e) =>
	getRequestURL(e).pathname.startsWith("/plugin-ui/") ? pluginUi(e) : "console",
);
const handle = toWebHandler(createApp().use(gate).use(consoleOk));
const login = toWebHandler(
	createApp().use(
		defineEventHandler(async (e) => {
			const s = await useSession(e, sessionConfig());
			await s.update({ authenticated: true, epoch: sessionEpoch() });
			return "ok";
		}),
	),
);

const ENV = {
	PUNKTFUNK_UI_SECRET: KEY,
	PUNKTFUNK_UI_PASSWORD: "pw",
	PUNKTFUNK_UI_EPOCH_FILE: join(
		mkdtempSync(join(tmpdir(), "pf-page-")),
		"epoch",
	),
	PUNKTFUNK_MGMT_TOKEN: "t0k",
};
beforeEach(() => {
	Object.assign(process.env, ENV, {
		PUNKTFUNK_MGMT_URL: `http://127.0.0.1:${host.port}`,
	});
});
afterEach(() => {
	seen.length = 0;
	for (const k of [...Object.keys(ENV), "PUNKTFUNK_MGMT_URL"])
		delete process.env[k];
});
afterAll(() => {
	plugin.stop(true);
	host.stop(true);
});

const send = (
	path: string,
	init: RequestInit & { lane?: "plugin" | "console" } = {},
) => {
	const headers = new Headers(init.headers);
	headers.set("x-pf-listener", init.lane ?? "plugin");
	return handle(new Request(`http://host${path}`, { ...init, headers }));
};

test("the session cookie alone opens no plugin page", async () => {
	const cookie = (await login(new Request("http://host/"))).headers
		.get("set-cookie")
		?.split(";")[0];
	expect(cookie).toBeTruthy();
	const live = await send("/x", {
		lane: "console",
		headers: { cookie: cookie ?? "" },
	});
	expect(await live.text()).toBe("console");
	for (const path of ["/plugin-ui/a/", "/plugin-ui/a/__config"]) {
		const res = await send(path, { headers: { cookie: cookie ?? "" } });
		expect(res.status, path).toBe(401);
		expect(res.headers.get("access-control-allow-origin")).toBeNull();
	}
	expect(seen).toEqual([]);
});

test("one plugin's page path does not reach another plugin", async () => {
	const a = mintPluginPagePath("a");
	const res = await send(
		`${a.replace("/plugin-ui/a-", "/plugin-ui/b-")}__config`,
	);
	expect(res.status).toBe(401);
	expect(seen).toEqual([]);
});

test("a page path reaches its plugin with the segment stripped and the prefix kept", async () => {
	const a = mintPluginPagePath("a");
	const res = await send(`${a}__config?x=1`, {
		headers: { cookie: "pf_session=nope" },
	});
	expect(res.status).toBe(200);
	expect(res.headers.get("access-control-allow-origin")).toBe("*");
	expect(seen).toEqual([
		{
			plugin: "Bearer secret-a",
			path: "/__config",
			prefix: a.slice(0, -1),
			cookie: null,
		},
	]);
});

test("every plugin listener answer is sandboxed, without allow-same-origin", async () => {
	const a = mintPluginPagePath("a");
	for (const res of [
		await send(a),
		await send(`${a}__config`),
		await send("/plugin-ui/a/"),
	]) {
		const csp = res.headers.get("content-security-policy") ?? "";
		expect(csp).toStartWith("sandbox allow-scripts ");
		expect(csp).not.toContain("allow-same-origin");
		expect(csp).toContain("frame-ancestors");
	}
	expect(await (await send(a)).text()).toContain("pf-ui:theme-request");
});

test("a preflight is answered for a page path and never reaches the plugin", async () => {
	const a = mintPluginPagePath("a");
	const ask = {
		method: "OPTIONS",
		headers: {
			origin: "null",
			"access-control-request-method": "PUT",
			"access-control-request-headers": "content-type",
		},
	};
	const res = await send(`${a}__config`, ask);
	expect(res.status).toBe(204);
	expect(res.headers.get("access-control-allow-origin")).toBe("*");
	expect(res.headers.get("access-control-allow-methods")).toContain("PUT");
	expect(res.headers.get("access-control-allow-headers")).toBe("content-type");
	expect((await send("/plugin-ui/a/__config", ask)).status).toBe(401);
	expect(seen).toEqual([]);
});

test("a page's own write from its opaque origin passes", async () => {
	const res = await send(`${mintPluginPagePath("a")}__config`, {
		method: "PUT",
		headers: {
			origin: "null",
			"sec-fetch-site": "cross-site",
			"content-type": "application/json",
		},
		body: "{}",
	});
	expect(res.status).toBe(200);
	expect(seen.map((s) => s.path)).toEqual(["/__config"]);
});

test("each listener still refuses the other's paths", async () => {
	const a = mintPluginPagePath("a");
	expect((await send(a, { lane: "console" })).status).toBe(404);
	expect((await send(`/api/v1/plugins/a/ui-credential?p=${a}`)).status).toBe(
		404,
	);
	expect(seen).toEqual([]);
});
