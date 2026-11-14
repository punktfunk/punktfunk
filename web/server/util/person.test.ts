// A person's own console: a ticket opens it only through the seat host the box names, a session
// lives here and never in the browser, and an operator session in the same browser stays apart.
import { beforeEach, describe, expect, test } from "bun:test";
import { createApp, defineEventHandler, getQuery, toWebHandler } from "h3";
import {
	liveSessions,
	openSession,
	resetPersonState,
	ticketProfile,
} from "./person";
import {
	ADMIN,
	asDoor,
	boxCalls,
	boxSays,
	console_,
	jar,
	listed,
	local,
	OTHER,
	OWNER,
	PIN,
	PROFILE,
	ran,
	seat,
	seatCalls,
	seatSays,
	selfSigned,
	spareSeat,
	ticket,
} from "./person.harness";

asDoor();
const handle = await console_();

beforeEach(() => {
	asDoor();
	resetPersonState();
	seatCalls.length = 0;
	boxCalls.length = 0;
	ran.length = 0;
	seatSays.redeem = "grant";
	listed.port = 0;
	boxSays.profiles = null;
});

const send = (path: string, init: RequestInit & { cookie?: string } = {}) =>
	handle(local(path, init));

/** Opens a person session through the ticket page: the browser's cookies after it. */
async function signIn(prior = ""): Promise<string> {
	const res = await send(`/_auth/person?ticket=${ticket()}`, { cookie: prior });
	expect(res.status).toBe(302);
	expect(res.headers.get("location")).toBe("/");
	return jar(res, prior);
}

/** An operator session, through the login form. */
async function logIn(prior = ""): Promise<string> {
	const res = await send("/_auth/login", {
		method: "POST",
		cookie: prior,
		headers: { "content-type": "application/json" },
		body: JSON.stringify({ password: "pw" }),
	});
	expect(res.status).toBe(200);
	return jar(res, prior);
}

