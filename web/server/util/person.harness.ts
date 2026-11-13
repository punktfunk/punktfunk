// For the person tests: a box and a seat host that record every call, and the console as Nitro
// mounts it. The seat host serves a certificate made at load, which the box lists as its pin.
import {
	createHash,
	generateKeyPairSync,
	randomBytes,
	sign,
	X509Certificate,
} from "node:crypto";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Glob } from "bun";
import { createApp, createRouter, type EventHandler, toWebHandler } from "h3";
import gate from "../middleware/auth";

export const ADMIN = "a".repeat(64);
export const PROFILE = "a1b2c3d4e5f6";
export const OTHER = "f6e5d4c3b2a1";
export const OWNER = "000000000000";

/** One DER element: `tag`, its shortest length, then `parts`. Bodies stay under 64 KiB. */
function der(tag: number, ...parts: Buffer[]): Buffer {
	const body = Buffer.concat(parts);
	const n = body.length;
	const len = n < 0x80 ? [n] : n < 0x100 ? [0x81, n] : [0x82, n >> 8, n & 0xff];
	return Buffer.concat([Buffer.from([tag, ...len]), body]);
}

/** `YYMMDDHHMMSSZ`, X.509's UTCTime. */
const utcTime = (d: Date) =>
	der(0x17, Buffer.from(`${d.toISOString().slice(2, 19).replace(/\D/g, "")}Z`));

/**
 * A self-signed P-256 certificate and its key, made in memory so no key is committed. Version 1,
 * no extensions: the console trusts the one certificate whose SHA-256 the box lists, nothing more.
 */
export function selfSigned(name: string): { cert: string; key: string } {
	const { publicKey, privateKey } = generateKeyPairSync("ec", {
		namedCurve: "P-256",
	});
	const ecdsaSha256 = der(
		0x30,
		der(0x06, Buffer.from("2a8648ce3d040302", "hex")),
	);
	const cn = der(
		0x30,
		der(
			0x31,
			der(
				0x30,
				der(0x06, Buffer.from("550403", "hex")),
				der(0x0c, Buffer.from(name)),
			),
		),
	);
	const serial = randomBytes(8);
	serial[0] = ((serial[0] ?? 0) & 0x7f) | 0x40;
	const day = 86_400_000;
	const tbs = der(
		0x30,
		der(0x02, serial),
		ecdsaSha256,
		cn,
		der(
			0x30,
			utcTime(new Date(Date.now() - day)),
			utcTime(new Date(Date.now() + day)),
		),
		cn,
		publicKey.export({ type: "spki", format: "der" }),
	);
	const signature = sign("sha256", tbs, privateKey);
	const cert = der(
		0x30,
		tbs,
		ecdsaSha256,
		der(0x03, Buffer.from([0]), signature),
	);
	return {
		cert: new X509Certificate(cert).toString(),
		key: privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
	};
}

const { cert, key } = selfSigned("punktfunk test seat");
export const PIN = createHash("sha256")
	.update(new X509Certificate(cert).raw)
	.digest("hex");

export interface Call {
	method: string;
	path: string;
	search: string;
	authorization: string | null;
	cookie: string | null;
	headers: Record<string, string>;
	body: string;
}

async function record(req: Request, into: Call[]): Promise<Call> {
	const url = new URL(req.url);
	const call: Call = {
		method: req.method,
		path: url.pathname,
		search: url.search,
		authorization: req.headers.get("authorization"),
		cookie: req.headers.get("cookie"),
		headers: Object.fromEntries(req.headers),
		body: await req.text(),
	};
	into.push(call);
	return call;
}

/** What the seat host answers a redemption: the grant, or a refusal status. */
export const seatSays: { redeem: number | "grant"; token: string } = {
	redeem: "grant",
	token: "5".repeat(64),
};
/** A plugin the seat host runs, and the secret its UI server takes. */
export const SEAT_PLUGIN = "quest";
export const PLUGIN_SECRET = "p".repeat(32);
export const pluginCalls: Call[] = [];
export const plugin = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: async (req) => {
		const call = await record(req, pluginCalls);
		return call.path === "/__health"
			? new Response("ok")
			: new Response(`<p>${call.path}</p>`, {
					headers: { "content-type": "text/plain" },
				});
	},
});
plugin.unref();

/** The management port the box lists for `PROFILE`'s seat; 0 is the seat host below. */
export const listed = { port: 0 };
export const seatCalls: Call[] = [];
export const boxCalls: Call[] = [];

export const seat = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	tls: { cert, key },
	fetch: async (req) => {
		const call = await record(req, seatCalls);
		if (call.path === "/api/v1/auth/person/session" && req.method === "POST") {
			if (seatSays.redeem !== "grant")
				return Response.json(
					{ code: "person_ticket_refused", error: "refused" },
					{ status: seatSays.redeem },
				);
			return Response.json({
				token: seatSays.token,
				expires_at: Math.floor(Date.now() / 1000) + 3600,
				profile: PROFILE,
			});
		}
		if (call.authorization !== `Bearer ${seatSays.token}`)
			return Response.json(
				{
					code: "person_session_ended",
					error: "Your Punktfunk session ended.",
				},
				{ status: 401 },
			);
		if (call.path === "/api/v1/host")
			return Response.json({ hostname: "Wohnzimmer" });
		if (call.path === `/api/v1/plugins/${SEAT_PLUGIN}/ui-credential`)
			return Response.json({ port: plugin.port, secret: PLUGIN_SECRET });
		if (call.path === "/api/v1/plugins/relayed/ui-credential")
			return Response.json({ port: 0, secret: "relay" });
		if (call.path === "/api/v1/auth/person/profile")
			return Response.json({
				id: PROFILE,
				display_name: "Anna",
				accent: "#336699",
				has_avatar: true,
			});
		if (call.path === "/api/v1/auth/person/profile/avatar")
			return call.headers["if-none-match"] === '"a1"'
				? new Response(null, { status: 304, headers: { etag: '"a1"' } })
				: new Response(new Uint8Array([137, 80, 78, 71]), {
						headers: { "content-type": "image/png", etag: '"a1"' },
					});
		return Response.json({ seat: call.path });
	},
});

