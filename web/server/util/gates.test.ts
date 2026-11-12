import { afterAll, afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Glob } from "bun";
import { createApp, createRouter, type EventHandler, toWebHandler } from "h3";
import { GATES, type Route, routeFile, seatGate } from "./gates";

const routes = join(import.meta.dir, "../routes/api/v1");
const repo = join(import.meta.dir, "../../..");
const asking = GATES.filter((r) => !r.open && !r.refused);
const conditional = GATES.filter((r) => r.when);
const row = (method: string, pattern: string): Route => {
	const found = GATES.find((r) => r.method === method && r.pattern === pattern);
	if (!found) throw new Error(`no row ${method} ${pattern}`);
	return found;
};

describe("seatGate", () => {
	test("names the box's gate for each call it guards", () => {
		const gate = (method: string, rest: string) => {
			const found = seatGate(method, rest);
			const route = GATES.find((r) => r.seat === found?.handler);
			return (
				found && route && { route: routeFile(route), params: found.params }
			);
		};
		expect(gate("PUT", "hooks")).toEqual({ route: "hooks.put", params: {} });
		expect(gate("POST", "store/install")?.route).toBe("store/install.post");
		expect(gate("PUT", "store/sources/mine")).toEqual({
			route: "store/sources/[name].put",
			params: { name: "mine" },
		});
		expect(gate("POST", "library/custom")?.route).toBe("library/custom.post");
		expect(gate("put", "library/custom/7")).toEqual({
			route: "library/custom/[id].put",
			params: { id: "7" },
		});
		expect(gate("PUT", "library/provider/romm")).toEqual({
			route: "library/provider/[provider].put",
			params: { provider: "romm" },
		});
		expect(gate("POST", "emulators/ppsspp/remove")).toEqual({
			route: "emulators/[id]/remove.post",
			params: { id: "ppsspp" },
		});
		expect(gate("POST", "plugin-access/romm/decide")?.route).toBe(
			"plugin-access/[plugin]/decide.post",
		);
		expect(gate("PATCH", "host/settings")?.route).toBe("host/settings.patch");
		expect(gate("PUT", "plugins/romm")?.route).toBe("plugins/[id].put");
	});

	test("leaves the reads and the ungated writes to the relay", () => {
		expect(seatGate("GET", "hooks")).toBeNull();
		expect(seatGate("DELETE", "store/sources/mine")).toBeNull();
		expect(seatGate("DELETE", "library/custom/7")).toBeNull();
		expect(seatGate("POST", "library/page")).toBeNull();
		expect(seatGate("PUT", "library/custom/7/extra")).toBeNull();
		expect(seatGate("PUT", "store/sources/")).toBeNull();
		expect(seatGate("POST", "emulators/ppsspp/install")).toBeNull();
	});
});

test("the rows that ask are the route files that ask for the password", () => {
	const files = [...new Glob("**/*.ts").scanSync(routes)]
		.filter((file) =>
			/from "(\.\.\/)+util\/(confirm|libraryConfirm)"/.test(
				readFileSync(join(routes, file), "utf8"),
			),
		)
		.map((file) => file.replace(/\.ts$/, ""));
	expect(asking.map(routeFile).sort()).toEqual(files.sort());
});

test("a row's handler and condition are its own file's", async () => {
	for (const route of GATES.filter((r) => r.seat || r.when)) {
		const file = await import(join(routes, `${routeFile(route)}.ts`));
		if (route.seat) expect(route.seat, routeFile(route)).toBe(file.default);
		expect(route.when, routeFile(route)).toBe(file.when);
	}
});

test("a row with no seat gate is a path the seat's host refuses", () => {
	const rust = readFileSync(
		join(repo, "crates/host/punktfunk-host/src/mgmt/seat_proxy.rs"),
		"utf8",
	);
	const list = /fn proxy_refuses[^}]*matches!\(\s*first,([^)]*)\)/.exec(
		rust,
	)?.[1];
	expect(list, "proxy_refuses in mgmt/seat_proxy.rs").toBeDefined();
	const refused = [...(list ?? "").matchAll(/"([^"]+)"/g)].map((m) => m[1]);
	const relayed = GATES.filter(
		(r) => !r.open && !r.seat && !refused.includes(r.pattern.split("/")[0]),
	);
	expect(relayed.map(routeFile)).toEqual([]);
});

test("every mutating operation in api/openapi.json has one decided row", () => {
	const spec: { paths: Record<string, Record<string, unknown>> } = JSON.parse(
		readFileSync(join(repo, "api/openapi.json"), "utf8"),
	);
	const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
		["POST", "PUT", "PATCH", "DELETE"]
			.filter((method) => method.toLowerCase() in item)
			.map((method) => `${method} ${path.replace(/^\/api\/v1\//, "")}`),
	);
	const rows = GATES.map((r) => `${r.method} ${r.pattern}`);
	expect(rows.filter((row, i) => rows.indexOf(row) !== i)).toEqual([]);
	expect(operations.filter((op) => !rows.includes(op))).toEqual([]);
	expect(rows.filter((row) => !operations.includes(row))).toEqual([]);
	const undecided = GATES.filter(
		(r) =>
			r.open !== undefined && (!r.open.trim() || /undecided/i.test(r.open)),
	);
	expect(undecided.map(routeFile)).toEqual([]);
});