describe("the ticket page", () => {
	test("a good ticket opens a session whose token stays on the server", async () => {
		const cookies = await signIn();
		expect(cookies).toMatch(/pf_person=[0-9a-f]{64}/);
		expect(cookies).toContain("pf_mode=person");
		expect(cookies).not.toContain(seatSays.token);
		const redeemed = seatCalls.find(
			(c) => c.path === "/api/v1/auth/person/session",
		);
		expect(redeemed?.authorization).toBeNull();
		expect(JSON.parse(redeemed?.body ?? "{}").ticket).toMatch(
			/\.a1b2c3d4e5f6\./,
		);
	});

	const refusedWith = async (path: string, why = "refused", peer?: string) => {
		const req = local(path);
		if (peer) req.headers.set("x-pf-peer-ip", peer);
		const res = await handle(req);
		expect(res.status, path).toBe(302);
		expect(res.headers.get("location"), path).toBe(`/closed?why=${why}`);
		expect(jar(res), path).not.toContain("pf_person");
		expect(jar(res), path).not.toContain("pf_mode");
	};

	test("a bad, used, expired or wrong seat's ticket ends in the sentence and no session", async () => {
		await refusedWith("/_auth/person");
		await refusedWith("/_auth/person?ticket=garbage");
		const stale = `${Math.floor(Date.now() / 1000) - 120}.${"0".repeat(32)}.${PROFILE}.${"c".repeat(64)}`;
		await refusedWith(`/_auth/person?ticket=${stale}`);
		// Used: the console never relays a ticket twice.
		const once = ticket();
		await send(`/_auth/person?ticket=${once}`);
		const before = seatCalls.length;
		await refusedWith(`/_auth/person?ticket=${once}`);
		expect(seatCalls.length).toBe(before);
		// Wrong seat: the seat host refuses a ticket its person token did not sign.
		seatSays.redeem = 401;
		await refusedWith(`/_auth/person?ticket=${ticket()}`);
		// A profile with no seat port listed, and one the box does not list, open nothing.
		await refusedWith(`/_auth/person?ticket=${ticket(OTHER)}`);
		await refusedWith(`/_auth/person?ticket=${ticket("abcdefabcdef")}`);
		seatSays.redeem = 429;
		await refusedWith(`/_auth/person?ticket=${ticket()}`, "wait");
		// From another computer, even with a good ticket.
		seatSays.redeem = "grant";
		await refusedWith(
			`/_auth/person?ticket=${ticket()}`,
			"refused",
			"192.168.1.44",
		);
	});

	test("a Windows box's seat opens its person's console; a take-turns profile has none", async () => {
		// As a Windows box lists them: the owner plays on the console session, a seat on an
		// existing account names it, and a take-turns profile has no host of its own.
		boxSays.profiles = [
			{
				id: OWNER,
				display_name: "Enrico",
				owner: true,
				seat: { kind: "desktop", state: "ready", port: 9777 },
			},
			{
				id: PROFILE,
				display_name: "Anna",
				owner: false,
				seat: {
					kind: "desktop",
					state: "ready",
					port: 9778,
					pin: PIN,
					mgmt_port: seat.port,
					account: "anna",
				},
			},
			{
				id: OTHER,
				display_name: "Kid",
				owner: false,
				seat: { kind: "desktop", state: "ready", port: 9777, take_turns: true },
			},
		];
		const cookies = await signIn();
		const me = await send("/_auth/person/me", { cookie: cookies });
		const body = (await me.json()) as { own: boolean; owner: string | null };
		expect(body.own).toBe(true);
		expect(body.owner).toBe("Enrico");
		const before = seatCalls.length;
		await refusedWith(`/_auth/person?ticket=${ticket(OTHER)}`);
		await refusedWith(`/_auth/person?ticket=${ticket(OWNER)}`);
		expect(seatCalls.length).toBe(before);
	});

	test("the seat host comes from the box, never from the ticket or the request", async () => {
		let decoyHits = 0;
		const decoy = Bun.serve({
			port: 0,
			hostname: "127.0.0.1",
			fetch: () => {
				decoyHits++;
				return Response.json({});
			},
		});
		const t = ticket();
		const res = await send(
			`/_auth/person?ticket=${t}&port=${decoy.port}&host=127.0.0.1:${decoy.port}`,
			{
				headers: {
					host: `127.0.0.1:${decoy.port}`,
					"x-forwarded-host": `127.0.0.1:${decoy.port}`,
				},
			},
		);
		expect(res.status).toBe(302);
		expect(res.headers.get("location")).toBe("/");
		expect(decoyHits).toBe(0);
		expect(boxCalls.map((c) => c.path)).toContain("/api/v1/profiles");
		expect(
			seatCalls.some((c) => c.path === "/api/v1/auth/person/session"),
		).toBe(true);
		// The profile is the only part of the ticket the console reads.
		expect(
			ticketProfile(`1.${"0".repeat(32)}.${PROFILE}.${"c".repeat(64)}`, 1000),
		).toBe(PROFILE);
		expect(
			ticketProfile(
				`1.${"0".repeat(32)}.${PROFILE}:${decoy.port}.${"c".repeat(64)}`,
				1000,
			),
		).toBeNull();
		decoy.stop(true);
	});

	test("a seat host that presents another certificate never sees the ticket", async () => {
		const squatter = Bun.serve({
			port: 0,
			hostname: "127.0.0.1",
			tls: selfSigned("punktfunk test seat"),
			fetch: async (req) => {
				heard.push(await req.text());
				return Response.json({});
			},
		});
		const heard: string[] = [];
		listed.port = squatter.port;
		const res = await send(`/_auth/person?ticket=${ticket()}`);
		expect(res.headers.get("location")).toBe("/closed?why=refused");
		expect(heard).toEqual([]);
		squatter.stop(true);
	});
});