/** The profile list the box answers in place of a door's, when a test sets one. */
export const boxSays: { profiles: unknown[] | null } = { profiles: null };

/** The box: its profile list names the seat above; every other call answers with its path. */
export const box = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	fetch: async (req) => {
		const call = await record(req, boxCalls);
		if (call.path === "/api/v1/profiles" && boxSays.profiles)
			return Response.json(boxSays.profiles);
		if (call.path === "/api/v1/profiles")
			return Response.json([
				{ id: OWNER, display_name: "Enrico", owner: true },
				{
					id: PROFILE,
					display_name: "Anna",
					owner: false,
					accent: "#336699",
					seat: {
						kind: "desktop",
						pin: PIN,
						mgmt_port: listed.port || seat.port,
					},
				},
				{
					id: OTHER,
					display_name: "Kid",
					owner: false,
					seat: { kind: "desktop", pin: PIN },
				},
			]);
		return Response.json({ box: call.path });
	},
});

/** A second seat host with the same certificate, for a test that stops it. */
export const spareSeat = () =>
	Bun.serve({
		port: 0,
		hostname: "127.0.0.1",
		tls: { cert, key },
		fetch: (req) =>
			req.method === "POST"
				? Response.json({
						token: seatSays.token,
						expires_at: Math.floor(Date.now() / 1000) + 3600,
						profile: PROFILE,
					})
				: Response.json({ hostname: "Wohnzimmer" }),
	});

// Shared by every test file that imports this one, so they outlive each file's tests.
seat.unref();
box.unref();

/** Every file under server/routes as Nitro mounts it: its path, method and handler. */
export function routeFiles(): {
	file: string;
	path: string;
	method?: string;
}[] {
	const dir = join(import.meta.dir, "../routes");
	return [...new Glob("**/*.ts").scanSync(dir)]
		.filter((f) => !f.endsWith(".test.ts"))
		.map((file) => {
			const parts = file.replace(/\.ts$/, "").split(".");
			const method = ["get", "post", "put", "patch", "delete"].includes(
				parts.at(-1) ?? "",
			)
				? parts.pop()
				: undefined;
			const path = `/${parts.join(".")}`
				.replace(/\/index$/, "")
				.replace(/\[\.\.\.(\w*)\]/g, "**")
				.replace(/\[(\w+)\]/g, ":$1");
			return { file, path, method };
		});
}

/** Which route files ran, by file name. */
export const ran: string[] = [];

/** The console: the gate, then every route file, each recording that it ran. */
export async function console_() {
	const router = createRouter();
	for (const { file, path, method } of routeFiles()) {
		const mod = await import(join(import.meta.dir, "../routes", file));
		const handler = mod.default as EventHandler;
		const spy: EventHandler = (event) => {
			ran.push(file);
			return handler(event);
		};
		router.add(path, spy, method as "get" | undefined);
	}
	return toWebHandler(createApp().use(gate).use(router));
}

// A stored hash, not the clear-text line: a clear-text login migrates the password file once
// per process, and auth.test.ts is the one test that may spend that. Cost 4 keeps logins fast.
const PW_HASH = Bun.password.hashSync("pw", { algorithm: "bcrypt", cost: 4 });
// What the console keeps on disk goes here: a sign-out writes the session epoch, and the
// default is the real config folder of whoever runs the tests.
const STATE = mkdtempSync(join(tmpdir(), "pf-person-"));

/** The env a door's console runs with, pointed at the fakes. Its password is `pw`. */
export function asDoor(): void {
	process.env.PUNKTFUNK_MGMT_URL = `http://127.0.0.1:${box.port}`;
	process.env.PUNKTFUNK_MGMT_TOKEN = ADMIN;
	process.env.PUNKTFUNK_UI_EPOCH_FILE = join(STATE, "epoch");
	process.env.PUNKTFUNK_UI_PASSWORD_HASH = PW_HASH;
	process.env.PUNKTFUNK_UI_SECRET = "s".repeat(40);
	delete process.env.PUNKTFUNK_UI_PASSWORD;
}

/** A fresh ticket for `profile` in the seat host's shape. */
export const ticket = (profile = PROFILE, n = Math.random()) =>
	`${Math.floor(Date.now() / 1000)}.${createHash("md5").update(String(n)).digest("hex")}.${profile}.${"c".repeat(64)}`;

/** The cookies a response set, as a `Cookie` header, merged over `jar`. */
export function jar(res: Response, prior = ""): string {
	const map = new Map(
		prior
			.split("; ")
			.filter(Boolean)
			.map((c) => c.split("=") as [string, string]),
	);
	for (const line of res.headers.getSetCookie()) {
		const [pair] = line.split(";");
		const [name, value] = (pair ?? "").split("=");
		if (!name) continue;
		if (!value || /max-age=0|expires=thu, 01 jan 1970/i.test(line))
			map.delete(name);
		else map.set(name, value);
	}
	return [...map].map(([k, v]) => `${k}=${v}`).join("; ");
}

/** A request from this computer. */
export const local = (
	path: string,
	init: RequestInit & { cookie?: string } = {},
) => {
	const headers = new Headers(init.headers);
	headers.set("x-pf-peer-ip", "127.0.0.1");
	if (init.cookie) headers.set("cookie", init.cookie);
	return new Request(`http://console${path}`, { ...init, headers });
};
