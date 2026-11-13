// The person's table against the API document: their seat host's rows are profile scoped or one
// of its pinned host reads, their box rows are the reads and writes the box takes from a person
// session, nothing `box` is reachable, and a route nobody listed is closed.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { GATES } from "./gates";
import { PERSON_ROUTES, personLane } from "./personRoutes";

type Op = { "x-scope"?: string };
const spec: { paths: Record<string, Record<string, Op>> } = JSON.parse(
	readFileSync(join(import.meta.dir, "../../../api/openapi.json"), "utf8"),
);
const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
	Object.entries(item).map(([method, op]) => ({
		method: method.toUpperCase(),
		pattern: path.replace(/^\/api\/v1\//, ""),
		scope: op["x-scope"],
	})),
);
const key = (r: { method: string; pattern: string }) =>
	`${r.method} ${r.pattern}`;
const concrete = (pattern: string) => pattern.replace(/\{\w+\}/g, "sample");

/** `SEAT_READS` in `mgmt/person_session.rs`: the host rows a seat host lets the person lane read. */
const HOST_READS = [
	"GET host",
	"GET status",
	"GET auth/person/profile",
	"GET auth/person/profile/avatar",
];
/** `BOX_READS` and `mgmt_routes::PERSON`: what the box takes from a person session. */
const BOX_REACH = [
	"GET library",
	"GET library/page",
	"GET library/art/{id}/{kind}",
	"GET library/scanners",
	"GET library/overrides",
	"GET session/settings",
	"PUT library/hidden/{id}",
	"PUT library/overrides/games/{id}",
	"PUT library/overrides/sources/{source}",
	"PUT library/scanners/{id}",
	"PUT session/settings",
	"DELETE session/settings",
];

test("every row is an operation of the API document, once", () => {
	const ops = new Set(operations.map(key));
	const rows = PERSON_ROUTES.map(key);
	expect(rows.filter((r) => !ops.has(r))).toEqual([]);
	expect(rows.filter((r, i) => rows.indexOf(r) !== i)).toEqual([]);
});

test("a seat row is the person's own data or execution, or a pinned host read", () => {
	const scope = new Map(operations.map((o) => [key(o), o.scope]));
	const off = PERSON_ROUTES.filter((r) => r.lane === "seat").filter((r) => {
		const s = scope.get(key(r));
		return (
			s !== "profile-data" &&
			s !== "profile-exec" &&
			!HOST_READS.includes(key(r))
		);
	});
	expect(off.map(key)).toEqual([]);
});

test("a view row is one the box takes from a person session", () => {
	const off = PERSON_ROUTES.filter(
		(r) => r.lane === "view" && !BOX_REACH.includes(key(r)),
	);
	expect(off.map(key)).toEqual([]);
});

test("nothing `box` is reachable, and a route nobody listed is closed", () => {
	const listed = new Set(PERSON_ROUTES.map(key));
	for (const op of operations) {
		const lane = personLane(op.method, concrete(op.pattern));
		if (listed.has(key(op))) expect(lane, key(op)).not.toBeNull();
		else expect(lane, key(op)).toBeNull();
		if (op.scope === "box") expect(lane, key(op)).toBeNull();
	}
	expect(personLane("GET", "library/custom")).toBeNull();
	expect(personLane("GET", "library/custom/a/b")).toBeNull();
	expect(personLane("PUT", "hooks/x")).toBeNull();
});

test("the operator's password gates: a person makes a gated call only on their own lane", () => {
	const person = new Set(PERSON_ROUTES.map(key));
	const writes = PERSON_ROUTES.filter((r) => r.method !== "GET");
	for (const w of writes) {
		const gate = GATES.find((g) => key(g) === key(w));
		expect(gate, key(w)).toBeDefined();
		expect(gate?.refused, key(w)).toBeUndefined();
		// Open for the operator, or gated with the reason a person needs no password.
		expect(gate?.open ?? gate?.person, key(w)).toBeTruthy();
		if (!gate?.open) expect(w.lane, key(w)).toBe("seat");
	}
	// The reason is never on a row the person does not reach, and never loosens the operator's.
	for (const g of GATES.filter((g) => g.person)) {
		expect(person.has(key(g)), key(g)).toBe(true);
		expect(g.open, key(g)).toBeUndefined();
	}
});