describe("a person's calls", () => {
	test("go to their seat host with their token, or to the box with the person headers", async () => {
		const cookies = await signIn();
		boxCalls.length = 0;
		seatCalls.length = 0;
		const hooks = await send("/api/v1/hooks?as=000000000000", {
			cookie: cookies,
		});
		expect(await hooks.json()).toEqual({ seat: "/api/v1/hooks" });
		const toSeat = seatCalls.at(-1);
		expect(toSeat?.authorization).toBe(`Bearer ${seatSays.token}`);
		expect(toSeat?.cookie).toBeNull();
		expect(toSeat?.search).toBe("");

		const page = await send("/api/v1/library/page?limit=5&as=000000000000", {
			cookie: cookies,
		});
		expect(await page.json()).toEqual({ box: "/api/v1/library/page" });
		const toBox = boxCalls.at(-1);
		expect(toBox?.authorization).toBeNull();
		expect(toBox?.cookie).toBeNull();
		expect(toBox?.headers["x-punktfunk-person-session"]).toBe(seatSays.token);
		expect(toBox?.headers["x-punktfunk-person-profile"]).toBe(PROFILE);
		expect(new URLSearchParams(toBox?.search).getAll("as")).toEqual([PROFILE]);
		expect(new URLSearchParams(toBox?.search).get("limit")).toBe("5");

		// An own title's cover is on the person's desktop, every other the box's.
		await send("/api/v1/library/art/own:0123456789ab/portrait", {
			cookie: cookies,
		});
		expect(decodeURIComponent(seatCalls.at(-1)?.path ?? "")).toBe(
			"/api/v1/library/art/own:0123456789ab/portrait",
		);
		await send("/api/v1/library/art/steam:570/portrait", { cookie: cookies });
		expect(decodeURIComponent(boxCalls.at(-1)?.path ?? "")).toBe(
			"/api/v1/library/art/steam:570/portrait",
		);

		// Their emulators and plugins are their seat host's; a source's stream settings the box's.
		const adopt = JSON.stringify({ exe: "/home/anna/rpcs3", password: "x" });
		await send("/api/v1/emulators/rpcs3/adopt", {
			method: "POST",
			cookie: cookies,
			headers: { "content-type": "application/json" },
			body: adopt,
		});
		expect(seatCalls.at(-1)).toMatchObject({
			path: "/api/v1/emulators/rpcs3/adopt",
			authorization: `Bearer ${seatSays.token}`,
			body: adopt,
		});
		await send("/api/v1/store/runtime", { cookie: cookies });
		expect(seatCalls.at(-1)?.path).toBe("/api/v1/store/runtime");
		// Folders for their own plugins: their seat host decides, and no password is asked.
		const allow = JSON.stringify({
			path: "/home/anna/Games",
			decision: "allow",
		});
		await send("/api/v1/plugin-access/demo/decide", {
			method: "POST",
			cookie: cookies,
			headers: { "content-type": "application/json" },
			body: allow,
		});
		expect(seatCalls.at(-1)).toMatchObject({
			path: "/api/v1/plugin-access/demo/decide",
			authorization: `Bearer ${seatSays.token}`,
			body: allow,
		});
		await send("/api/v1/plugin-access", { cookie: cookies });
		expect(seatCalls.at(-1)?.path).toBe("/api/v1/plugin-access");
		await send("/api/v1/library/overrides/sources/steam", {
			method: "PUT",
			cookie: cookies,
			headers: { "content-type": "application/json" },
			body: "{}",
		});
		expect(boxCalls.at(-1)).toMatchObject({
			method: "PUT",
			path: "/api/v1/library/overrides/sources/steam",
			search: `?as=${PROFILE}`,
			authorization: null,
		});
		expect(ran.filter((f) => f.startsWith("api/"))).toEqual([]);
	});

	test("anything off the table is refused here, however it is spelled", async () => {
		const cookies = await signIn();
		boxCalls.length = 0;
		seatCalls.length = 0;
		for (const [method, path] of [
			["GET", "/api/v1/profiles"],
			["GET", "/api/v1/events"],
			["GET", "/api/v1/plugins/x/ui-credential"],
			["PUT", "/api/v1/store/sources/x"],
			["PATCH", "/api/v1/host/settings"],
			["POST", "/api/v1/auth/person/verify"],
			["GET", "/api/v1/profiles/a1b2c3d4e5f6/proxy/hooks"],
			["GET", "/api/v1/hooks/../profiles"],
			["GET", "/api/v1/library/custom/a%2F..%2F..%2Fprofiles"],
			["GET", "/api//v1/./native/clients"],
			["DELETE", "/api/v1/hooks"],
		] as const) {
			const res = await send(path, { method, cookie: cookies });
			expect(res.status, `${method} ${path}`).toBe(403);
		}
		expect([...boxCalls, ...seatCalls]).toEqual([]);
	});

	test("a session the seat host ended, or a stopped seat host, ends here with its reason", async () => {
		const cookies = await signIn();
		seatSays.token = "6".repeat(64);
		const res = await send("/api/v1/hooks", { cookie: cookies });
		expect(res.status).toBe(401);
		expect(((await res.json()) as { code: string }).code).toBe(
			"person_session_ended",
		);
		expect(jar(res, cookies)).not.toContain("pf_person");
		seatSays.token = "5".repeat(64);

		const spare = spareSeat();
		listed.port = spare.port;
		const other = await signIn();
		spare.stop(true);
		const gone = await send("/api/v1/hooks", { cookie: other });
		expect(gone.status).toBe(401);
		expect(((await gone.json()) as { code: string }).code).toBe(
			"person_seat_stopped",
		);
	});

	test("an ended session's pages say so, and never show the operator's login", async () => {
		const res = await send("/library", {
			cookie: `pf_person=${"9".repeat(64)}`,
		});
		expect(res.status).toBe(302);
		expect(res.headers.get("location")).toBe("/closed?why=ended");
		const api = await send("/api/v1/library", {
			cookie: `pf_person=${"9".repeat(64)}`,
		});
		expect(api.status).toBe(401);
		expect(((await api.json()) as { code: string }).code).toBe(
			"person_session_ended",
		);
	});

	test("a session from another computer is not a session", async () => {
		const cookies = await signIn();
		const req = local("/api/v1/hooks", { cookie: cookies });
		req.headers.set("x-pf-peer-ip", "192.168.1.44");
		const res = await handle(req);
		expect(res.status).toBe(401);
		expect(seatCalls.filter((c) => c.path === "/api/v1/hooks")).toEqual([]);
	});

	test("me and the picture come from their seat host with their session, never the box", async () => {
		const cookies = await signIn();
		boxCalls.length = 0;
		const res = await send("/_auth/person/me", { cookie: cookies });
		expect(await res.json()).toMatchObject({
			profile: PROFILE,
			name: "Anna",
			owner: "Enrico",
			accent: "#336699",
			own: false,
			host: "Wohnzimmer",
			avatar: "/_auth/person/picture",
		});
		const picture = await send("/_auth/person/picture", { cookie: cookies });
		expect(picture.headers.get("content-type")).toBe("image/png");
		const again = await send("/_auth/person/picture", {
			cookie: cookies,
			headers: { "if-none-match": '"a1"' },
		});
		expect(again.status).toBe(304);
		expect(boxCalls).toEqual([]);
		const reads = seatCalls.filter((c) =>
			c.path.startsWith("/api/v1/auth/person/profile"),
		);
		expect(reads.map((c) => c.authorization)).toEqual(
			reads.map(() => `Bearer ${seatSays.token}`),
		);
	});
});