/** The host as the console's server reaches it: answers every call and records it. */
const calls: { method: string; path: string; body: unknown }[] = [];
const STORED_GUEST = {
	name: "Couch",
	fingerprint: "ab12",
	grants: 0x01,
	expires_unix: null,
	until_disconnect: false,
};
const upstream = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: async (req) => {
		const { pathname } = new URL(req.url);
		if (req.method === "GET" && pathname === "/api/v1/native/clients")
			return Response.json([STORED_GUEST]);
		const text = await req.text();
		calls.push({
			method: req.method,
			path: pathname,
			body: text ? JSON.parse(text) : null,
		});
		return Response.json({});
	},
});
afterAll(() => upstream.stop(true));
afterEach(() => {
	calls.length = 0;
	delete process.env.PUNKTFUNK_MGMT_URL;
	delete process.env.PUNKTFUNK_MGMT_TOKEN;
});

/** `route`'s file mounted as the console mounts it, with no console password configured. */
async function mount(route: Route) {
	process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${upstream.port}`;
	process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
	delete process.env.PUNKTFUNK_UI_PASSWORD;
	delete process.env.PUNKTFUNK_UI_PASSWORD_HASH;
	const file = await import(join(routes, `${routeFile(route)}.ts`));
	const router = createRouter();
	const path = `/api/v1/${route.pattern.replace(/\{(\w+)\}/g, ":$1")}`;
	router.add(path, file.default as EventHandler, route.method.toLowerCase());
	const handle = toWebHandler(createApp().use(router));
	return (body: unknown) =>
		handle(
			new Request(
				`http://console/api/v1/${route.pattern.replace(/\{\w+\}/g, "ab12")}`,
				{
					method: route.method,
					headers: { "content-type": "application/json" },
					body: JSON.stringify(body),
				},
			),
		);
}

/** For each conditional row: a call it asks on, and one it lets through. */
const BOTH_WAYS: [Route, unknown, unknown][] = [
	[row("POST", "emulators/{id}/remove"), { purge: true }, { purge: false }],
	[
		row("POST", "plugin-access/{plugin}/decide"),
		{ path: "/games", decision: "allow" },
		{ path: "/games", decision: "deny" },
	],
	[
		row("PATCH", "native/clients/{fingerprint}"),
		{ grants: 0x21 },
		{ expires_in_secs: 0 },
	],
	[row("PATCH", "host/settings"), { gamestream: true }, { max_fps: 60 }],
];

test("each conditional row is pinned both ways", () => {
	expect(BOTH_WAYS.map(([r]) => routeFile(r)).sort()).toEqual(
		conditional.map(routeFile).sort(),
	);
});

describe.each(
	BOTH_WAYS.map(
		([r, asks, passes]) => [routeFile(r), r, asks, passes] as const,
	),
)("%s", (_file, route, asks, passes) => {
	test("its condition reads the body", async () => {
		process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${upstream.port}`;
		process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
		const call = (body: unknown) => ({
			body: body as Record<string, unknown>,
			params: { id: "ab12", plugin: "ab12", fingerprint: "AB12" },
		});
		expect(await route.when?.(call(asks))).toBe(true);
		expect(await route.when?.(call(passes))).toBe(false);
	});

	test("its file asks on one and forwards the other without the password", async () => {
		const send = await mount(route);
		const refused = await send(asks);
		expect(refused.status).toBe(503);
		expect(calls).toEqual([]);
		const res = await send({ ...(passes as object), password: "typed" });
		expect(res.status).toBe(200);
		expect(calls).toHaveLength(1);
		expect(calls[0]?.body).not.toHaveProperty("password");
	});
});

test("the five plane settings ask; any other setting does not", async () => {
	const when = row("PATCH", "host/settings").when;
	const asks = (body: object) =>
		when?.({ body: body as Record<string, unknown>, params: {} });
	for (const id of [
		"gamestream",
		"webtransport",
		"webtransport_origins",
		"gamestream_encrypt",
		"clipboard",
	]) {
		expect(await asks({ [id]: null }), id).toBe(true);
		expect(await asks({ max_fps: 60, [id]: "x" }), id).toBe(true);
	}
	expect(await asks({ max_fps: 60, gamescope_isolate: true })).toBe(false);
});

test("a refused row answers with a sentence and never reaches the host", async () => {
	for (const route of GATES.filter((r) => r.refused)) {
		const res = await (await mount(route))({ title: "x" });
		expect(res.status, routeFile(route)).toBe(403);
		const { error } = (await res.json()) as { error: string };
		expect(error).toMatch(/^[A-Z].*\.$/);
		expect(calls).toEqual([]);
	}
});
