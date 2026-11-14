// A plugin form's save grants the folders the operator confirmed (`decide` `allow` on the
// operator's lane), so a save that hands one over asks for the console password like the gated
// route, and a save that hands none over does not. The plugin's own answers decide no grant.
import { afterAll, afterEach, beforeEach, expect, test } from "bun:test";
import { createApp, createRouter, toWebHandler } from "h3";
import {
	GRANT_HEADER,
	type HandedPath,
	type Handover,
} from "../../src/lib/handed-paths";
import { PASSWORD_HEADER } from "../../src/lib/password-gates";
import configRoute from "../routes/api/plugin-config/[id]";
import gameRoute from "../routes/api/plugin-game/[id]";
import { bustCredential } from "./pluginProxy";

const SCHEMA = {
	schema: {
		type: "object",
		properties: { roms: { type: "string", format: "pf:path" } },
	},
};
/** What each fake saw: the plugin's reads and saves, and the host's access calls. */
const reads: string[] = [];
const saves: unknown[] = [];
const access: { route: string; body: unknown }[] = [];
/** The refusal the host answers a `decide` with, when a test sets one. */
let refusal: Record<string, unknown> | null = null;

const plugin = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: async (req) => {
		if (req.method === "GET") {
			reads.push(req.url);
			return Response.json({ schema: SCHEMA, value: { roms: "/games/old" } });
		}
		saves.push(JSON.parse(await req.text()));
		return Response.json({ ok: true });
	},
});
const host = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: async (req) => {
		const { pathname } = new URL(req.url);
		if (pathname === "/api/v1/plugins/demo/ui-credential")
			return Response.json({ port: plugin.port, secret: "s" });
		const route = pathname.split("/").pop() ?? "";
		access.push({ route, body: JSON.parse(await req.text()) });
		if (route === "decide" && refusal)
			return Response.json(refusal, { status: 400 });
		return Response.json({});
	},
});

const router = createRouter()
	.add("/api/plugin-config/:id", configRoute, "put")
	.add("/api/plugin-game/:id", gameRoute, "put");
const handle = toWebHandler(createApp().use(router));
/** What the drawer sends for a form loaded with `/games/old` and saved with `roms`. */
const loadedOld = (roms: string): Handover =>
	roms === "/games/old"
		? { grant: [], keep: null }
		: { grant: [{ path: roms, write: false }], keep: [roms] };
const save = (
	url: string,
	roms: string,
	password?: string,
	handover: Handover | string = loadedOld(roms),
) =>
	handle(
		new Request(`http://console${url}`, {
			method: "PUT",
			headers: {
				"content-type": "application/json",
				"x-pf-peer-ip": "10.49.0.1",
				...(password === undefined ? {} : { [PASSWORD_HEADER]: password }),
				[GRANT_HEADER]:
					typeof handover === "string"
						? handover
						: encodeURIComponent(JSON.stringify(handover)),
			},
			body: JSON.stringify({ roms }),
		}),
	);

beforeEach(async () => {
	process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${host.port}`;
	process.env.PUNKTFUNK_MGMT_TOKEN = "t0k";
	delete process.env.PUNKTFUNK_UI_PASSWORD;
	process.env.PUNKTFUNK_UI_PASSWORD_HASH = await Bun.password.hash("typed", {
		algorithm: "argon2id",
	});
	bustCredential("demo");
});
afterEach(() => {
	refusal = null;
	reads.length = 0;
	saves.length = 0;
	access.length = 0;
	delete process.env.PUNKTFUNK_MGMT_URL;
	delete process.env.PUNKTFUNK_MGMT_TOKEN;
	delete process.env.PUNKTFUNK_UI_PASSWORD_HASH;
});
afterAll(() => {
	plugin.stop(true);
	host.stop(true);
});

test("a save that hands over no new folder does not ask", async () => {
	const res = await save("/api/plugin-config/demo", "/games/old");
	expect(res.status).toBe(200);
	expect(saves).toEqual([{ roms: "/games/old" }]);
	expect(access).toEqual([]);
});

test("a save that hands over a new folder saves nothing without the password", async () => {
	for (const password of [undefined, "wrong"]) {
		const res = await save("/api/plugin-config/demo", "/games/new", password);
		expect(res.status).toBe(401);
	}
	const game = await save("/api/plugin-game/demo?entry=custom:7", "/games/new");
	expect(game.status).toBe(401);
	expect(saves).toEqual([]);
	expect(access).toEqual([]);
});

test("with the password it saves and grants the new folder", async () => {
	const res = await save("/api/plugin-config/demo", "/games/new", "typed");
	expect(res.status).toBe(200);
	expect(saves).toEqual([{ roms: "/games/new" }]);
	expect(access.map((a) => a.route)).toEqual(["decide", "release"]);
	expect(access[0]?.body).toEqual({
		path: "/games/new",
		decision: "allow",
		write: false,
		form: "config",
	});
});

test("a refused grant relays the host's code and params with its English", async () => {
	refusal = {
		error: "Couldn't record the decision",
		code: "plugin_access_decision_not_recorded",
		params: { reason: "disk" },
	};
	const res = await save("/api/plugin-config/demo", "/games/new", "typed");
	expect(res.status).toBe(200);
	const body = (await res.json()) as { access: { refused: unknown[] } };
	expect(body.access.refused).toEqual([
		{
			path: "/games/new",
			error: "Couldn't record the decision",
			code: "plugin_access_decision_not_recorded",
			params: { reason: "disk" },
		},
	]);
});

test("grants only a confirmed path the value holds, at its access, and reads no plugin", async () => {
	const confirmed: HandedPath[] = [
		{ path: "/games/new", write: true },
		{ path: "/home/u/.local/share/punktfunk/emulators/x/app", write: true },
	];
	const res = await save("/api/plugin-config/demo", " /games/new ", "typed", {
		grant: confirmed,
		keep: null,
	});
	expect(res.status).toBe(200);
	expect(reads).toEqual([]);
	expect(access).toEqual([
		{
			route: "decide",
			body: {
				path: "/games/new",
				decision: "allow",
				write: true,
				form: "config",
			},
		},
	]);
});

test("a confirmed path the value lacks asks nothing and grants nothing", async () => {
	const res = await save("/api/plugin-config/demo", "/games/new", undefined, {
		grant: [{ path: "/elsewhere", write: false }],
		keep: ["/games/new", "/elsewhere"],
	});
	expect(res.status).toBe(200);
	expect(saves).toEqual([{ roms: "/games/new" }]);
	expect(access).toEqual([
		{ route: "release", body: { form: "config", keep: ["/games/new"] } },
	]);
});

test("a grant header that doesn't parse saves nothing", async () => {
	for (const bad of ["%7B", encodeURIComponent('{"grant":[{"path":1}]}')]) {
		const res = await save(
			"/api/plugin-config/demo",
			"/games/new",
			"typed",
			bad,
		);
		expect(res.status).toBe(400);
	}
	expect(saves).toEqual([]);
	expect(access).toEqual([]);
});