describe("two kinds of session in one browser", () => {
	test("a person's cookie decides its requests: the operator's is never read for them", async () => {
		const both = await signIn(await logIn());
		expect(both).toContain("pf_session=");
		boxCalls.length = 0;
		const res = await send("/api/v1/profiles", { cookie: both });
		expect(res.status).toBe(403);
		expect(boxCalls).toEqual([]);
		expect(ran.filter((f) => f.startsWith("api/"))).toEqual([]);
	});

	test("the person signing out leaves the operator signed in", async () => {
		const both = await signIn(await logIn());
		const out = await send("/_auth/person/end", {
			method: "POST",
			cookie: both,
		});
		const after = jar(out, both);
		expect(after).not.toContain("pf_person");
		expect(after).not.toContain("pf_mode");
		expect(after).toContain("pf_session=");
		expect(seatCalls.at(-1)).toMatchObject({
			method: "DELETE",
			path: "/api/v1/auth/person/session",
			authorization: `Bearer ${seatSays.token}`,
		});
		boxCalls.length = 0;
		const res = await send("/api/v1/profiles", { cookie: after });
		expect(res.status).toBe(200);
		expect(boxCalls.at(-1)?.authorization).toBe(`Bearer ${ADMIN}`);
	});

	test("the operator signing out leaves the person's console", async () => {
		const both = await signIn(await logIn());
		const out = await send("/_auth/logout", { method: "POST", cookie: both });
		const after = jar(out, both);
		expect(after).toContain("pf_person=");
		const res = await send("/api/v1/hooks", { cookie: after });
		expect(res.status).toBe(200);
	});

	test("the operator signing in ends a person's console rather than becoming it", async () => {
		const person = await signIn();
		const operator = await logIn(person);
		expect(operator).not.toContain("pf_person");
		expect(operator).not.toContain("pf_mode");
		boxCalls.length = 0;
		await send("/api/v1/profiles", { cookie: operator });
		expect(boxCalls.at(-1)?.authorization).toBe(`Bearer ${ADMIN}`);
	});

	test("a person's ended cookie gives way to an operator signed in on the same browser", async () => {
		const operator = await logIn();
		const stale = `${operator}; pf_person=${"9".repeat(64)}; pf_mode=person`;
		const res = await send("/api/v1/profiles", { cookie: stale });
		expect(res.status).toBe(200);
		expect(jar(res, stale)).not.toContain("pf_person");
	});
});

describe("sessions held here", () => {
	const opener = toWebHandler(
		createApp().use(
			"/open",
			defineEventHandler((event) => {
				const { profile, n } = getQuery(event);
				openSession(event, {
					profile: String(profile),
					owner: null,
					own: true,
					seat: { port: seat.port, pin: PIN },
					token: String(n),
					expiresAt: Date.now() + 60_000,
				});
				return "";
			}),
		),
	);
	const open = (profile: string, n: number) =>
		opener(local(`/open?profile=${profile}&n=${n}`));
	const tokens = (profile: string) =>
		liveSessions()
			.filter((s) => s.profile === profile)
			.map((s) => s.token);

	test("one profile's opens end only its own oldest sessions", async () => {
		await open(OTHER, 0);
		for (let n = 1; n <= 40; n++) await open(PROFILE, n);
		expect(tokens(OTHER)).toEqual(["0"]);
		expect(tokens(PROFILE)).toEqual(
			Array.from({ length: 16 }, (_, i) => String(25 + i)),
		);
	});
});
