// The console's admin bearer never rides on a person's session. Every route file is called as the
// person, with every method it serves, and neither fake host may see the admin token. No `/api/**`
// or plugin page file runs for them at all: the gate answers first. The plugin health probe runs,
// and asks their seat host alone.
import { beforeAll, beforeEach, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
	ADMIN,
	asDoor,
	boxCalls,
	console_,
	jar,
	local,
	ran,
	routeFiles,
	seatCalls,
	ticket,
} from "./person.harness";

asDoor();
const handle = await console_();
let cookies = "";
/** A live person session: the operator's sign-in, one of the files walked, ends the last one. */
async function person(): Promise<void> {
	asDoor();
	const res = await handle(local(`/_auth/person?ticket=${ticket()}`));
	cookies = jar(res);
	expect(cookies).toContain("pf_person=");
}
beforeAll(person);
beforeEach(asDoor);

const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE"];
/** A request path for a route file: each parameter a sample, a catch-all two segments. */
const sample = (path: string) =>
	path.replace(/:\w+/g, "a1b2c3d4e5f6").replace(/\*\*/g, "v1/hooks");

test("no route file sends the admin bearer for a person, on any method", async () => {
	const files = routeFiles();
	expect(files.length).toBeGreaterThan(40);
	for (const { file, path, method } of files) {
		for (const verb of method ? [method.toUpperCase()] : METHODS) {
			await person();
			boxCalls.length = 0;
			seatCalls.length = 0;
			ran.length = 0;
			const body =
				verb === "GET" ? undefined : JSON.stringify({ password: "pw" });
			await handle(
				local(sample(path), {
					method: verb,
					cookie: cookies,
					headers: { "content-type": "application/json" },
					body,
				}),
			);
			const seen = [...boxCalls, ...seatCalls];
			const admin = seen.filter(
				(c) =>
					c.authorization === `Bearer ${ADMIN}` ||
					JSON.stringify(c).includes(ADMIN),
			);
			expect(admin.map((c) => `${file}: ${c.method} ${c.path}`)).toEqual([]);
			if (/^(api|plugin-ui)\//.test(file))
				expect(ran, `${verb} ${file}`).toEqual([]);
			if (/^_plugin-health\//.test(file))
				expect(boxCalls, `${verb} ${file}`).toEqual([]);
		}
	}
});

test("the event stream and the seat proxy, spelled any way, never reach a route file", async () => {
	await person();
	for (const path of [
		"/api/v1/events",
		"/api/v1/profiles/a1b2c3d4e5f6/proxy/events",
		"/api/v1/profiles/a1b2c3d4e5f6/proxy/hooks",
		"/api/v1/session/1/pads",
		"/api/v1/library/art/steam:570/portrait/../../../../profiles",
		"/api/v1/plugins/x/ui-credential",
	]) {
		ran.length = 0;
		boxCalls.length = 0;
		const res = await handle(local(path, { cookie: cookies }));
		expect(res.status, path).toBe(403);
		expect(ran, path).toEqual([]);
		expect(boxCalls, path).toEqual([]);
	}
});

test("the files a person's calls run through hold no admin credential", () => {
	const admin =
		/\b(mgmtFetch|mgmtBearer|mgmtToken|relayToMgmt|proxySse|forwardJson)\b|from "\.\/forward"|sseProxy/;
	for (const file of [
		"util/person.ts",
		"util/personApi.ts",
		"util/personRoutes.ts",
		"util/personPlugins.ts",
		"routes/_auth/person/me.get.ts",
		"routes/_auth/person/end.post.ts",
		"routes/_auth/person/picture.get.ts",
	]) {
		const text = readFileSync(join(import.meta.dir, "..", file), "utf8");
		expect(text.match(admin)?.[0], file).toBeUndefined();
	}
	// Sign-in alone reads the box with it, once, for the seat host, before the person's cookie
	// exists. Their name and picture come from their seat host with their own session.
	const signIn = readFileSync(
		join(import.meta.dir, "../routes/_auth/person/index.get.ts"),
		"utf8",
	);
	expect(signIn).toContain('from "../../../util/personLookup"');
	const lookup = readFileSync(join(import.meta.dir, "personLookup.ts"), "utf8");
	const reads = lookup.match(/mgmtFetch\([^,]+/g) ?? [];
	expect(reads).toEqual(['mgmtFetch("/api/v1/profiles"']);
	// The route itself never holds the bearer: only the lookup does.
	const signInImports = [...signIn.matchAll(/from "([^"]+)"/g)].map(
		(m) => m[1],
	);
	expect(signInImports).not.toContain("../../../util/forward");
});
