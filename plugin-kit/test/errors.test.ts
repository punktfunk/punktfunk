// What a kit error says when something interpolates it — which is the whole diagnosis surface a
// plugin operator gets, because `sync-engine`'s failure path logs only the pretty-printed cause.
import { describe, expect, test } from "bun:test";
import { HostRequestError, SyncError } from "../src/errors.js";

describe("HostRequestError", () => {
	// The default string form is the bare tag, which would hide the method, path and the host's own
	// explanation of a 400. Interpolation is the assertion because a log line renders it that way.
	test("names the call and carries the host's explanation", () => {
		const err = new HostRequestError({
			method: "PUT",
			path: "/library/provider/lutris?store=lutris",
			cause: new Error("art.portrait: local art must be an image file"),
		});

		expect(`${err}`).toContain("PUT");
		expect(`${err}`).toContain("/library/provider/lutris?store=lutris");
		expect(`${err}`).toContain("art.portrait");
		expect(`${err}`).not.toBe("HostRequestError");
	});

	// The host's rejection arrives as a parsed `{error: "…"}` body, not an Error. Left to default
	// stringification that is `[object Object]` — the useful half lost a second way.
	test("renders an object cause instead of [object Object]", () => {
		const err = new HostRequestError({
			method: "PUT",
			path: "/library/provider/steam",
			cause: { error: "art.header: local art must be an image file" },
		});

		expect(`${err}`).toContain("art.header");
		expect(`${err}`).not.toContain("[object Object]");
	});

	// Error formatting must never itself throw: a cycle (or a BigInt) would make JSON.stringify
	// blow up INSIDE the catch that is trying to report the original failure.
	test("survives a cause that cannot be serialized", () => {
		const cyclic: Record<string, unknown> = {};
		cyclic.self = cyclic;
		const err = new HostRequestError({
			method: "GET",
			path: "/library",
			cause: cyclic,
		});

		expect(() => `${err}`).not.toThrow();
		expect(`${err}`).toContain("/library");
	});

	// The tag stays matchable — `Effect.catchTag`/`_tag` narrowing must not be traded away for a
	// readable message.
	test("keeps its tag and its fields", () => {
		const err = new HostRequestError({
			method: "DELETE",
			path: "/library/provider/heroic",
			cause: "boom",
		});

		expect(err._tag).toBe("HostRequestError");
		expect(err.method).toBe("DELETE");
		expect(err.path).toBe("/library/provider/heroic");
	});
});

describe("SyncError", () => {
	// The default string form is the bare tag, so a plugin that renders `String(e)` into its API
	// error would show the operator the word "SyncError" and drop the host's nested explanation.
	// The chain must survive.
	test("carries the nested host explanation, not the bare tag", () => {
		const err = new SyncError({
			reason: "manual",
			cause: new HostRequestError({
				method: "PUT",
				path: "/library/provider/rom-manager",
				cause: {
					error:
						'`launch.kind = "command"` is executed as the host user and may only be set with the operator\'s admin token',
				},
			}),
		});

		expect(`${err}`).toContain("manual");
		expect(`${err}`).toContain("/library/provider/rom-manager");
		expect(`${err}`).toContain("launch.kind");
		expect(`${err}`).not.toBe("SyncError");
		expect(`${err}`).not.toContain("[object Object]");
	});

	test("keeps its tag and fields for catchTag narrowing", () => {
		const err = new SyncError({ reason: "startup", cause: "boom" });
		expect(err._tag).toBe("SyncError");
		expect(err.reason).toBe("startup");
	});
});
