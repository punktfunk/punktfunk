import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Glob } from "bun";
import {
	encodeRest,
	GATED_ROUTES,
	gatedRoute,
	HOST_REFUSED,
	isUiCredential,
	seatCall,
	seatPath,
} from "./seatProxy";

const ID = "0a1b2c3d4e5f";
const base = `/api/v1/profiles/${ID}/proxy`;

describe("seatCall", () => {
	test("names the profile and the path on its host", () => {
		expect(seatCall(`${base}/library/page`)).toEqual({
			id: ID,
			rest: "library/page",
		});
	});

	test("resolves dots and escapes, so the gate reads what the host reads", () => {
		expect(seatCall(`${base}/library/./custom`)?.rest).toBe("library/custom");
		expect(seatCall(`${base}/x/../store/install`)?.rest).toBe("store/install");
		expect(seatCall(`${base}/store%2Finstall`)?.rest).toBe("store/install");
		expect(seatCall(`${base}/%2e%2e/store/install`)).toBeNull();
	});

	test("refuses a path that is not a seat's", () => {
		expect(seatCall(`/api/v1/profiles/${ID}/proxy`)).toBeNull();
		expect(seatCall(`/api/v1/profiles/${ID}/proxy/`)).toBeNull();
		expect(seatCall(`/api/v1/profiles/bad.id/proxy/library`)).toBeNull();
		expect(seatCall(`${base}/../../library`)).toBeNull();
	});

	test("forwards the resolved path, each segment escaped once", () => {
		const call = seatCall(`${base}/store/sources/my%20src`);
		expect(call && seatPath(call.id, encodeRest(call.rest))).toBe(
			`${base}/store/sources/my%20src`,
		);
	});
});

test("a plugin UI's credential is refused whichever way it is spelled", () => {
	expect(isUiCredential("plugins/steam/ui-credential")).toBe(true);
	expect(
		isUiCredential(
			seatCall(`${base}/plugins/a%2Fb/../steam/ui-credential`)?.rest ?? "",
		),
	).toBe(true);
	expect(isUiCredential("plugins/steam")).toBe(false);
});

describe("gatedRoute", () => {
	test("names the box's gate for each call it guards", () => {
		expect(gatedRoute("PUT", "hooks")?.route).toBe("hooks.put");
		expect(gatedRoute("POST", "store/install")?.route).toBe(
			"store/install.post",
		);
		expect(gatedRoute("PUT", "store/sources/mine")).toEqual({
			route: "store/sources/[name].put",
			params: { name: "mine" },
		});
		expect(gatedRoute("POST", "library/custom")?.route).toBe(
			"library/custom.post",
		);
		expect(gatedRoute("put", "library/custom/7")).toEqual({
			route: "library/custom/[id].put",
			params: { id: "7" },
		});
		expect(gatedRoute("PUT", "library/provider/romm")).toEqual({
			route: "library/provider/[provider].put",
			params: { provider: "romm" },
		});
	});

	test("leaves the reads and the ungated writes to the relay", () => {
		expect(gatedRoute("GET", "hooks")).toBeNull();
		expect(gatedRoute("DELETE", "store/sources/mine")).toBeNull();
		expect(gatedRoute("DELETE", "library/custom/7")).toBeNull();
		expect(gatedRoute("POST", "library/page")).toBeNull();
		expect(gatedRoute("PUT", "library/custom/7/extra")).toBeNull();
	});
});

test("every password-gated route is gated through a seat too, or the seat's host refuses it", () => {
	const root = join(import.meta.dir, "../routes/api/v1");
	const gated = [...new Glob("**/*.ts").scanSync(root)]
		.filter((file) =>
			/from "(\.\.\/)+util\/(confirm|libraryConfirm)"/.test(
				readFileSync(join(root, file), "utf8"),
			),
		)
		.map((file) => file.replace(/\.ts$/, ""));
	expect(gated).toEqual(expect.arrayContaining([...GATED_ROUTES]));
	const unguarded = gated.filter(
		(route) =>
			!(GATED_ROUTES as readonly string[]).includes(route) &&
			!HOST_REFUSED.includes(route.split("/")[0] ?? ""),
	);
	expect(unguarded).toEqual([]);
});
